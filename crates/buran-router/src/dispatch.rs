//! Application worker pools: the router side of BWP, kernel-arbitrated
//! queue edition.
//!
//! Requests are datagrams on a shared AF_UNIX SOCK_DGRAM socket: the kernel
//! wakes exactly one idle worker per datagram — worker self-service without
//! a router round-trip (the MPMC arbitration, the hardest concurrency in
//! this design, is the kernel's job, not ours). A worker at its granted
//! concurrency stays out of `recv`, which is what keeps the kernel
//! balancing load across workers.
//!
//! Responses come back on each worker's private stream; a lightweight
//! per-worker reader task demultiplexes frames into per-request channels by
//! request id. Workers may interleave frames of many concurrent requests
//! (event-loop runtimes); the demux does not care. That extra hop is on the
//! client path, not in the worker's critical path.
//!
//! Saturation contract (spec 2.9):
//! - `queue.max`     = semaphore permits; none left -> instant 503;
//! - `queue.timeout` = deadline for the first sign of life (Claim);
//! - `limits.timeout`= per-event budget enforced by the caller. A stall
//!   fails the request, not the worker: the caller marks it stuck and only
//!   a worker whose every granted slot is stuck gets SIGKILLed (for
//!   concurrency 1 that is exactly the old kill-on-timeout semantics).
//!
//! Crash semantics: datagrams a dead worker never consumed stay queued for
//! the survivors; every request it had claimed fails.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use buran_config::{Application, Processes};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixDatagram;
use tracing::{error, info, warn};

use buran_ipc::{
    FrameHeader, FrameKind, Hello, HelloAck, BWP_VERSION, CAP_BODY_STREAM, CAP_WEBSOCKET,
    CONCURRENCY_UNBOUNDED, FLAG_BODY_FILE, FLAG_BODY_STREAM, FLAG_UPGRADE, FRAME_HEADER_LEN,
};

/// Spawns one worker process and returns the router side of its response
/// stream. Implemented by the supervisor (buran main).
pub type Spawner = Arc<dyn Fn() -> anyhow::Result<tokio::net::UnixStream> + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchError {
    /// queue.max reached -> instant 503.
    QueueFull,
    /// No worker claimed the request within queue.timeout -> 503.
    QueueTimeout,
    /// Worker/pool failure -> 502.
    WorkerFailed,
}

/// Response events delivered to the connection task.
pub enum WorkerEvent {
    Headers { status: u16, headers: Vec<u8> },
    Chunk(Vec<u8>),
    /// WebSocket message from the worker (upgraded requests only).
    Ws { opcode: u32, payload: Vec<u8> },
    End,
    Failed(DispatchError),
}

/// How the request body reaches the worker.
pub enum SubmitBody {
    /// Body (possibly empty) is inline in the request payload.
    Inline,
    /// The preread body field carries a temp-file path (FLAG_BODY_FILE).
    File,
    /// The body follows as RequestBody frames on the claiming worker's
    /// stream (FLAG_BODY_STREAM); chunks are drawn from this receiver once
    /// the request is claimed. Closing the sender ends the body: short of
    /// content_length means the client aborted.
    Stream(tokio::sync::mpsc::Receiver<Vec<u8>>),
    /// WebSocket upgrade offer (FLAG_UPGRADE, empty body): if the worker
    /// answers 101 the request turns into a WsMessage tunnel.
    Upgrade,
}

struct Pending {
    events: tokio::sync::mpsc::Sender<WorkerEvent>,
    /// Worker that claimed this request (key into `Pool::workers`).
    claimed_by: Option<u32>,
    /// Body chunk source for FLAG_BODY_STREAM requests; taken on Claim.
    body: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

/// Per-worker state shared between the reader task, body pumps and the
/// stuck-tracking path.
struct WorkerState {
    name: String,
    pid: u32,
    /// Effective concurrency granted in HelloAck (u32::MAX = unbounded).
    granted: u32,
    /// Write half of the worker's stream: HelloAck + RequestBody frames.
    /// Frames are locked whole so pumps of concurrent requests interleave
    /// at frame granularity.
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    /// Requests abandoned by the router (timeout) that the worker still has
    /// not finished. Every granted slot stuck = the worker is wedged.
    abandoned: Mutex<HashSet<u32>>,
}

impl WorkerState {
    async fn send_frame(&self, header: &FrameHeader, payload: &[u8]) -> std::io::Result<()> {
        let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        buf.extend_from_slice(&header.encode());
        buf.extend_from_slice(payload);
        self.writer.lock().await.write_all(&buf).await
    }
}

struct Metrics {
    active: AtomicU32,
    in_flight: AtomicU32,
}

pub struct Pool {
    /// Router end of the shared work socket (connected datagram pair).
    work: UnixDatagram,
    pending: Mutex<HashMap<u32, Pending>>,
    next_id: AtomicU32,
    queue: Arc<tokio::sync::Semaphore>,
    queue_timeout: Duration,
    request_timeout: Duration,
    spawner: Spawner,
    app_name: String,
    max: u32,
    spare: u32,
    /// None for fixed pools: no growth, no idle exit.
    idle_exit: Option<Duration>,
    metrics: Metrics,
    worker_seq: AtomicU32,
    body_temp_path: std::path::PathBuf,
    /// applications.<name>.concurrency (u32::MAX = no cap).
    concurrency_cap: u32,
    /// Concurrency granted to this pool's workers, set at first handshake
    /// (workers are homogeneous: same module binary, same config; 0 = no
    /// worker has completed a handshake yet).
    granted: AtomicU32,
    /// Capabilities declared by this pool's workers (same homogeneity).
    caps: AtomicU32,
    workers: Mutex<HashMap<u32, Arc<WorkerState>>>,
}

/// Payload larger than this spills to a temp file (datagram size budget)
/// or, for CAP_BODY_STREAM pools, flows as RequestBody frames.
pub const INLINE_BODY_LIMIT: usize = 96 * 1024;

impl Pool {
    pub fn start(
        app_name: &str,
        app: &Application,
        spawner: Spawner,
        work: std::os::unix::net::UnixDatagram,
        body_temp_path: &str,
    ) -> anyhow::Result<Arc<Pool>> {
        let (initial, max, spare, idle_exit) = match app.processes {
            Processes::Fixed(n) => (n, n, n, None),
            Processes::Dynamic { max, spare, idle_timeout } => {
                let spare = spare.max(1);
                (spare, max, spare, Some(Duration::from_secs(idle_timeout)))
            }
        };

        work.set_nonblocking(true)?;
        let work = UnixDatagram::from_std(work)?;

        std::fs::create_dir_all(body_temp_path)?;

        let pool = Arc::new(Pool {
            work,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            queue: Arc::new(tokio::sync::Semaphore::new(app.queue.max as usize)),
            queue_timeout: Duration::from_secs(app.queue.timeout),
            request_timeout: Duration::from_secs(app.limits.timeout),
            spawner,
            app_name: app_name.to_string(),
            max,
            spare,
            idle_exit,
            metrics: Metrics { active: AtomicU32::new(0), in_flight: AtomicU32::new(0) },
            worker_seq: AtomicU32::new(0),
            body_temp_path: std::path::PathBuf::from(body_temp_path),
            concurrency_cap: app.concurrency.unwrap_or(u32::MAX),
            granted: AtomicU32::new(0),
            caps: AtomicU32::new(0),
            workers: Mutex::new(HashMap::new()),
        });

        for _ in 0..initial {
            pool.spawn_worker();
        }
        if let Some(idle_exit) = idle_exit {
            tokio::spawn(janitor(Arc::clone(&pool), idle_exit));
        }

        Ok(pool)
    }

    /// Where oversized request bodies spill (FLAG_BODY_FILE).
    pub fn body_temp(&self) -> &std::path::Path {
        &self.body_temp_path
    }

    /// Whether this pool's workers accept streamed request bodies. False
    /// until the first handshake — early requests take the temp-file path.
    pub fn streams_body(&self) -> bool {
        self.caps.load(Ordering::Relaxed) & CAP_BODY_STREAM != 0
    }

    /// Whether this pool's workers accept WebSocket upgrades. False until
    /// the first handshake — early upgrade requests pass through as plain
    /// HTTP and the application answers what it can.
    pub fn supports_websocket(&self) -> bool {
        self.caps.load(Ordering::Relaxed) & CAP_WEBSOCKET != 0
    }

    /// Per-event budget for the caller's timeouts.
    pub fn event_timeout(&self) -> Duration {
        self.request_timeout + Duration::from_secs(5)
    }

    /// First-event budget: covers queue wait plus the request itself.
    pub fn first_event_timeout(&self) -> Duration {
        self.queue_timeout + self.request_timeout + Duration::from_secs(5)
    }

    /// (workers, in_flight, queued≈waiting permits) — for /status.
    pub fn stats(&self) -> (u32, u32, usize) {
        let in_flight = self.metrics.in_flight.load(Ordering::Relaxed);
        (self.metrics.active.load(Ordering::Relaxed), in_flight, self.pending.lock().map(|p| p.len()).unwrap_or(0))
    }

    /// Concurrency of one worker for capacity math: before the first
    /// handshake assume the blocking profile (1).
    fn worker_concurrency(&self) -> u64 {
        match self.granted.load(Ordering::Relaxed) {
            0 => 1,
            n => u64::from(n),
        }
    }

    /// Total requests the current workers can hold at once.
    fn capacity(&self) -> u64 {
        u64::from(self.metrics.active.load(Ordering::Relaxed)) * self.worker_concurrency()
    }

    /// Enqueue a request into the kernel work queue. The response arrives
    /// as events; the returned guard cleans up on drop.
    pub async fn submit(
        self: &Arc<Self>,
        payload: Vec<u8>,
        body: SubmitBody,
    ) -> Result<ResponseStream, DispatchError> {
        // Full queue -> fail fast, no waiting (spec 2.9).
        let permit = Arc::clone(&self.queue)
            .try_acquire_owned()
            .map_err(|_| DispatchError::QueueFull)?;

        let mut fh = FrameHeader::new(FrameKind::Request, 0, payload.len() as u32);
        let body_rx = match body {
            SubmitBody::Inline => None,
            SubmitBody::File => {
                fh.flags |= FLAG_BODY_FILE;
                None
            }
            SubmitBody::Stream(rx) => {
                fh.flags |= FLAG_BODY_STREAM;
                Some(rx)
            }
            SubmitBody::Upgrade => {
                fh.flags |= FLAG_UPGRADE;
                None
            }
        };

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        fh.request_id = id;
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        self.pending
            .lock()
            .expect("pending lock")
            .insert(id, Pending { events: tx, claimed_by: None, body: body_rx });
        self.metrics.in_flight.fetch_add(1, Ordering::Relaxed);

        let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        msg.extend_from_slice(&fh.encode());
        msg.extend_from_slice(&payload);

        // Demand-driven growth: capacity exhausted and room to grow.
        if self.idle_exit.is_some()
            && u64::from(self.metrics.in_flight.load(Ordering::Relaxed)) >= self.capacity()
            && self.metrics.active.load(Ordering::Relaxed) < self.max
        {
            self.spawn_worker();
        }

        // Socket buffer full = queue overflow -> bounded wait.
        let sent = tokio::time::timeout(self.queue_timeout, self.work.send(&msg)).await;
        match sent {
            Ok(Ok(_)) => Ok(ResponseStream { pool: Arc::clone(self), id, rx, _permit: permit }),
            Ok(Err(e)) => {
                self.remove_pending(id);
                error!(app = %self.app_name, "work queue send failed: {e}");
                Err(DispatchError::WorkerFailed)
            }
            Err(_) => {
                self.remove_pending(id);
                Err(DispatchError::QueueTimeout)
            }
        }
    }

    fn remove_pending(&self, id: u32) {
        if self.pending.lock().expect("pending lock").remove(&id).is_some() {
            self.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
        }
    }

    /// The caller gave up on request `id` (limits.timeout). The request is
    /// counted against the worker that claimed it; a worker whose every
    /// granted slot is stuck is wedged and gets SIGKILL — the respawn path
    /// then fails whatever else it held.
    fn mark_stuck(&self, id: u32) {
        let claimed_by = self
            .pending
            .lock()
            .expect("pending lock")
            .get(&id)
            .and_then(|p| p.claimed_by);
        let Some(worker_id) = claimed_by else {
            return; // never claimed: the datagram is lost or still queued
        };
        let Some(worker) = self.workers.lock().expect("workers lock").get(&worker_id).cloned()
        else {
            return; // already gone
        };

        let stuck = {
            let mut abandoned = worker.abandoned.lock().expect("abandoned lock");
            abandoned.insert(id);
            abandoned.len() as u64
        };
        warn!(
            app = %self.app_name, worker = %worker.name, request = id, stuck,
            "request timed out; abandoning"
        );
        // Unbounded workers (granted == MAX) are never declared wedged.
        if stuck >= u64::from(worker.granted) {
            error!(
                app = %self.app_name, worker = %worker.name, pid = worker.pid,
                "all {} slot(s) stuck; killing worker", worker.granted
            );
            kill_worker(worker.pid);
        }
    }

    /// Account a worker loss and restore the pool floor (fixed: `max`,
    /// dynamic: `spare`; growth beyond that is demand-driven).
    fn retire_and_refill(self: &Arc<Self>) {
        let before = self.metrics.active.fetch_sub(1, Ordering::Relaxed);
        let floor = if self.idle_exit.is_none() { self.max } else { self.spare };
        if before.saturating_sub(1) < floor {
            self.spawn_worker();
        }
    }

    /// Start one worker asynchronously; its reader task serves for its
    /// whole life and orders a replacement on exit.
    fn spawn_worker(self: &Arc<Self>) {
        self.metrics.active.fetch_add(1, Ordering::Relaxed);
        let id = self.worker_seq.fetch_add(1, Ordering::Relaxed);
        let pool = Arc::clone(self);
        let name = format!("{}[{id}]", self.app_name);

        tokio::spawn(async move {
            loop {
                let stream = match (pool.spawner)() {
                    Ok(s) => s,
                    Err(e) => {
                        error!(worker = %name, "spawn failed: {e:#}; retrying in 1s");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let (rd, wr) = stream.into_split();
                let mut reader = BufReader::with_capacity(64 * 1024, rd);
                match handshake(&pool, &name, &mut reader, wr).await {
                    Ok(worker) => {
                        info!(
                            worker = %name, pid = worker.pid,
                            concurrency = worker.granted, "worker ready"
                        );
                        pool.workers.lock().expect("workers lock").insert(id, Arc::clone(&worker));
                        reader_task(&pool, id, &worker, reader).await;
                        pool.workers.lock().expect("workers lock").remove(&id);
                        // Worker gone (recycle, crash, retire): the reader
                        // accounted for in-flight fallout; restore the floor.
                        pool.retire_and_refill();
                        return;
                    }
                    Err(e) => {
                        error!(worker = %name, "handshake failed: {e}; respawning");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        });
    }
}

fn kill_worker(pid: u32) {
    let Ok(raw) = i32::try_from(pid) else { return };
    let Some(pid) = rustix::process::Pid::from_raw(raw) else { return };
    if let Err(e) = rustix::process::kill_process(pid, rustix::process::Signal::KILL) {
        warn!("SIGKILL worker {raw}: {e}");
    }
}

/// Hello -> HelloAck. The granted concurrency is the declared value capped
/// by config; it also seeds the pool-wide capacity numbers (workers of one
/// pool are homogeneous by design: same binary, same config).
async fn handshake(
    pool: &Pool,
    name: &str,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
) -> std::io::Result<Arc<WorkerState>> {
    let (header, payload) = read_frame(reader).await?;
    if header.kind != FrameKind::Hello {
        return Err(std::io::Error::other("expected Hello"));
    }
    let hello = Hello::decode(&payload).map_err(|e| std::io::Error::other(e.to_string()))?;
    if hello.version != BWP_VERSION {
        return Err(std::io::Error::other(format!("unsupported BWP version {}", hello.version)));
    }

    let declared = match hello.concurrency {
        CONCURRENCY_UNBOUNDED => u32::MAX,
        n => n,
    };
    let granted = declared.min(pool.concurrency_cap);
    pool.granted.store(granted, Ordering::Relaxed);
    pool.caps.store(hello.capabilities, Ordering::Relaxed);

    let ack = HelloAck { version: BWP_VERSION, concurrency: granted }.encode();
    let worker = Arc::new(WorkerState {
        name: name.to_string(),
        pid: hello.pid,
        granted,
        writer: tokio::sync::Mutex::new(writer),
        abandoned: Mutex::new(HashSet::new()),
    });
    let fh = FrameHeader::new(FrameKind::HelloAck, 0, ack.len() as u32);
    worker.send_frame(&fh, &ack).await?;
    Ok(worker)
}

/// Read a worker's response stream for its whole life, demultiplexing
/// frames into pending request channels.
async fn reader_task(
    pool: &Arc<Pool>,
    worker_id: u32,
    worker: &Arc<WorkerState>,
    mut stream: BufReader<OwnedReadHalf>,
) {
    // Requests this worker has claimed and not yet finished. Event-loop
    // workers hold many at once.
    let mut claimed: HashSet<u32> = HashSet::new();

    loop {
        let (header, payload) = match read_frame(&mut stream).await {
            Ok(f) => f,
            Err(_) => {
                // Worker exited. Every claimed-but-unfinished request fails;
                // unconsumed datagrams stay queued for survivors.
                for id in claimed.drain() {
                    if let Some(p) = pool.pending.lock().expect("pending lock").remove(&id) {
                        pool.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
                        let _ = p.events.try_send(WorkerEvent::Failed(DispatchError::WorkerFailed));
                    }
                }
                return;
            }
        };

        let id = header.request_id;
        let event = match header.kind {
            FrameKind::Claim => {
                claimed.insert(id);
                let body = {
                    let mut pending = pool.pending.lock().expect("pending lock");
                    match pending.get_mut(&id) {
                        Some(p) => {
                            p.claimed_by = Some(worker_id);
                            p.body.take()
                        }
                        // Already abandoned (timeout/disconnect before the
                        // claim): the worker serves it into the void.
                        None => None,
                    }
                };
                if let Some(rx) = body {
                    tokio::spawn(pump_body(Arc::clone(worker), id, rx));
                }
                continue;
            }
            FrameKind::ResponseHeaders => WorkerEvent::Headers {
                status: header.aux.clamp(100, 599) as u16,
                headers: payload,
            },
            FrameKind::ResponseBody => WorkerEvent::Chunk(payload),
            FrameKind::WsMessage => WorkerEvent::Ws { opcode: header.aux, payload },
            FrameKind::End => {
                claimed.remove(&id);
                worker.abandoned.lock().expect("abandoned lock").remove(&id);
                WorkerEvent::End
            }
            FrameKind::Error => {
                warn!(worker = %worker.name, "worker error: {}", String::from_utf8_lossy(&payload));
                claimed.remove(&id);
                worker.abandoned.lock().expect("abandoned lock").remove(&id);
                WorkerEvent::Failed(DispatchError::WorkerFailed)
            }
            FrameKind::Log => {
                info!("worker: {}", String::from_utf8_lossy(&payload));
                continue;
            }
            _ => continue,
        };

        let is_end = matches!(event, WorkerEvent::End | WorkerEvent::Failed(_));
        let sender = {
            let mut pending = pool.pending.lock().expect("pending lock");
            if is_end {
                pending.remove(&id).map(|p| {
                    pool.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
                    p.events
                })
            } else {
                pending.get(&id).map(|p| p.events.clone())
            }
        };
        if let Some(sender) = sender {
            // Client gone (dropped receiver): frames are simply discarded.
            let _ = sender.send(event).await;
        }
    }
}

/// Feed one streamed request body to the worker that claimed it. Chunks
/// become RequestBody frames; the zero-length terminator always follows,
/// whether the source completed or was dropped mid-way (the worker tells
/// the two apart by comparing received bytes with content_length).
async fn pump_body(
    worker: Arc<WorkerState>,
    request_id: u32,
    mut rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) {
    while let Some(chunk) = rx.recv().await {
        if chunk.is_empty() {
            continue; // reserve zero-length for the terminator
        }
        let fh = FrameHeader::new(FrameKind::RequestBody, request_id, chunk.len() as u32);
        if worker.send_frame(&fh, &chunk).await.is_err() {
            return; // worker died; its reader task handles the fallout
        }
    }
    let fh = FrameHeader::new(FrameKind::RequestBody, request_id, 0);
    let _ = worker.send_frame(&fh, &[]).await;
}

/// Retire workers beyond demand: one Retire datagram per surplus worker;
/// exactly one worker consumes each, drains its claimed requests and exits.
async fn janitor(pool: Arc<Pool>, idle_exit: Duration) {
    let tick = (idle_exit / 2).max(Duration::from_secs(1));
    loop {
        tokio::time::sleep(tick).await;
        let active = u64::from(pool.metrics.active.load(Ordering::Relaxed));
        let in_flight = u64::from(pool.metrics.in_flight.load(Ordering::Relaxed));
        let conc = pool.worker_concurrency();
        // Workers needed to hold the current load, plus the spare floor.
        let needed = in_flight.div_ceil(conc) + u64::from(pool.spare);
        if active > needed {
            let retire = FrameHeader::new(FrameKind::Retire, 0, 0).encode();
            if pool.work.send(&retire).await.is_err() {
                return;
            }
            info!(app = %pool.app_name, active, in_flight, "shrinking pool by one");
        }
    }
}

/// Response stream for one submitted request. Dropping it abandons the
/// request: late frames are discarded by the reader task.
pub struct ResponseStream {
    pool: Arc<Pool>,
    id: u32,
    rx: tokio::sync::mpsc::Receiver<WorkerEvent>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl ResponseStream {
    pub async fn next_event(&mut self) -> Option<WorkerEvent> {
        self.rx.recv().await
    }

    /// The caller's per-event budget ran out. Distinguishes a genuine stall
    /// from a client disconnect (plain drop): only stalls count against the
    /// worker's health.
    pub fn mark_stuck(&self) {
        self.pool.mark_stuck(self.id);
    }

    /// Send one WebSocket message to the worker that claimed this request
    /// (upgraded requests only).
    pub async fn send_ws(&self, opcode: u32, payload: &[u8]) -> std::io::Result<()> {
        let worker = {
            let claimed_by = self
                .pool
                .pending
                .lock()
                .expect("pending lock")
                .get(&self.id)
                .and_then(|p| p.claimed_by)
                .ok_or_else(|| std::io::Error::other("request not claimed"))?;
            self.pool
                .workers
                .lock()
                .expect("workers lock")
                .get(&claimed_by)
                .cloned()
                .ok_or_else(|| std::io::Error::other("worker gone"))?
        };
        let mut fh = FrameHeader::new(FrameKind::WsMessage, self.id, payload.len() as u32);
        fh.aux = opcode;
        worker.send_frame(&fh, payload).await
    }
}

impl Drop for ResponseStream {
    fn drop(&mut self) {
        // Normal completion already removed the entry; this covers timeouts
        // and client disconnects.
        self.pool.remove_pending(self.id);
    }
}

async fn read_frame(stream: &mut BufReader<OwnedReadHalf>) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut head = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut head).await?;
    let header = FrameHeader::decode(&head)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut payload = vec![0u8; header.payload_len as usize];
    stream.read_exact(&mut payload).await?;
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buran_ipc::{RequestBuilder, WS_OP_TEXT};
    use tokio::net::UnixStream;
    use tokio::time::timeout;

    const TICK: Duration = Duration::from_secs(5);

    fn test_app(concurrency: Option<u32>) -> Application {
        let mut yaml = String::from(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: test
    processes: 1
",
        );
        if let Some(c) = concurrency {
            yaml.push_str(&format!("    concurrency: {c}\n"));
        }
        buran_config::from_str(&yaml).unwrap().applications.get("app").unwrap().clone()
    }

    /// Spawner that hands the worker-side stream ends to the test.
    fn test_spawner() -> (Spawner, tokio::sync::mpsc::UnboundedReceiver<UnixStream>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let spawner: Spawner = Arc::new(move || {
            let (router_side, worker_side) = std::os::unix::net::UnixStream::pair()?;
            router_side.set_nonblocking(true)?;
            worker_side.set_nonblocking(true)?;
            tx.send(UnixStream::from_std(worker_side)?)
                .map_err(|_| anyhow::anyhow!("test dropped the stream receiver"))?;
            Ok(UnixStream::from_std(router_side)?)
        });
        (spawner, rx)
    }

    async fn wk_send(stream: &mut UnixStream, kind: FrameKind, id: u32, aux: u32, payload: &[u8]) {
        let mut fh = FrameHeader::new(kind, id, payload.len() as u32);
        fh.aux = aux;
        let mut buf = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
        buf.extend_from_slice(&fh.encode());
        buf.extend_from_slice(payload);
        stream.write_all(&buf).await.unwrap();
    }

    async fn wk_recv(stream: &mut UnixStream) -> (FrameHeader, Vec<u8>) {
        let mut head = [0u8; FRAME_HEADER_LEN];
        stream.read_exact(&mut head).await.unwrap();
        let header = FrameHeader::decode(&head).unwrap();
        let mut payload = vec![0u8; header.payload_len as usize];
        stream.read_exact(&mut payload).await.unwrap();
        (header, payload)
    }

    struct Rig {
        pool: Arc<Pool>,
        /// Worker end of the private stream, handshake already done.
        stream: UnixStream,
        /// Worker end of the shared work socket.
        work: UnixDatagram,
        ack: HelloAck,
        /// Kept alive: dropping it would close respawned router ends.
        _spawns: tokio::sync::mpsc::UnboundedReceiver<UnixStream>,
    }

    /// Start a pool with one fake worker and complete its handshake.
    async fn rig(cap: Option<u32>, declare: u32, capabilities: u32) -> Rig {
        let (spawner, mut spawns) = test_spawner();
        let (router_work, worker_work) = std::os::unix::net::UnixDatagram::pair().unwrap();
        worker_work.set_nonblocking(true).unwrap();
        let work = UnixDatagram::from_std(worker_work).unwrap();
        let temp = std::env::temp_dir().join("buran-dispatch-tests");

        let pool = Pool::start(
            "app",
            &test_app(cap),
            spawner,
            router_work,
            temp.to_str().unwrap(),
        )
        .unwrap();

        let mut stream = timeout(TICK, spawns.recv()).await.unwrap().unwrap();
        let hello = Hello {
            version: BWP_VERSION,
            pid: 0, // kill_worker ignores pid 0: no stray signals from tests
            concurrency: declare,
            capabilities,
        }
        .encode();
        wk_send(&mut stream, FrameKind::Hello, 0, 0, &hello).await;
        let (header, payload) = wk_recv(&mut stream).await;
        assert_eq!(header.kind, FrameKind::HelloAck);
        let ack = HelloAck::decode(&payload).unwrap();

        Rig { pool, stream, work, ack, _spawns: spawns }
    }

    fn request_payload() -> Vec<u8> {
        RequestBuilder::new().method(b"GET").path(b"/t").finish()
    }

    /// Receive one Request datagram on the worker side.
    async fn wk_recv_work(work: &UnixDatagram) -> FrameHeader {
        let mut buf = vec![0u8; 64 * 1024];
        let n = timeout(TICK, work.recv(&mut buf)).await.unwrap().unwrap();
        assert!(n >= FRAME_HEADER_LEN);
        FrameHeader::decode(buf[..FRAME_HEADER_LEN].try_into().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn handshake_grants_capped_concurrency_and_records_caps() {
        let r = rig(Some(2), 8, CAP_BODY_STREAM | CAP_WEBSOCKET).await;
        assert_eq!(r.ack.version, BWP_VERSION);
        assert_eq!(r.ack.concurrency, 2, "declared 8 capped by config to 2");
        assert!(r.pool.streams_body());
        assert!(r.pool.supports_websocket());
    }

    #[tokio::test]
    async fn unbounded_declaration_without_cap_grants_max() {
        let r = rig(None, CONCURRENCY_UNBOUNDED, 0).await;
        assert_eq!(r.ack.concurrency, u32::MAX);
        assert!(!r.pool.streams_body());
        assert!(!r.pool.supports_websocket());
    }

    #[tokio::test]
    async fn interleaved_responses_demux_by_request_id() {
        let mut r = rig(Some(4), 4, 0).await;

        let mut first =
            r.pool.submit(request_payload(), SubmitBody::Inline).await.unwrap();
        let mut second =
            r.pool.submit(request_payload(), SubmitBody::Inline).await.unwrap();
        let id1 = wk_recv_work(&r.work).await.request_id;
        let id2 = wk_recv_work(&r.work).await.request_id;

        // Claim both, then interleave the two responses frame by frame.
        wk_send(&mut r.stream, FrameKind::Claim, id1, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::Claim, id2, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id2, 202, b"").await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id1, 201, b"").await;
        wk_send(&mut r.stream, FrameKind::ResponseBody, id1, 0, b"one").await;
        wk_send(&mut r.stream, FrameKind::ResponseBody, id2, 0, b"two").await;
        wk_send(&mut r.stream, FrameKind::End, id1, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::End, id2, 0, &[]).await;

        for (rs, status, body) in [(&mut first, 201, b"one"), (&mut second, 202, b"two")] {
            match timeout(TICK, rs.next_event()).await.unwrap() {
                Some(WorkerEvent::Headers { status: s, .. }) => assert_eq!(s, status),
                _ => panic!("expected headers"),
            }
            match timeout(TICK, rs.next_event()).await.unwrap() {
                Some(WorkerEvent::Chunk(c)) => assert_eq!(c, body),
                _ => panic!("expected chunk"),
            }
            assert!(matches!(
                timeout(TICK, rs.next_event()).await.unwrap(),
                Some(WorkerEvent::End)
            ));
        }
    }

    #[tokio::test]
    async fn worker_death_fails_every_claimed_request() {
        let mut r = rig(Some(4), 4, 0).await;

        let mut first =
            r.pool.submit(request_payload(), SubmitBody::Inline).await.unwrap();
        let mut second =
            r.pool.submit(request_payload(), SubmitBody::Inline).await.unwrap();
        let id1 = wk_recv_work(&r.work).await.request_id;
        let id2 = wk_recv_work(&r.work).await.request_id;

        wk_send(&mut r.stream, FrameKind::Claim, id1, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::Claim, id2, 0, &[]).await;
        // Give the reader task a moment to process both claims, then die.
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(r.stream);

        for rs in [&mut first, &mut second] {
            assert!(
                matches!(
                    timeout(TICK, rs.next_event()).await.unwrap(),
                    Some(WorkerEvent::Failed(DispatchError::WorkerFailed))
                ),
                "every claimed request must fail when the worker dies"
            );
        }
    }

    #[tokio::test]
    async fn body_pump_streams_chunks_and_terminator() {
        let mut r = rig(None, 4, CAP_BODY_STREAM).await;

        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let _rs = r.pool.submit(request_payload(), SubmitBody::Stream(rx)).await.unwrap();
        let header = wk_recv_work(&r.work).await;
        assert_ne!(header.flags & FLAG_BODY_STREAM, 0, "stream flag must be set");

        wk_send(&mut r.stream, FrameKind::Claim, header.request_id, 0, &[]).await;
        tx.send(b"hello ".to_vec()).await.unwrap();
        tx.send(b"world".to_vec()).await.unwrap();
        drop(tx);

        let mut got = Vec::new();
        loop {
            let (fh, payload) = timeout(TICK, wk_recv(&mut r.stream)).await.unwrap();
            assert_eq!(fh.kind, FrameKind::RequestBody);
            assert_eq!(fh.request_id, header.request_id);
            if payload.is_empty() {
                break; // terminator
            }
            got.extend_from_slice(&payload);
        }
        assert_eq!(got, b"hello world");
    }

    #[tokio::test]
    async fn ws_messages_flow_both_ways() {
        let mut r = rig(None, 4, CAP_WEBSOCKET).await;

        let mut rs = r.pool.submit(request_payload(), SubmitBody::Upgrade).await.unwrap();
        let header = wk_recv_work(&r.work).await;
        assert_ne!(header.flags & FLAG_UPGRADE, 0, "upgrade flag must be set");
        let id = header.request_id;

        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id, 101, b"").await;
        match timeout(TICK, rs.next_event()).await.unwrap() {
            Some(WorkerEvent::Headers { status, .. }) => assert_eq!(status, 101),
            _ => panic!("expected 101"),
        }

        // Router -> worker.
        rs.send_ws(WS_OP_TEXT, b"marco").await.unwrap();
        let (fh, payload) = timeout(TICK, wk_recv(&mut r.stream)).await.unwrap();
        assert_eq!(fh.kind, FrameKind::WsMessage);
        assert_eq!((fh.request_id, fh.aux), (id, WS_OP_TEXT));
        assert_eq!(payload, b"marco");

        // Worker -> router.
        wk_send(&mut r.stream, FrameKind::WsMessage, id, WS_OP_TEXT, b"polo").await;
        match timeout(TICK, rs.next_event()).await.unwrap() {
            Some(WorkerEvent::Ws { opcode, payload }) => {
                assert_eq!(opcode, WS_OP_TEXT);
                assert_eq!(payload, b"polo");
            }
            _ => panic!("expected ws message"),
        }

        // End releases the slot like any other request.
        wk_send(&mut r.stream, FrameKind::End, id, 0, &[]).await;
        assert!(matches!(timeout(TICK, rs.next_event()).await.unwrap(), Some(WorkerEvent::End)));
    }
}
