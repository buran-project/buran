//! The concurrent BWP worker loop: executable reference for event-loop
//! runtime modules.
//!
//! Shape of the loop (the contract from `buran-ipc`, in code):
//!
//! - Hello declares unbounded concurrency + CAP_BODY_STREAM; the router
//!   answers with the granted value and this worker never holds more
//!   claimed requests than that (a semaphore permit per request).
//! - The permit is acquired BEFORE `recv` on the shared work socket: at
//!   capacity the worker stays out of the queue and the kernel wakes
//!   somebody else. That is the whole load-balancing story.
//! - One reader task demultiplexes RequestBody frames into per-request
//!   channels; one writer task serializes response frames from all
//!   concurrent handlers. Frames of different requests interleave freely.
//! - Retire and max_requests drain gracefully: stop taking work, finish
//!   every claimed request, then exit (stream EOF tells the router).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use buran_ipc::{
    FrameHeader, FrameKind, Hello, HelloAck, RequestView, BWP_VERSION, CAP_BODY_STREAM,
    CAP_WEBSOCKET, CONCURRENCY_UNBOUNDED, FLAG_BODY_FILE, FLAG_BODY_STREAM, FLAG_UPGRADE,
    FRAME_HEADER_LEN, PONG_BUSY, PONG_IDLE, WS_OP_BINARY, WS_OP_CLOSE, WS_OP_TEXT,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::net::{UnixDatagram, UnixStream};
use tokio::sync::{mpsc, Semaphore};

use crate::AppConfig;

/// Largest datagram a worker accepts (must cover header + inline payload).
const MAX_DGRAM: usize = 256 * 1024;

/// Practical ceiling when the router grants "unbounded": permits must be
/// finite and one request costs a task + buffers anyway.
const MAX_CONCURRENCY: u32 = 1024;

/// Router -> worker traffic of one claimed request.
enum Inbound {
    /// RequestBody chunk; empty = terminator.
    Body(Vec<u8>),
    /// WsMessage after an accepted upgrade: (opcode, payload).
    Ws(u32, Vec<u8>),
}

/// Per-request inbound frames are routed here by the reader task.
type BodyRoutes = Arc<Mutex<HashMap<u32, mpsc::Sender<Inbound>>>>;

/// In-flight handlers by request id (also the "busy" set for Ping); the value
/// is the handler's abort handle so an `Abort` can cancel it.
type InFlight = Arc<Mutex<HashMap<u32, tokio::task::AbortHandle>>>;

pub fn serve(
    work: std::os::unix::net::UnixDatagram,
    stream: std::os::unix::net::UnixStream,
    app: &AppConfig,
    token: u64,
) -> std::io::Result<()> {
    // The runtime is built here, strictly after the prototype's fork.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    rt.block_on(run(work, stream, app, token))
}

async fn run(
    work: std::os::unix::net::UnixDatagram,
    stream: std::os::unix::net::UnixStream,
    app: &AppConfig,
    token: u64,
) -> std::io::Result<()> {
    work.set_nonblocking(true)?;
    stream.set_nonblocking(true)?;
    let work = UnixDatagram::from_std(work)?;
    let stream = UnixStream::from_std(stream)?;
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::with_capacity(16 * 1024, rd);

    // Handshake: Hello -> HelloAck; the granted concurrency sizes the
    // permit pool.
    let hello = Hello {
        version: BWP_VERSION,
        pid: std::process::id(),
        concurrency: CONCURRENCY_UNBOUNDED,
        capabilities: CAP_BODY_STREAM | CAP_WEBSOCKET,
        token,
    }
    .encode();
    let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + hello.len());
    msg.extend_from_slice(&FrameHeader::new(FrameKind::Hello, 0, hello.len() as u32).encode());
    msg.extend_from_slice(&hello);
    wr.write_all(&msg).await?;

    let (header, payload) = read_frame(&mut rd).await?;
    if header.kind != FrameKind::HelloAck {
        return Err(std::io::Error::other("expected HelloAck"));
    }
    let ack = HelloAck::decode(&payload).map_err(|e| std::io::Error::other(e.to_string()))?;
    if ack.version != BWP_VERSION {
        return Err(std::io::Error::other(format!("unsupported BWP version {}", ack.version)));
    }
    let granted = ack.concurrency.min(MAX_CONCURRENCY);

    // Single writer task: response frames from all handlers funnel here.
    let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(256);
    let writer = tokio::spawn(async move {
        while let Some(buf) = out_rx.recv().await {
            if wr.write_all(&buf).await.is_err() {
                break;
            }
        }
    });

    // Reader task: routes RequestBody frames to their requests and answers
    // liveness Pings / task Aborts. `inflight` maps a request to its handler's
    // abort handle (also the "busy" set for Ping).
    let bodies: BodyRoutes = Arc::new(Mutex::new(HashMap::new()));
    let inflight: InFlight = Arc::new(Mutex::new(HashMap::new()));
    tokio::spawn(route_bodies(rd, Arc::clone(&bodies), Arc::clone(&inflight), out_tx.clone()));

    let sem = Arc::new(Semaphore::new(granted as usize));
    let mut served: u64 = 0;
    let mut dgram = vec![0u8; MAX_DGRAM];

    loop {
        // Capacity check first: no free slot, no recv.
        let permit = sem
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore never closed");

        let n = work.recv(&mut dgram).await?;
        if n < FRAME_HEADER_LEN {
            continue; // permit drops, slot freed
        }
        let header =
            match FrameHeader::decode(dgram[..FRAME_HEADER_LEN].try_into().expect("len checked")) {
                Ok(h) => h,
                Err(_) => continue,
            };
        match header.kind {
            FrameKind::Request => {}
            FrameKind::Retire => break, // graceful: drain below
            _ => continue,
        }
        let payload_end = FRAME_HEADER_LEN + header.payload_len as usize;
        if payload_end > n {
            continue; // truncated datagram: drop, router times it out
        }
        let payload = dgram[FRAME_HEADER_LEN..payload_end].to_vec();
        let id = header.request_id;

        // Streamed body or upgrade: the route must exist before Claim goes
        // out, or a fast router could race its first frame past us.
        let body_rx = if header.flags & (FLAG_BODY_STREAM | FLAG_UPGRADE) != 0 {
            let (tx, rx) = mpsc::channel::<Inbound>(8);
            bodies.lock().expect("bodies lock").insert(id, tx);
            Some(rx)
        } else {
            None
        };

        if out_tx
            .send(FrameHeader::new(FrameKind::Claim, id, 0).encode().to_vec())
            .await
            .is_err()
        {
            break; // router gone
        }

        let out = out_tx.clone();
        let bodies_c = Arc::clone(&bodies);
        let inflight_c = Arc::clone(&inflight);
        let jh = tokio::spawn(async move {
            let _permit = permit; // slot is held until the response is out
            handle_request(payload, header.flags, id, body_rx, out).await;
            bodies_c.lock().expect("bodies lock").remove(&id);
            inflight_c.lock().expect("inflight lock").remove(&id);
        });
        // Single-threaded runtime: the task cannot run until we await, so this
        // insert always precedes the handler's self-removal.
        inflight.lock().expect("inflight lock").insert(id, jh.abort_handle());

        served += 1;
        if app.max_requests > 0 && served >= app.max_requests {
            break; // recycle, but only after the drain below
        }
    }

    // Drain: every permit back = every claimed request finished.
    let _ = sem.acquire_many(granted).await;
    drop(out_tx);
    let _ = writer.await;
    Ok(())
}

/// Echo response: request line + headers of interest + the body, all as
/// plain text. Enough to assert routing, concurrency and body delivery
/// end to end.
async fn handle_request(
    payload: Vec<u8>,
    flags: u8,
    id: u32,
    body_rx: Option<mpsc::Receiver<Inbound>>,
    out: mpsc::Sender<Vec<u8>>,
) {
    if flags & FLAG_UPGRADE != 0 {
        ws_session(id, body_rx, out).await;
        return;
    }
    let mut buf = Vec::with_capacity(1024);
    match build_response(&payload, flags, body_rx).await {
        Ok(body) => {
            let headers = b"content-type: text/plain\r\n";
            let mut fh = FrameHeader::new(FrameKind::ResponseHeaders, id, headers.len() as u32);
            fh.aux = 200;
            push_frame(&mut buf, &fh, headers);
            push_frame(
                &mut buf,
                &FrameHeader::new(FrameKind::ResponseBody, id, body.len() as u32),
                &body,
            );
            // End releases the client, Done frees the slot (no background here,
            // so they go back to back).
            push_frame(&mut buf, &FrameHeader::new(FrameKind::End, id, 0), &[]);
            push_frame(&mut buf, &FrameHeader::new(FrameKind::Done, id, 0), &[]);
        }
        Err(msg) => {
            push_frame(
                &mut buf,
                &FrameHeader::new(FrameKind::Error, id, msg.len() as u32),
                msg.as_bytes(),
            );
        }
    }
    // One send = one batched write for the whole response.
    let _ = out.send(buf).await;
}

/// WebSocket echo: accept with 101, mirror every text/binary message,
/// End on close (or when the router side goes away). The whole concurrent
/// WS contract of `buran-ipc` in one function.
async fn ws_session(id: u32, body_rx: Option<mpsc::Receiver<Inbound>>, out: mpsc::Sender<Vec<u8>>) {
    let Some(mut rx) = body_rx else { return };

    let mut accept = Vec::with_capacity(FRAME_HEADER_LEN);
    let mut fh = FrameHeader::new(FrameKind::ResponseHeaders, id, 0);
    fh.aux = 101;
    push_frame(&mut accept, &fh, &[]);
    if out.send(accept).await.is_err() {
        return;
    }

    loop {
        match rx.recv().await {
            Some(Inbound::Ws(op @ (WS_OP_TEXT | WS_OP_BINARY), payload)) => {
                let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
                let mut fh = FrameHeader::new(FrameKind::WsMessage, id, payload.len() as u32);
                fh.aux = op;
                push_frame(&mut buf, &fh, &payload);
                if out.send(buf).await.is_err() {
                    return;
                }
            }
            // Close from the client (or router teardown): finish the
            // request — End releases the concurrency slot.
            Some(Inbound::Ws(WS_OP_CLOSE, _)) | None => break,
            Some(_) => {} // stray body chunks / unknown opcodes: drop
        }
    }

    let mut end = Vec::with_capacity(2 * FRAME_HEADER_LEN);
    push_frame(&mut end, &FrameHeader::new(FrameKind::End, id, 0), &[]);
    push_frame(&mut end, &FrameHeader::new(FrameKind::Done, id, 0), &[]);
    let _ = out.send(end).await;
}

async fn build_response(
    payload: &[u8],
    flags: u8,
    body_rx: Option<mpsc::Receiver<Inbound>>,
) -> Result<Vec<u8>, String> {
    let view = RequestView::parse(payload).map_err(|e| e.to_string())?;
    let body = read_body(&view, flags, body_rx).await?;

    let mut resp = Vec::with_capacity(128 + body.len());
    resp.extend_from_slice(view.method().map_err(|e| e.to_string())?);
    resp.push(b' ');
    resp.extend_from_slice(view.target().map_err(|e| e.to_string())?);
    resp.push(b'\n');
    resp.extend_from_slice(format!("body-length: {}\n", body.len()).as_bytes());
    resp.extend_from_slice(&body);
    Ok(resp)
}

async fn read_body(
    view: &RequestView<'_>,
    flags: u8,
    body_rx: Option<mpsc::Receiver<Inbound>>,
) -> Result<Vec<u8>, String> {
    if flags & FLAG_BODY_STREAM != 0 {
        let mut rx = body_rx.ok_or("streamed body without a channel")?;
        let total = view.content_length();
        let mut body = Vec::new();
        while let Some(inbound) = rx.recv().await {
            let Inbound::Body(chunk) = inbound else {
                continue;
            };
            if chunk.is_empty() {
                break; // terminator
            }
            body.extend_from_slice(&chunk);
            if body.len() as u64 >= total {
                break;
            }
        }
        if body.len() as u64 != total {
            // Short of content_length = the client aborted the upload.
            return Err("request body aborted".to_string());
        }
        Ok(body)
    } else if flags & FLAG_BODY_FILE != 0 {
        let path = std::str::from_utf8(view.preread_body().map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?
            .to_string();
        let data = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
        let _ = tokio::fs::remove_file(&path).await;
        Ok(data)
    } else {
        Ok(view.preread_body().map_err(|e| e.to_string())?.to_vec())
    }
}

/// Reader half of the response stream: after the handshake the router sends
/// RequestBody / WsMessage (per-request), plus liveness Pings and task Aborts.
async fn route_bodies(
    mut rd: BufReader<OwnedReadHalf>,
    bodies: BodyRoutes,
    inflight: InFlight,
    out: mpsc::Sender<Vec<u8>>,
) {
    loop {
        let Ok((header, payload)) = read_frame(&mut rd).await else {
            return; // router gone; in-flight handlers finish on their own
        };
        let id = header.request_id;
        let inbound = match header.kind {
            FrameKind::RequestBody => Inbound::Body(payload),
            FrameKind::WsMessage => Inbound::Ws(header.aux, payload),
            // Liveness probe: alive; busy iff the task is still running.
            FrameKind::Ping => {
                let busy = inflight.lock().expect("inflight lock").contains_key(&id);
                let mut pong = FrameHeader::new(FrameKind::Pong, id, 0);
                pong.aux = if busy { PONG_BUSY } else { PONG_IDLE };
                let _ = out.send(pong.encode().to_vec()).await;
                continue;
            }
            // Cooperative cancel: abort the handler and free the slot (Done).
            FrameKind::Abort => {
                // Take the handle (drops the guard) before any await.
                let handle = inflight.lock().expect("inflight lock").remove(&id);
                if let Some(handle) = handle {
                    handle.abort();
                    bodies.lock().expect("bodies lock").remove(&id);
                    let done = FrameHeader::new(FrameKind::Done, id, 0).encode().to_vec();
                    let _ = out.send(done).await;
                }
                continue;
            }
            _ => continue,
        };
        let tx = bodies.lock().expect("bodies lock").get(&id).cloned();
        if let Some(tx) = tx {
            // A closed receiver (request already finished) just drops the
            // frame — that is the "drain after early response" contract.
            let _ = tx.send(inbound).await;
        }
    }
}

fn push_frame(out: &mut Vec<u8>, header: &FrameHeader, payload: &[u8]) {
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(payload);
}

async fn read_frame(
    rd: &mut BufReader<OwnedReadHalf>,
) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut head = [0u8; FRAME_HEADER_LEN];
    rd.read_exact(&mut head).await?;
    let header = FrameHeader::decode(&head).map_err(|e| std::io::Error::other(e.to_string()))?;
    // Grow with the data instead of trusting payload_len up front (avoids a
    // 4 GiB pre-allocation from a bogus header).
    let want = u64::from(header.payload_len);
    let mut payload = Vec::new();
    if rd.take(want).read_to_end(&mut payload).await? as u64 != want {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buran_ipc::{RequestBuilder, PONG_BUSY};

    #[tokio::test]
    async fn route_bodies_answers_ping_and_aborts_task() {
        let (router, worker) = UnixStream::pair().unwrap();
        let (worker_rd, _worker_wr) = worker.into_split();
        let (_router_rd, mut router_wr) = router.into_split();

        let bodies: BodyRoutes = Arc::new(Mutex::new(HashMap::new()));
        let inflight: InFlight = Arc::new(Mutex::new(HashMap::new()));
        let (out_tx, mut out_rx) = mpsc::channel::<Vec<u8>>(8);
        tokio::spawn(route_bodies(
            BufReader::new(worker_rd),
            Arc::clone(&bodies),
            Arc::clone(&inflight),
            out_tx,
        ));

        // A running handler for request 5.
        let handler = tokio::spawn(async { std::future::pending::<()>().await });
        inflight.lock().unwrap().insert(5, handler.abort_handle());

        // Ping(5) -> Pong: busy.
        router_wr.write_all(&FrameHeader::new(FrameKind::Ping, 5, 0).encode()).await.unwrap();
        let pong = out_rx.recv().await.unwrap();
        let h = FrameHeader::decode(pong[..FRAME_HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!((h.kind, h.request_id, h.aux), (FrameKind::Pong, 5, PONG_BUSY));

        // Abort(5) -> handler cancelled, Done(5) frees the slot.
        router_wr.write_all(&FrameHeader::new(FrameKind::Abort, 5, 0).encode()).await.unwrap();
        let done = out_rx.recv().await.unwrap();
        let h = FrameHeader::decode(done[..FRAME_HEADER_LEN].try_into().unwrap()).unwrap();
        assert_eq!((h.kind, h.request_id), (FrameKind::Done, 5));
        assert!(handler.await.unwrap_err().is_cancelled());
        assert!(!inflight.lock().unwrap().contains_key(&5));
    }

    /// Split batched output buffers into (header, payload) frames.
    fn frames(bufs: Vec<Vec<u8>>) -> Vec<(FrameHeader, Vec<u8>)> {
        let mut out = Vec::new();
        for buf in bufs {
            let mut i = 0;
            while i + FRAME_HEADER_LEN <= buf.len() {
                let h =
                    FrameHeader::decode(buf[i..i + FRAME_HEADER_LEN].try_into().unwrap()).unwrap();
                let start = i + FRAME_HEADER_LEN;
                let end = start + h.payload_len as usize;
                out.push((h, buf[start..end].to_vec()));
                i = end;
            }
        }
        out
    }

    async fn drain(mut rx: mpsc::Receiver<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        while let Some(buf) = rx.recv().await {
            out.push(buf);
        }
        out
    }

    #[tokio::test]
    async fn ws_session_accepts_echoes_and_ends_on_close() {
        let (in_tx, in_rx) = mpsc::channel::<Inbound>(8);
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(64);

        let session = tokio::spawn(ws_session(7, Some(in_rx), out_tx));
        in_tx.send(Inbound::Ws(WS_OP_TEXT, b"marco".to_vec())).await.unwrap();
        in_tx.send(Inbound::Ws(WS_OP_BINARY, vec![1, 2, 3])).await.unwrap();
        in_tx.send(Inbound::Ws(WS_OP_CLOSE, vec![])).await.unwrap();
        session.await.unwrap();

        let f = frames(drain(out_rx).await);
        assert_eq!(f.len(), 5);
        assert_eq!((f[0].0.kind, f[0].0.aux), (FrameKind::ResponseHeaders, 101));
        assert_eq!((f[1].0.kind, f[1].0.aux), (FrameKind::WsMessage, WS_OP_TEXT));
        assert_eq!(f[1].1, b"marco");
        assert_eq!((f[2].0.kind, f[2].0.aux), (FrameKind::WsMessage, WS_OP_BINARY));
        assert_eq!(f[2].1, vec![1, 2, 3]);
        // End releases the client, Done frees the slot.
        assert_eq!((f[3].0.kind, f[3].0.request_id), (FrameKind::End, 7));
        assert_eq!((f[4].0.kind, f[4].0.request_id), (FrameKind::Done, 7));
    }

    #[tokio::test]
    async fn ws_session_ends_when_router_side_vanishes() {
        let (in_tx, in_rx) = mpsc::channel::<Inbound>(8);
        let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>(64);
        let session = tokio::spawn(ws_session(3, Some(in_rx), out_tx));
        drop(in_tx); // router teardown: inbound channel closes
        session.await.unwrap();

        let f = frames(drain(out_rx).await);
        // Terminates with End then Done.
        assert_eq!(f[f.len() - 2].0.kind, FrameKind::End);
        assert_eq!(f.last().unwrap().0.kind, FrameKind::Done);
    }

    #[tokio::test]
    async fn read_body_collects_streamed_chunks_until_total() {
        let payload = RequestBuilder::new().content_length(11).finish();
        let view = RequestView::parse(&payload).unwrap();

        let (tx, rx) = mpsc::channel::<Inbound>(8);
        tx.send(Inbound::Body(b"hello ".to_vec())).await.unwrap();
        tx.send(Inbound::Body(b"world".to_vec())).await.unwrap();
        drop(tx);

        let body = read_body(&view, FLAG_BODY_STREAM, Some(rx)).await.unwrap();
        assert_eq!(body, b"hello world");
    }

    #[tokio::test]
    async fn read_body_reports_aborted_stream() {
        let payload = RequestBuilder::new().content_length(100).finish();
        let view = RequestView::parse(&payload).unwrap();

        let (tx, rx) = mpsc::channel::<Inbound>(8);
        tx.send(Inbound::Body(b"only-a-piece".to_vec())).await.unwrap();
        tx.send(Inbound::Body(Vec::new())).await.unwrap(); // early terminator
        drop(tx);

        assert!(read_body(&view, FLAG_BODY_STREAM, Some(rx)).await.is_err());
    }

    #[tokio::test]
    async fn read_body_inline_passthrough() {
        let payload =
            RequestBuilder::new().content_length(4).preread_body(b"ping").finish();
        let view = RequestView::parse(&payload).unwrap();
        let body = read_body(&view, 0, None).await.unwrap();
        assert_eq!(body, b"ping");
    }
}
