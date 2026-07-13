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

use std::io::{BufReader, Read, Write};
use std::os::unix::net::{UnixDatagram, UnixStream};

use buran_ipc::{
    BwpError, FrameHeader, FrameKind, Hello, HelloAck, RequestView, BWP_VERSION, FRAME_HEADER_LEN,
};
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
    out: &'a mut Vec<u8>,
    request_id: u32,
    headers_sent: bool,
    finished: bool,
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
        debug_assert!(self.headers_sent && !self.finished);
        let fh = FrameHeader::new(FrameKind::ResponseBody, self.request_id, chunk.len() as u32);
        push_frame(self.out, &fh, chunk);
        // Large accumulations flush early: bounded worker memory and the
        // client sees bytes flowing.
        if self.out.len() >= 256 * 1024 {
            self.flush()?;
        }
        Ok(())
    }

    /// Finish the response (lazy: frames leave in one write after the
    /// handler returns). For the fastcgi_finish_request path — client
    /// released while the script keeps running — call `finish_now()`.
    pub fn finish(&mut self) -> Result<(), WorkerError> {
        if self.finished {
            return Ok(());
        }
        push_frame(self.out, &FrameHeader::new(FrameKind::End, self.request_id, 0), &[]);
        self.finished = true;
        Ok(())
    }

    /// Finish and flush immediately (early client release).
    pub fn finish_now(&mut self) -> Result<(), WorkerError> {
        self.finish()?;
        self.flush()
    }

    pub fn error(&mut self, message: &str) -> Result<(), WorkerError> {
        let fh = FrameHeader::new(FrameKind::Error, self.request_id, message.len() as u32);
        push_frame(self.out, &fh, message.as_bytes());
        self.finished = true;
        Ok(())
    }

    fn flush(&mut self) -> Result<(), WorkerError> {
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
    mut handler: F,
) -> Result<(), WorkerError>
where
    F: FnMut(&RequestView<'_>, u8, &mut Responder<'_>) -> Result<(), WorkerError>,
{
    let mut served: u64 = 0;
    let mut write_stream = resp.try_clone()?;
    let mut reader = BufReader::with_capacity(4 * 1024, resp);
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
    }
    .encode();
    let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + hello.len());
    push_frame(&mut msg, &FrameHeader::new(FrameKind::Hello, 0, hello.len() as u32), &hello);
    write_stream.write_all(&msg)?;

    let (ack, ack_payload) = read_frame(&mut reader)?;
    if ack.kind != FrameKind::HelloAck {
        return Err(BwpError::BadMagic.into());
    }
    let ack = HelloAck::decode(&ack_payload)?;
    if ack.version != BWP_VERSION {
        return Err(BwpError::UnsupportedVersion(ack.version).into());
    }

    loop {
        let n = work.recv(&mut dgram)?;
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

        // Claim: lets the router map request -> worker for diagnostics.
        write_stream
            .write_all(&FrameHeader::new(FrameKind::Claim, header.request_id, 0).encode())?;

        let view = RequestView::parse(payload)?;
        out.clear();
        let mut responder = Responder {
            stream: &mut write_stream,
            out: &mut out,
            request_id: header.request_id,
            headers_sent: false,
            finished: false,
        };

        match handler(&view, header.flags, &mut responder) {
            Ok(()) => responder.finish()?,
            Err(_) if !responder.finished => responder.error("handler failed")?,
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

fn read_frame(reader: &mut BufReader<UnixStream>) -> Result<(FrameHeader, Vec<u8>), WorkerError> {
    let mut head = [0u8; FRAME_HEADER_LEN];
    match reader.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(WorkerError::Closed),
        Err(e) => return Err(e.into()),
    }
    let header = FrameHeader::decode(&head)?;
    // Grow with the data instead of pre-allocating payload_len: a bogus header
    // hits EOF rather than committing gigabytes of zeroed memory.
    let want = u64::from(header.payload_len);
    let mut payload = Vec::new();
    if reader.by_ref().take(want).read_to_end(&mut payload)? as u64 != want {
        return Err(WorkerError::Closed);
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn responder_output(build: impl FnOnce(&mut Responder<'_>)) -> Vec<u8> {
        let (mut stream, _peer) = UnixStream::pair().unwrap();
        let mut out = Vec::new();
        let mut r = Responder {
            stream: &mut stream,
            out: &mut out,
            request_id: 7,
            headers_sent: false,
            finished: false,
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
        assert_eq!(f.len(), 3);

        assert_eq!(f[0].0.kind, FrameKind::ResponseHeaders);
        assert_eq!(f[0].0.aux, 200); // status rides in aux
        assert_eq!(f[0].0.request_id, 7);
        assert_eq!(f[0].1, b"content-type: text/plain\r\n");

        assert_eq!(f[1].0.kind, FrameKind::ResponseBody);
        assert_eq!(f[1].1, b"hello");

        assert_eq!(f[2].0.kind, FrameKind::End);
        assert_eq!(f[2].0.payload_len, 0);
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

            let (end, _) = read_frame_sync(&mut reader);
            assert_eq!((end.kind, end.request_id), (FrameKind::End, 9));
        });

        // max_requests = 1: the loop must return cleanly after one response.
        run(work_worker, stream_worker, 1, |view, flags, responder| {
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
            work_router.send(&FrameHeader::new(FrameKind::Retire, 0, 0).encode()).unwrap();
            work_router // keep the socket alive until the worker exits
        });

        run(work_worker, stream_worker, 0, |_, _, _| panic!("no request was sent")).unwrap();
        router.join().unwrap();
    }
}
