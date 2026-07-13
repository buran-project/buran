//! Worker-side BWP loop, kernel-arbitrated work queue edition.
//!
//! Requests arrive on a *shared* SOCK_DGRAM socket inherited by every worker
//! of the application: the kernel wakes exactly one idle worker per
//! datagram — worker self-service without a router round-trip. Responses
//! leave on the worker's own stream channel, batched into a single write
//! per request.
//!
//! This SDK implements the blocking profile of the protocol: concurrency 1,
//! one request at a time, no body streaming. Event-loop runtimes implement
//! BWP natively (see the concurrency contract in `buran-ipc`); the
//! `buran-echo` module is the reference for that profile.
//!
//! Strictly single-threaded and blocking by design: the prototype forks
//! workers, and fork is only safe while no other threads exist.
//!
//! Crash semantics: datagrams not yet consumed survive a worker death and
//! are served by the remaining workers; the one being processed fails alone.

use std::io::{Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};

use buran_ipc::{
    BwpError, FrameHeader, FrameKind, Hello, HelloAck, RequestView, BWP_VERSION, FRAME_HEADER_LEN,
    PONG_BUSY, PONG_IDLE,
};
use rustix::event::{PollFd, PollFlags};
use thiserror::Error;

/// Largest datagram a worker accepts (must cover header + inline payload).
pub const MAX_DGRAM: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("protocol: {0}")]
    Bwp(#[from] BwpError),
    #[error("router closed the channel")]
    Closed,
    /// The client for this request went away (router sent Abort, or the
    /// stream broke). Surfaced to the runtime as a short/failed write.
    #[error("client gone")]
    ClientGone,
}

/// Module description printed by `--describe` (JSON on stdout).
pub struct Describe {
    pub runtime: &'static str,
    pub version: String,
    /// Extensions that are executable sources of this runtime: the router
    /// refuses to serve them as static files (source-leak protection).
    pub source_extensions: &'static [&'static str],
}

impl Describe {
    pub fn print(&self) {
        println!(
            "{}",
            serde_json::json!({
                "bwp": BWP_VERSION,
                "runtime": self.runtime,
                "version": self.version,
                "source_extensions": self.source_extensions,
            })
        );
    }
}

/// Per-request response channel handed to the module handler.
pub struct Responder<'a> {
    stream: &'a mut UnixStream,
    /// Read half of the private stream, non-blocking. During a request the
    /// only inbound frame is `Abort` (blocking workers read nothing else),
    /// so any readable bytes here mean the client is gone.
    abort: &'a mut UnixStream,
    out: &'a mut Vec<u8>,
    request_id: u32,
    headers_sent: bool,
    /// `End` sent: the client response is complete (client released).
    end_sent: bool,
    /// Terminal sent (`Done` or `Error`): the task is over, slot freed.
    done_sent: bool,
    aborted: bool,
}

fn push_frame(out: &mut Vec<u8>, header: &FrameHeader, payload: &[u8]) {
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
}

impl Responder<'_> {
    /// `headers` is a pre-serialized block: `name: value\r\n` pairs.
    pub fn send_headers(&mut self, status: u16, headers: &[u8]) -> Result<(), WorkerError> {
        debug_assert!(!self.headers_sent);
        let mut fh = FrameHeader::new(FrameKind::ResponseHeaders, self.request_id, headers.len() as u32);
        fh.aux = status as u32;
        push_frame(self.out, &fh, headers);
        self.headers_sent = true;
        Ok(())
    }

    pub fn send_body(&mut self, chunk: &[u8]) -> Result<(), WorkerError> {
        debug_assert!(self.headers_sent && !self.end_sent);
        // Surface a pending disconnect before buffering more output; the
        // runtime translates the error into its own abort (PHP user-abort).
        if self.poll_control() {
            return Err(WorkerError::ClientGone);
        }
        let fh = FrameHeader::new(FrameKind::ResponseBody, self.request_id, chunk.len() as u32);
        push_frame(self.out, &fh, chunk);
        // Large accumulations drain early: bounded worker memory and the
        // client sees bytes flowing.
        if self.out.len() >= 256 * 1024 {
            self.drain()?;
        }
        Ok(())
    }

    /// Explicit flush (PHP `flush()` / SSE): push a Flush marker so the router
    /// forwards buffered output now and streams the rest, then drain to the
    /// wire. Must follow `send_headers`. Errors surface a client disconnect.
    pub fn flush(&mut self) -> Result<(), WorkerError> {
        debug_assert!(self.headers_sent && !self.end_sent);
        if self.poll_control() {
            return Err(WorkerError::ClientGone);
        }
        push_frame(self.out, &FrameHeader::new(FrameKind::Flush, self.request_id, 0), &[]);
        self.drain()
    }

    /// Drain any pending router->worker control frames without blocking, and
    /// report whether this request's client is gone. On this channel only
    /// `Abort` (client gone) and `Ping` (liveness) arrive; a `Ping` is answered
    /// with `Pong: busy`. Called at output points (the runtime turns a `true`
    /// into its own abort, e.g. PHP user-abort).
    fn poll_control(&mut self) -> bool {
        if self.aborted {
            return true;
        }
        loop {
            let mut head = [0u8; FRAME_HEADER_LEN];
            // Per-call non-blocking recv: does not touch the shared fd status
            // flags that the (dup'd) write half relies on.
            let n =
                match rustix::net::recv(&*self.abort, &mut head, rustix::net::RecvFlags::DONTWAIT) {
                    Ok((0, _)) => {
                        self.aborted = true; // router closed the stream
                        return true;
                    }
                    Ok((n, _)) => n,
                    Err(rustix::io::Errno::AGAIN) => return self.aborted, // nothing more
                    Err(_) => {
                        self.aborted = true;
                        return true;
                    }
                };
            // Finish a split header in blocking mode (the router writes the
            // 16-byte frame atomically, but be robust) so the stream is not
            // left mid-frame.
            if n < FRAME_HEADER_LEN && self.abort.read_exact(&mut head[n..]).is_err() {
                self.aborted = true;
                return true;
            }
            match FrameHeader::decode(&head) {
                // Client gone for THIS request (a stale abort for a finished
                // request is consumed and ignored).
                Ok(h) if h.kind == FrameKind::Abort && h.request_id == self.request_id => {
                    self.aborted = true;
                    return true;
                }
                // Liveness probe: we are alive and busy on the current request.
                Ok(h) if h.kind == FrameKind::Ping => {
                    let mut pong = FrameHeader::new(FrameKind::Pong, h.request_id, 0);
                    pong.aux = PONG_BUSY;
                    let _ = self.stream.write_all(&pong.encode());
                }
                _ => {} // stale abort for another id, or unexpected: keep draining
            }
        }
    }

    /// Mark the client response complete (`End`): release the client and stop
    /// `response_timeout`. The task may keep running (background) until `Done`.
    fn send_end(&mut self) {
        if !self.end_sent {
            push_frame(self.out, &FrameHeader::new(FrameKind::End, self.request_id, 0), &[]);
            self.end_sent = true;
        }
    }

    /// Complete the task: send `End` (if not already) then `Done` — the task
    /// is fully finished (including background), the slot is freed and
    /// `task_timeout` stops. Called by the loop when the handler returns.
    pub fn finish(&mut self) -> Result<(), WorkerError> {
        if self.done_sent {
            return Ok(());
        }
        self.send_end();
        push_frame(self.out, &FrameHeader::new(FrameKind::Done, self.request_id, 0), &[]);
        self.done_sent = true;
        Ok(())
    }

    /// `fastcgi_finish_request`: release the client now (send `End` and drain)
    /// while the task keeps running in the background. `Done` follows when the
    /// handler returns (via `finish`).
    pub fn finish_now(&mut self) -> Result<(), WorkerError> {
        self.send_end();
        self.drain()
    }

    pub fn error(&mut self, message: &str) -> Result<(), WorkerError> {
        let fh = FrameHeader::new(FrameKind::Error, self.request_id, message.len() as u32);
        push_frame(self.out, &fh, message.as_bytes());
        // Error is terminal: it frees the slot on its own (no separate Done).
        self.end_sent = true;
        self.done_sent = true;
        Ok(())
    }

    /// Write the buffered frames to the wire. Private: callers choose the
    /// semantic entry point (`flush` for a streaming flush, `finish_now` for
    /// early release, or the size-based drain inside `send_body`).
    fn drain(&mut self) -> Result<(), WorkerError> {
        if !self.out.is_empty() {
            self.stream.write_all(self.out)?;
            self.out.clear();
        }
        Ok(())
    }
}

/// Run the worker loop: handshake on the response stream, then requests off
/// the shared work socket. `handler` gets the request view plus the frame
/// flags (FLAG_BODY_FILE et al.).
///
/// `max_requests` > 0 recycles the worker (FPM `pm.max_requests` semantics):
/// after the N-th response the worker exits; the router notices the stream
/// EOF, unconsumed datagrams stay queued for the other workers.
pub fn run<F>(
    work: UnixDatagram,
    resp: UnixStream,
    max_requests: u64,
    token: u64,
    mut handler: F,
) -> Result<(), WorkerError>
where
    F: FnMut(&RequestView<'_>, u8, &mut Responder<'_>) -> Result<(), WorkerError>,
{
    let mut served: u64 = 0;
    let mut write_stream = resp.try_clone()?;
    // The read half is also the control channel (idle Pings, in-flight Abort).
    // Read it byte-exact, never buffered ahead: a control frame arriving right
    // after HelloAck must not be swallowed by read-ahead.
    let mut abort = resp;
    let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut dgram = vec![0u8; MAX_DGRAM];

    // Handshake on the private stream: Hello -> HelloAck. The blocking SDK
    // declares concurrency 1 and no capabilities; the granted value in the
    // ack can only be 1, so it is not consulted further.
    let hello = Hello {
        version: BWP_VERSION,
        pid: std::process::id(),
        concurrency: 1,
        capabilities: 0,
        token,
    }
    .encode();
    let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + hello.len());
    push_frame(&mut msg, &FrameHeader::new(FrameKind::Hello, 0, hello.len() as u32), &hello);
    write_stream.write_all(&msg)?;

    let ack = read_hello_ack(&mut abort)?;
    if ack.version != BWP_VERSION {
        return Err(BwpError::UnsupportedVersion(ack.version).into());
    }

    // Requests arrive on the shared work socket; wait on it AND the control

    // Requests arrive on the shared work socket; wait on it AND the control
    // stream at once so an idle worker can answer a Ping. Non-blocking recv:
    // poll may wake several idle workers but only one wins the datagram.
    work.set_nonblocking(true)?;

    loop {
        // Idle wait: block until a request arrives or a control frame does.
        let n = loop {
            let (work_ready, ctrl_ready) = {
                let mut fds =
                    [PollFd::new(&work, PollFlags::IN), PollFd::new(&abort, PollFlags::IN)];
                rustix::event::poll(&mut fds, None).map_err(std::io::Error::from)?;
                (
                    fds[0].revents().contains(PollFlags::IN),
                    fds[1].revents().intersects(PollFlags::IN | PollFlags::HUP),
                )
            };
            if ctrl_ready {
                answer_idle_control(&mut abort, &mut write_stream)?;
            }
            if work_ready {
                match work.recv(&mut dgram) {
                    Ok(n) => break n,
                    // Another worker grabbed the datagram: back to waiting.
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                    Err(e) => return Err(e.into()),
                }
            }
        };
        if n < FRAME_HEADER_LEN {
            continue;
        }
        let header = FrameHeader::decode(dgram[..FRAME_HEADER_LEN].try_into().expect("len checked"))?;
        match header.kind {
            FrameKind::Request => {}
            FrameKind::Retire => return Ok(()), // dynamic shrink
            _ => continue,
        }
        let payload_end = FRAME_HEADER_LEN + header.payload_len as usize;
        if payload_end > n {
            continue; // truncated datagram: drop, router times it out
        }
        let payload = &dgram[FRAME_HEADER_LEN..payload_end];

        // Claim: lets the router map request -> worker (timing, stuck-tracking).
        write_stream
            .write_all(&FrameHeader::new(FrameKind::Claim, header.request_id, 0).encode())?;

        let view = RequestView::parse(payload)?;
        out.clear();
        let mut responder = Responder {
            stream: &mut write_stream,
            abort: &mut abort,
            out: &mut out,
            request_id: header.request_id,
            headers_sent: false,
            end_sent: false,
            done_sent: false,
            aborted: false,
        };

        match handler(&view, header.flags, &mut responder) {
            Ok(()) => responder.finish()?,
            Err(_) if !responder.done_sent => responder.error("handler failed")?,
            Err(_) => {}
        }

        // Whole response in a single write.
        write_stream.write_all(&out)?;
        out.clear();

        served += 1;
        if max_requests > 0 && served >= max_requests {
            return Ok(()); // recycle: EOF on the stream tells the router
        }
    }
}

/// Drain control frames that arrived on the private stream while the worker is
/// idle. The only expected frame is `Ping` (liveness) -> answer `Pong: idle`;
/// a stale `Abort` for a finished request is consumed and ignored.
fn answer_idle_control(
    abort: &mut UnixStream,
    write_stream: &mut UnixStream,
) -> Result<(), WorkerError> {
    loop {
        let mut head = [0u8; FRAME_HEADER_LEN];
        let n = match rustix::net::recv(&*abort, &mut head, rustix::net::RecvFlags::DONTWAIT) {
            Ok((0, _)) => return Err(WorkerError::Closed), // router gone
            Ok((n, _)) => n,
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(_) => return Err(WorkerError::Closed),
        };
        if n < FRAME_HEADER_LEN && abort.read_exact(&mut head[n..]).is_err() {
            return Err(WorkerError::Closed);
        }
        if let Ok(h) = FrameHeader::decode(&head)
            && h.kind == FrameKind::Ping
        {
            let mut pong = FrameHeader::new(FrameKind::Pong, h.request_id, 0);
            pong.aux = PONG_IDLE;
            write_stream.write_all(&pong.encode())?;
        }
    }
}

/// Read the HelloAck frame byte-exact (no read-ahead): a control frame sent
/// right after it must stay on the stream for the idle poll, not be swallowed.
fn read_hello_ack(stream: &mut UnixStream) -> Result<HelloAck, WorkerError> {
    let mut head = [0u8; FRAME_HEADER_LEN];
    match stream.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(WorkerError::Closed),
        Err(e) => return Err(e.into()),
    }
    let header = FrameHeader::decode(&head)?;
    if header.kind != FrameKind::HelloAck {
        return Err(BwpError::BadMagic.into());
    }
    // Grow with the data; a bogus payload_len hits EOF rather than pre-allocating.
    let want = u64::from(header.payload_len);
    let mut payload = Vec::new();
    if (&mut *stream).take(want).read_to_end(&mut payload)? as u64 != want {
        return Err(WorkerError::Closed);
    }
    HelloAck::decode(&payload).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufReader;

    /// Split a batched output buffer into (header, payload) frames.
    fn frames(buf: &[u8]) -> Vec<(FrameHeader, Vec<u8>)> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + FRAME_HEADER_LEN <= buf.len() {
            let h = FrameHeader::decode(buf[i..i + FRAME_HEADER_LEN].try_into().unwrap()).unwrap();
            let start = i + FRAME_HEADER_LEN;
            let end = start + h.payload_len as usize;
            out.push((h, buf[start..end].to_vec()));
            i = end;
        }
        out
    }

    #[test]
    fn send_body_reports_client_gone_on_abort() {
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let (mut abort, mut abort_peer) = UnixStream::pair().unwrap();
        // The router signals that request 7's client is gone.
        abort_peer.write_all(&FrameHeader::new(FrameKind::Abort, 7, 0).encode()).unwrap();

        let mut out = Vec::new();
        let mut r = Responder {
            stream: &mut stream,
            abort: &mut abort,
            out: &mut out,
            request_id: 7,
            headers_sent: true,
            end_sent: false,
            done_sent: false,
            aborted: false,
        };
        assert!(matches!(r.send_body(b"x"), Err(WorkerError::ClientGone)));
    }

    #[test]
    fn ping_is_answered_with_pong_busy_and_does_not_abort() {
        let (mut stream, mut stream_peer) = UnixStream::pair().unwrap();
        let (mut abort, mut abort_peer) = UnixStream::pair().unwrap();
        // Router pings the current request 7 (liveness, not a disconnect).
        abort_peer.write_all(&FrameHeader::new(FrameKind::Ping, 7, 0).encode()).unwrap();

        let mut out = Vec::new();
        let mut r = Responder {
            stream: &mut stream,
            abort: &mut abort,
            out: &mut out,
            request_id: 7,
            headers_sent: true,
            end_sent: false,
            done_sent: false,
            aborted: false,
        };
        assert!(r.send_body(b"x").is_ok(), "a Ping must not abort the request");

        // The worker answered Pong: busy on request 7.
        let mut head = [0u8; FRAME_HEADER_LEN];
        stream_peer.read_exact(&mut head).unwrap();
        let pong = FrameHeader::decode(&head).unwrap();
        assert_eq!((pong.kind, pong.request_id, pong.aux), (FrameKind::Pong, 7, PONG_BUSY));
    }

    #[test]
    fn abort_for_another_request_is_ignored() {
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let (mut abort, mut abort_peer) = UnixStream::pair().unwrap();
        // Stale abort for a different (finished) request must not abort ours.
        abort_peer.write_all(&FrameHeader::new(FrameKind::Abort, 99, 0).encode()).unwrap();

        let mut out = Vec::new();
        let mut r = Responder {
            stream: &mut stream,
            abort: &mut abort,
            out: &mut out,
            request_id: 7,
            headers_sent: true,
            end_sent: false,
            done_sent: false,
            aborted: false,
        };
        assert!(r.send_body(b"x").is_ok());
    }

    fn responder_output(build: impl FnOnce(&mut Responder<'_>)) -> Vec<u8> {
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        // Idle abort channel: the peer stays alive so a probe sees no data
        // (EAGAIN) rather than EOF.
        let (mut abort, _abort_peer) = UnixStream::pair().unwrap();
        let mut out = Vec::new();
        let mut r = Responder {
            stream: &mut stream,
            abort: &mut abort,
            out: &mut out,
            request_id: 7,
            headers_sent: false,
            end_sent: false,
            done_sent: false,
            aborted: false,
        };
        build(&mut r);
        out
    }

    #[test]
    fn push_frame_prepends_header() {
        let mut out = Vec::new();
        push_frame(&mut out, &FrameHeader::new(FrameKind::ResponseBody, 3, 2), b"hi");
        let f = frames(&out);
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].0.kind, FrameKind::ResponseBody);
        assert_eq!(f[0].0.request_id, 3);
        assert_eq!(f[0].1, b"hi");
    }

    #[test]
    fn headers_body_finish_are_batched_in_order() {
        let out = responder_output(|r| {
            r.send_headers(200, b"content-type: text/plain\r\n").unwrap();
            r.send_body(b"hello").unwrap();
            r.finish().unwrap();
        });
        let f = frames(&out);
        assert_eq!(f.len(), 4);

        assert_eq!(f[0].0.kind, FrameKind::ResponseHeaders);
        assert_eq!(f[0].0.aux, 200); // status rides in aux
        assert_eq!(f[0].0.request_id, 7);
        assert_eq!(f[0].1, b"content-type: text/plain\r\n");

        assert_eq!(f[1].0.kind, FrameKind::ResponseBody);
        assert_eq!(f[1].1, b"hello");

        // End releases the client, Done ends the task (frees the slot).
        assert_eq!(f[2].0.kind, FrameKind::End);
        assert_eq!(f[2].0.payload_len, 0);
        assert_eq!(f[3].0.kind, FrameKind::Done);
        assert_eq!(f[3].0.request_id, 7);
    }

    #[test]
    fn finish_is_idempotent() {
        let out = responder_output(|r| {
            r.send_headers(204, b"").unwrap();
            r.finish().unwrap();
            r.finish().unwrap(); // second call is a no-op
        });
        let ends = frames(&out).into_iter().filter(|(h, _)| h.kind == FrameKind::End).count();
        assert_eq!(ends, 1);
    }

    #[test]
    fn error_emits_error_frame() {
        let out = responder_output(|r| {
            r.error("boom").unwrap();
        });
        let f = frames(&out);
        assert_eq!(f[0].0.kind, FrameKind::Error);
        assert_eq!(f[0].1, b"boom");
    }

    fn read_frame_sync(reader: &mut impl Read) -> (FrameHeader, Vec<u8>) {
        let mut head = [0u8; FRAME_HEADER_LEN];
        reader.read_exact(&mut head).unwrap();
        let header = FrameHeader::decode(&head).unwrap();
        let mut payload = vec![0u8; header.payload_len as usize];
        reader.read_exact(&mut payload).unwrap();
        (header, payload)
    }

    fn write_frame_sync(stream: &mut UnixStream, kind: FrameKind, payload: &[u8]) {
        let mut buf = Vec::new();
        push_frame(&mut buf, &FrameHeader::new(kind, 0, payload.len() as u32), payload);
        stream.write_all(&buf).unwrap();
    }

    /// Full loop against a fake router: handshake, one request, recycle.
    #[test]
    fn run_serves_a_request_and_recycles_after_max_requests() {
        let (work_router, work_worker) = UnixDatagram::pair().unwrap();
        let (mut stream_router, stream_worker) = UnixStream::pair().unwrap();

        let router = std::thread::spawn(move || {
            let mut reader = BufReader::new(stream_router.try_clone().unwrap());

            // Hello: the blocking profile pins its declaration.
            let (header, payload) = read_frame_sync(&mut reader);
            assert_eq!(header.kind, FrameKind::Hello);
            let hello = buran_ipc::Hello::decode(&payload).unwrap();
            assert_eq!(hello.version, BWP_VERSION);
            assert_eq!(hello.concurrency, 1);
            assert_eq!(hello.capabilities, 0);
            assert!(hello.pid > 0);

            let ack = buran_ipc::HelloAck { version: BWP_VERSION, concurrency: 1 }.encode();
            write_frame_sync(&mut stream_router, FrameKind::HelloAck, &ack);

            // One request through the work queue.
            let req = buran_ipc::RequestBuilder::new().method(b"GET").path(b"/x").finish();
            let mut dgram = Vec::new();
            push_frame(&mut dgram, &FrameHeader::new(FrameKind::Request, 9, req.len() as u32), &req);
            work_router.send(&dgram).unwrap();

            let (claim, _) = read_frame_sync(&mut reader);
            assert_eq!((claim.kind, claim.request_id), (FrameKind::Claim, 9));

            let (headers, block) = read_frame_sync(&mut reader);
            assert_eq!((headers.kind, headers.aux), (FrameKind::ResponseHeaders, 200));
            assert_eq!(block, b"x-test: 1\r\n");

            let (body, chunk) = read_frame_sync(&mut reader);
            assert_eq!(body.kind, FrameKind::ResponseBody);
            assert_eq!(chunk, b"GET /x");

            // End (client) then Done (task) close the request.
            let (end, _) = read_frame_sync(&mut reader);
            assert_eq!((end.kind, end.request_id), (FrameKind::End, 9));
            let (done, _) = read_frame_sync(&mut reader);
            assert_eq!((done.kind, done.request_id), (FrameKind::Done, 9));
        });

        // max_requests = 1: the loop must return cleanly after one response.
        run(work_worker, stream_worker, 1, 0, |view, flags, responder| {
            assert_eq!(flags, 0);
            responder.send_headers(200, b"x-test: 1\r\n")?;
            let mut line = view.method().unwrap().to_vec();
            line.push(b' ');
            line.extend_from_slice(view.path().unwrap());
            responder.send_body(&line)?;
            responder.finish()
        })
        .unwrap();

        router.join().unwrap();
    }

    #[test]
    fn retire_datagram_exits_the_loop() {
        let (work_router, work_worker) = UnixDatagram::pair().unwrap();
        let (mut stream_router, stream_worker) = UnixStream::pair().unwrap();

        let router = std::thread::spawn(move || {
            let mut reader = BufReader::new(stream_router.try_clone().unwrap());
            let (header, _) = read_frame_sync(&mut reader);
            assert_eq!(header.kind, FrameKind::Hello);
            let ack = buran_ipc::HelloAck { version: BWP_VERSION, concurrency: 1 }.encode();
            write_frame_sync(&mut stream_router, FrameKind::HelloAck, &ack);
            // The idle worker waits on both sockets; a Retire datagram exits it.
            work_router.send(&FrameHeader::new(FrameKind::Retire, 0, 0).encode()).unwrap();
            (work_router, stream_router) // keep both alive until the worker exits
        });

        run(work_worker, stream_worker, 0, 0, |_, _, _| panic!("no request was sent")).unwrap();
        router.join().unwrap();
    }

    #[test]
    fn idle_worker_answers_ping_with_pong_idle() {
        let (work_router, work_worker) = UnixDatagram::pair().unwrap();
        let (mut stream_router, stream_worker) = UnixStream::pair().unwrap();

        let router = std::thread::spawn(move || {
            let mut reader = BufReader::new(stream_router.try_clone().unwrap());
            let (hello, _) = read_frame_sync(&mut reader);
            assert_eq!(hello.kind, FrameKind::Hello);
            let ack = buran_ipc::HelloAck { version: BWP_VERSION, concurrency: 1 }.encode();
            write_frame_sync(&mut stream_router, FrameKind::HelloAck, &ack);

            // Worker is idle now: a liveness Ping about request 42 must be
            // answered Pong: idle (proves the idle worker watches its stream).
            stream_router.write_all(&FrameHeader::new(FrameKind::Ping, 42, 0).encode()).unwrap();
            let (pong, _) = read_frame_sync(&mut reader);
            assert_eq!((pong.kind, pong.request_id, pong.aux), (FrameKind::Pong, 42, PONG_IDLE));

            work_router.send(&FrameHeader::new(FrameKind::Retire, 0, 0).encode()).unwrap();
            (work_router, stream_router)
        });

        run(work_worker, stream_worker, 0, 0, |_, _, _| panic!("no request was sent")).unwrap();
        router.join().unwrap();
    }
}
