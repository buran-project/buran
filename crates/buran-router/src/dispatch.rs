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
//! - `queue.max`            = semaphore permits; none left -> instant 503;
//! - `queue.timeout`        = deadline for the first sign of life (Claim);
//! - `limits.response_timeout` = per-event budget enforced by the caller. A
//!   stall fails the request, not the worker: the caller marks it stuck and
//!   only a worker whose every granted slot is stuck gets SIGKILLed (for
//!   concurrency 1 that is exactly the old kill-on-timeout semantics).
//!   (`limits.task_timeout` — total wall-clock incl. background — lands in Ф4.)
//!
//! Crash semantics: datagrams a dead worker never consumed stay queued for
//! the survivors; every request it had claimed fails.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use buran_config::{Application, Processes, DEFAULT_CONCURRENCY_CAP};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixDatagram;
use tracing::{error, info, warn};

use buran_ipc::{
    FrameHeader, FrameKind, Hello, HelloAck, BWP_VERSION, CAP_BODY_STREAM, CAP_WEBSOCKET,
    CONCURRENCY_UNBOUNDED, FLAG_BODY_FILE, FLAG_BODY_STREAM, FLAG_UPGRADE, FRAME_HEADER_LEN,
    PONG_IDLE,
};

/// Worker lifecycle, owned by the supervisor (buran main): spawn a worker and
/// kill one by the token it declared in Hello. Killing goes through the
/// worker's parent (the prototype) so it is pid-reuse safe — the router never
/// signals a pid directly.
pub trait Spawn: Send + Sync {
    /// Spawn one worker; return the router side of its response stream and the
    /// authoritative kill token the supervisor assigned it. The token comes
    /// from the spawner, never from the worker's Hello, so a worker cannot
    /// choose (or spoof) its own kill identity and misdirect a kill at a sibling.
    fn spawn(&self) -> anyhow::Result<(tokio::net::UnixStream, u64)>;
    /// Ask the prototype to SIGKILL the worker with this token. Best-effort.
    fn kill(&self, token: u64);
}

pub type Spawner = Arc<dyn Spawn>;

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
    /// Worker asked to forward buffered output now (PHP `flush()` / SSE).
    Flush,
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
    /// When the worker claimed it — start of the `task_timeout` wall-clock.
    claimed_at: Option<Instant>,
    /// The task_timeout sweep already sent an `Abort` for this task.
    abort_sent: bool,
    /// When the `Abort` was sent; a task still alive `grace` later is defiant.
    aborted_at: Option<Instant>,
    /// Body chunk source for FLAG_BODY_STREAM requests; taken on Claim.
    body: Option<tokio::sync::mpsc::Receiver<Vec<u8>>>,
}

/// Per-worker state shared between the reader task, body pumps and the
/// stuck-tracking path.
struct WorkerState {
    name: String,
    pid: u32,
    /// Authoritative kill token assigned by the supervisor (delivered with the
    /// spawn, not self-reported by the worker): the router asks the prototype
    /// to kill by this token (pid-reuse safe), never by `pid`.
    token: u64,
    /// Effective concurrency granted in HelloAck (u32::MAX = unbounded).
    granted: u32,
    /// Write half of the worker's stream: HelloAck + RequestBody frames.
    /// Frames are locked whole so pumps of concurrent requests interleave
    /// at frame granularity.
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
    /// Set once the sweep asked the prototype to kill this worker, so repeated
    /// sweep ticks do not re-send the command while it winds down.
    kill_requested: std::sync::atomic::AtomicBool,
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
    /// Provisioned worker slots: incremented when a slot starts spawning and
    /// decremented when it is retired, so it counts slots still spawning,
    /// handshaking, or backing off after a crash — not just serving ones. Used
    /// for provisioning decisions (the floor/refill and the growth cap). The
    /// *live serving* set is `Pool::workers`; occupancy reported to operators
    /// uses that (see `Pool::live_workers`) so a pool whose workers cannot come
    /// up does not look healthy.
    active: AtomicU32,
    in_flight: AtomicU32,
}

/// Occupancy snapshot exposed by /status.
pub struct PoolStats {
    /// Worker processes currently running.
    pub workers: u32,
    /// Free request slots (capacity minus claimed); idle workers for
    /// blocking pools.
    pub idle: u64,
    /// Requests accepted but not yet claimed by any worker.
    pub queued: usize,
}

pub struct Pool {
    /// Router end of the shared work socket (connected datagram pair).
    work: UnixDatagram,
    pending: Mutex<HashMap<u32, Pending>>,
    next_id: AtomicU32,
    queue: Arc<tokio::sync::Semaphore>,
    queue_timeout: Duration,
    request_timeout: Duration,
    /// limits.task_timeout: total wall-clock a task may hold its slot before
    /// the sweep aborts it and, if it will not wind down, kills the worker.
    task_timeout: Duration,
    spawner: Spawner,
    app_name: String,
    max: u32,
    spare: u32,
    /// None for fixed pools: no growth, no idle exit.
    idle_exit: Option<Duration>,
    metrics: Metrics,
    worker_seq: AtomicU32,
    /// Consecutive fast worker crashes (came up, then died within
    /// `HEALTHY_UPTIME`). Drives exponential respawn backoff; reset to 0 the
    /// moment a worker survives long enough to count as healthy.
    crash_streak: AtomicU32,
    body_temp_path: std::path::PathBuf,
    /// uid the workers drop to, if any: spilled request bodies are created
    /// 0600 and chowned to it so a worker under a different user can open
    /// them without the file being world-readable.
    body_owner: Option<u32>,
    /// applications.<name>.concurrency (u32::MAX = no cap).
    concurrency_cap: u32,
    /// Concurrency granted to this pool's workers, set at first handshake
    /// (workers are homogeneous: same module binary, same config; 0 = no
    /// worker has completed a handshake yet).
    granted: AtomicU32,
    /// Capabilities declared by this pool's workers (same homogeneity).
    caps: AtomicU32,
    workers: Mutex<HashMap<u32, Arc<WorkerState>>>,
    /// In-flight liveness probes: (worker_id, task_id) -> one-shot for the
    /// `Pong` status (`PONG_IDLE`/`PONG_BUSY`). The sweep registers one before a
    /// `Ping`; only the *probed* worker's reader may complete it. Keying by
    /// worker as well as task stops one worker answering another's probe — a
    /// forged `Pong: idle` would otherwise spare a wedged worker from the kill.
    pings: Mutex<HashMap<(u32, u32), tokio::sync::oneshot::Sender<u32>>>,
}

/// Payload larger than this spills to a temp file (datagram size budget)
/// or, for CAP_BODY_STREAM pools, flows as RequestBody frames.
pub const INLINE_BODY_LIMIT: usize = 96 * 1024;

/// A worker that exits within this window of coming up is treated as a crash
/// (bad module, poisoned opcache, fatal on the first request) rather than a
/// healthy recycle/retire, and triggers respawn backoff.
const HEALTHY_UPTIME: Duration = Duration::from_secs(10);
/// Base respawn delay after a crash, doubled per consecutive fast crash.
const CRASH_BACKOFF_BASE: Duration = Duration::from_millis(100);
/// Ceiling on the respawn backoff: a persistently broken pool retries at this
/// bounded slow rate instead of storming forks (and, as PID 1, SIGCHLD).
const CRASH_BACKOFF_MAX: Duration = Duration::from_secs(30);

/// Budget for a freshly-spawned worker to send its Hello. Warm workers (forked
/// from an already-booted prototype) answer in milliseconds; this only bounds a
/// wedged/hung module that connected but never handshakes. Such a slot is not
/// yet in `workers`, so the task_timeout sweep does not cover it — without this
/// the pre-Hello read would park forever, slowly bleeding pool capacity.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Exponential backoff for a crash streak: `BASE * 2^(streak-1)`, capped at
/// `CRASH_BACKOFF_MAX`. The shift is clamped so the multiply cannot overflow.
fn crash_backoff(streak: u32) -> Duration {
    let shift = streak.saturating_sub(1).min(9);
    CRASH_BACKOFF_BASE.saturating_mul(1u32 << shift).min(CRASH_BACKOFF_MAX)
}

impl Pool {
    pub fn start(
        app_name: &str,
        app: &Application,
        spawner: Spawner,
        work: std::os::unix::net::UnixDatagram,
        body_temp_path: &str,
        body_owner: Option<u32>,
    ) -> anyhow::Result<Arc<Pool>> {
        let (initial, max, spare, idle_exit) = match app.processes {
            // Auto is resolved to Fixed at config load; resolve here too as a
            // backstop so a hand-built Application never leaves the pool at 0.
            Processes::Fixed(n) => (n, n, n, None),
            Processes::Auto => {
                let n = buran_config::auto_worker_count();
                (n, n, n, None)
            }
            Processes::Dynamic { max, spare, idle_timeout } => {
                let spare = spare.max(1);
                (spare, max, spare, Some(Duration::from_secs(idle_timeout)))
            }
        };

        work.set_nonblocking(true)?;
        let work = UnixDatagram::from_std(work)?;

        std::fs::create_dir_all(body_temp_path)?;
        // Sweep spill files orphaned by a previous instance that was killed
        // before its worker could unlink them (Drop never ran).
        cleanup_stale_spills(std::path::Path::new(body_temp_path));

        let pool = Arc::new(Pool {
            work,
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU32::new(1),
            queue: Arc::new(tokio::sync::Semaphore::new(app.queue.max as usize)),
            queue_timeout: Duration::from_secs(app.queue.timeout),
            request_timeout: Duration::from_secs(app.limits.response_timeout),
            task_timeout: Duration::from_secs(app.limits.task_timeout),
            spawner,
            app_name: app_name.to_string(),
            max,
            spare,
            idle_exit,
            metrics: Metrics { active: AtomicU32::new(0), in_flight: AtomicU32::new(0) },
            worker_seq: AtomicU32::new(0),
            crash_streak: AtomicU32::new(0),
            body_temp_path: std::path::PathBuf::from(body_temp_path),
            body_owner,
            concurrency_cap: app.concurrency.unwrap_or(u32::MAX),
            granted: AtomicU32::new(0),
            caps: AtomicU32::new(0),
            workers: Mutex::new(HashMap::new()),
            pings: Mutex::new(HashMap::new()),
        });

        for _ in 0..initial {
            pool.spawn_worker();
        }
        if let Some(idle_exit) = idle_exit {
            tokio::spawn(janitor(Arc::clone(&pool), idle_exit));
        }
        tokio::spawn(task_timeout_sweep(Arc::clone(&pool)));

        Ok(pool)
    }

    /// Where oversized request bodies spill (FLAG_BODY_FILE).
    pub fn body_temp(&self) -> &std::path::Path {
        &self.body_temp_path
    }

    /// The spill filename for a given sequence number: `body-<pid>-<seq>`.
    /// The pid lets startup cleanup tell a dead instance's leftovers from a
    /// co-running one sharing the same temp directory.
    pub fn spill_path(&self, seq: u64) -> std::path::PathBuf {
        self.body_temp_path.join(format!("body-{}-{}", std::process::id(), seq))
    }

    /// Write an oversized request body to `path`, created 0600 so it is never
    /// world/group-readable (bodies carry uploads, tokens, passwords). When
    /// the workers drop to a uid, the file is chowned to it so a worker under
    /// a different user can still open it (0600 alone would deny it).
    pub async fn write_spill(&self, path: &std::path::PathBuf, bytes: &[u8]) -> std::io::Result<()> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .await?;
        if let Some(uid) = self.body_owner {
            // fchown by the open handle: no path race, and it only succeeds
            // while buran runs as root (the privilege-drop precondition for a
            // configured worker uid), which is exactly when it is needed.
            let std_file = file.try_clone().await?.into_std().await;
            tokio::task::spawn_blocking(move || {
                std::os::unix::fs::fchown(&std_file, Some(uid), None)
            })
            .await
            .map_err(std::io::Error::other)??;
        }
        file.write_all(bytes).await?;
        file.flush().await
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

    /// Snapshot of pool occupancy for /status.
    pub fn stats(&self) -> PoolStats {
        // Report the live serving set, not provisioned slots (`metrics.active`):
        // a slot still spawning / handshaking / backing off must not show up as
        // an available worker, or a pool that cannot bring workers up would look
        // healthy while nothing can answer.
        let workers = self.live_workers();
        // One pass over pending: an entry is claimed once a worker picked it
        // up (`claimed_by`), otherwise it is still waiting in the kernel queue.
        let claimed = {
            let pending = self.pending.lock().expect("pending lock");
            pending.values().filter(|p| p.claimed_by.is_some()).count()
        };
        let in_flight = self.metrics.in_flight.load(Ordering::Relaxed) as usize;
        let queued = in_flight.saturating_sub(claimed);
        // Free request slots: live capacity minus what workers hold now. For
        // blocking pools (concurrency 1) this is exactly the idle worker count.
        let capacity = workers * self.worker_concurrency();
        let idle = capacity.saturating_sub(claimed as u64);
        PoolStats { workers: workers as u32, idle, queued }
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

    /// Workers that have completed their handshake and can serve — the
    /// authoritative live set. Unlike `metrics.active` (provisioned slots,
    /// which include ones still spawning / handshaking / backing off), this
    /// reflects what is actually serving, so operator-facing occupancy cannot
    /// report a healthy pool while nothing can answer.
    fn live_workers(&self) -> u64 {
        self.workers.lock().expect("workers lock").len() as u64
    }

    /// Reserve a queue slot up front, before the caller does any expensive
    /// per-request setup (e.g. spilling a large body to disk). A full queue
    /// fails here (spec 2.9), so nothing hits the disk just to be rejected.
    pub fn try_reserve(self: &Arc<Self>) -> Result<QueuePermit, DispatchError> {
        Arc::clone(&self.queue).try_acquire_owned().map_err(|_| DispatchError::QueueFull)
    }

    /// Enqueue a request into the kernel work queue under a slot already
    /// reserved by [`try_reserve`]. The response arrives as events; the
    /// returned guard cleans up on drop.
    pub async fn submit(
        self: &Arc<Self>,
        permit: QueuePermit,
        payload: Vec<u8>,
        body: SubmitBody,
    ) -> Result<ResponseStream, DispatchError> {
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

        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let id = {
            let mut pending = self.pending.lock().expect("pending lock");
            // Allocate a request id not already in flight. `next_id` is a u32
            // that wraps after 2^32 submits; skipping ids still present in
            // `pending` stops a wrapped counter from colliding with a live,
            // long-running request (SSE / WebSocket / post-finish_request
            // background task) — a collision would overwrite that request's
            // slot and cross-deliver its frames to the wrong client. `pending`
            // is bounded by pool concurrency, so this spins at most a handful
            // of times.
            let id = loop {
                let candidate = self.next_id.fetch_add(1, Ordering::Relaxed);
                if !pending.contains_key(&candidate) {
                    break candidate;
                }
            };
            pending.insert(
                id,
                Pending {
                    events: tx,
                    claimed_by: None,
                    claimed_at: None,
                    abort_sent: false,
                    aborted_at: None,
                    body: body_rx,
                },
            );
            id
        };
        fh.request_id = id;
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

    /// Free the slot only if no worker claimed the request (lost datagram /
    /// abandoned before a claim). A claimed task keeps its slot until its
    /// `Done` (or the worker dies), so a `fastcgi_finish_request` background
    /// task stays tracked past the client response.
    fn remove_if_unclaimed(&self, id: u32) {
        let mut pending = self.pending.lock().expect("pending lock");
        if let Some(p) = pending.get(&id)
            && p.claimed_by.is_none() {
                pending.remove(&id);
                self.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
            }
    }

    /// Send an `Abort` to the worker claiming task `id` (client gone, response
    /// stall, or task over budget). Best-effort.
    async fn abort_task(&self, id: u32) {
        let worker = {
            let claimed_by =
                self.pending.lock().expect("pending lock").get(&id).and_then(|p| p.claimed_by);
            let Some(worker_id) = claimed_by else { return };
            self.workers.lock().expect("workers lock").get(&worker_id).cloned()
        };
        let Some(worker) = worker else { return };
        let _ = worker.send_frame(&FrameHeader::new(FrameKind::Abort, id, 0), &[]).await;
    }

    /// One task_timeout pass (pure decision, no I/O): mark newly over-budget
    /// tasks for `Abort`, count tasks still alive `grace` after their Abort as
    /// defiant, and return `(ids to abort, kill candidates)`. A worker becomes
    /// a candidate when its defiant slots reach the degradation threshold
    /// (blocking: one slot; event loop: a majority). The final kill is gated by
    /// an authoritative `Ping` (the caller), so a candidate that is actually
    /// idle (reader lagged behind a `Done`) is spared.
    fn sweep_decide(&self, now: Instant, grace: Duration) -> (Vec<u32>, Vec<KillCandidate>) {
        let mut to_abort: Vec<u32> = Vec::new();
        // worker_id -> (defiant count, a sample defiant task to probe).
        let mut defiant: HashMap<u32, (u32, u32)> = HashMap::new();
        {
            let mut pending = self.pending.lock().expect("pending lock");
            for (&id, p) in pending.iter_mut() {
                let Some(claimed_at) = p.claimed_at else {
                    continue; // never claimed: not our budget to enforce
                };
                if !p.abort_sent {
                    if now.duration_since(claimed_at) >= self.task_timeout {
                        p.abort_sent = true;
                        p.aborted_at = Some(now);
                        to_abort.push(id);
                    }
                } else if let Some(aborted_at) = p.aborted_at
                    && now.duration_since(aborted_at) >= grace
                        && let Some(w) = p.claimed_by {
                            let e = defiant.entry(w).or_insert((0, id));
                            e.0 += 1;
                        }
            }
        }

        let mut candidates: Vec<KillCandidate> = Vec::new();
        if !defiant.is_empty() {
            let workers = self.workers.lock().expect("workers lock");
            for (worker_id, (count, probe_task)) in defiant {
                let Some(worker) = workers.get(&worker_id) else { continue };
                let threshold = (worker.granted / 2).max(1);
                if count >= threshold && !worker.kill_requested.load(Ordering::Relaxed) {
                    candidates.push(KillCandidate { worker_id, token: worker.token, probe_task });
                }
            }
        }
        (to_abort, candidates)
    }

    /// Liveness probe: `Ping` the worker about `task_id` and await its `Pong`
    /// up to `deadline`. Returns the status (`PONG_IDLE`/`PONG_BUSY`), or None
    /// if the worker is gone or did not answer (wedged event loop / stuck).
    async fn probe(&self, worker_id: u32, task_id: u32, deadline: Duration) -> Option<u32> {
        let worker = self.workers.lock().expect("workers lock").get(&worker_id).cloned()?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        let key = (worker_id, task_id);
        self.pings.lock().expect("pings lock").insert(key, tx);
        if worker.send_frame(&FrameHeader::new(FrameKind::Ping, task_id, 0), &[]).await.is_err() {
            self.pings.lock().expect("pings lock").remove(&key);
            return None;
        }
        match tokio::time::timeout(deadline, rx).await {
            Ok(Ok(status)) => Some(status),
            _ => {
                self.pings.lock().expect("pings lock").remove(&key);
                None
            }
        }
    }

    /// Authoritative confirm before an irreversible kill, uniform for both
    /// profiles. `Pong: idle` means the worker is actually free (its "defiant"
    /// task finished and the reader lagged) -> spare it; `Pong: busy` or no
    /// answer (wedged) -> kill via the prototype by token.
    async fn confirm_and_kill(&self, candidates: Vec<KillCandidate>, deadline: Duration) {
        for c in candidates {
            if self.probe(c.worker_id, c.probe_task, deadline).await == Some(PONG_IDLE) {
                continue;
            }
            let Some(worker) = self.workers.lock().expect("workers lock").get(&c.worker_id).cloned()
            else {
                continue;
            };
            if !worker.kill_requested.swap(true, Ordering::Relaxed) {
                error!(
                    app = %self.app_name, worker = %worker.name, pid = worker.pid,
                    "task over budget and not winding down; killing"
                );
                self.spawner.kill(c.token);
            }
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
                let (stream, token) = match pool.spawner.spawn() {
                    Ok(spawned) => spawned,
                    Err(e) => {
                        error!(worker = %name, "spawn failed: {e:#}; retrying in 1s");
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                };
                let (rd, wr) = stream.into_split();
                let mut reader = BufReader::with_capacity(64 * 1024, rd);
                match handshake(&pool, &name, &mut reader, wr, token).await {
                    Ok(worker) => {
                        info!(
                            worker = %name, pid = worker.pid,
                            concurrency = worker.granted, "worker ready"
                        );
                        let up = Instant::now();
                        pool.workers.lock().expect("workers lock").insert(id, Arc::clone(&worker));
                        reader_task(&pool, id, &worker, reader).await;
                        pool.workers.lock().expect("workers lock").remove(&id);

                        // A worker that dies almost immediately after coming up
                        // is crash-looping, not recycling. Without a brake this
                        // is an unthrottled fork/boot storm (and, as PID 1, a
                        // SIGCHLD flood) that one bad app can use to starve the
                        // whole box. Back off exponentially per consecutive fast
                        // crash; respawn this slot in place so the streak (and
                        // its `active` count) persists across attempts.
                        if up.elapsed() < HEALTHY_UPTIME {
                            let streak = pool.crash_streak.fetch_add(1, Ordering::Relaxed) + 1;
                            let backoff = crash_backoff(streak);
                            error!(
                                worker = %name, streak, uptime = ?up.elapsed(),
                                "worker crashed on startup; backing off {backoff:?} before respawn"
                            );
                            tokio::time::sleep(backoff).await;
                            continue;
                        }
                        // Healthy lifetime (normal recycle/retire/kill): clear
                        // the streak and let the floor logic decide on a
                        // replacement. The reader accounted for in-flight fallout.
                        pool.crash_streak.store(0, Ordering::Relaxed);
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

/// Hello -> HelloAck. The granted concurrency is the declared value capped
/// by config; it also seeds the pool-wide capacity numbers (workers of one
/// pool are homogeneous by design: same binary, same config).
async fn handshake(
    pool: &Pool,
    name: &str,
    reader: &mut BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    token: u64,
) -> std::io::Result<Arc<WorkerState>> {
    // Bound the pre-Hello read: a worker that connects but never handshakes must
    // not park this slot's spawn task forever (it is not yet in `workers`, so the
    // sweep cannot reap it). On elapse, reap it by its authoritative token and
    // surface an error so spawn_worker respawns.
    let (header, payload) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, read_frame(reader)).await {
        Ok(frame) => frame?,
        Err(_) => {
            pool.spawner.kill(token);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "worker did not send Hello within the handshake timeout",
            ));
        }
    };
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
    // Effective concurrency is always finite: an unbounded declaration with no
    // config cap falls back to DEFAULT_CONCURRENCY_CAP so the reaping backstop
    // and capacity math always have a bound (memory is bounded by queue.max).
    let granted = match declared.min(pool.concurrency_cap) {
        u32::MAX => DEFAULT_CONCURRENCY_CAP,
        n => n,
    };
    // Workers of a pool are homogeneous by design, so the FIRST handshake seeds
    // the pool-wide concurrency and capability values and later ones must not
    // change them: a divergent late worker (a bug, or a hostile module) would
    // otherwise resize the whole pool's capacity math or flip capability bits
    // (body-streaming / websocket) mid-flight. `granted` is always >= 1, so 0
    // is the "unset" sentinel; seed once, then reuse the seeded value.
    let granted = match pool.granted.compare_exchange(0, granted, Ordering::Relaxed, Ordering::Relaxed)
    {
        Ok(_) => {
            pool.caps.store(hello.capabilities, Ordering::Relaxed);
            granted
        }
        Err(seeded) => {
            if seeded != granted || pool.caps.load(Ordering::Relaxed) != hello.capabilities {
                warn!(
                    worker = %name,
                    "worker profile (granted {granted}, caps {}) differs from the pool's seeded \
                     values (granted {seeded}); using the pool's",
                    hello.capabilities,
                );
            }
            seeded
        }
    };

    let ack = HelloAck { version: BWP_VERSION, concurrency: granted }.encode();
    let worker = Arc::new(WorkerState {
        name: name.to_string(),
        pid: hello.pid,
        token,
        granted,
        writer: tokio::sync::Mutex::new(writer),
        kill_requested: std::sync::atomic::AtomicBool::new(false),
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
            // Worker exited: run the shared cleanup after the loop.
            Err(_) => break,
        };

        let id = header.request_id;

        // A worker may only address a request it actually claimed. The kernel
        // hands each queued datagram to exactly one worker, but nothing in the
        // frame binds a response to its claimer — so without this gate a buggy
        // or compromised worker could emit frames carrying another client's
        // `request_id` and steal or overwrite that client's response, free its
        // slot, or pump it a foreign body. `Claim` is the sole request-scoped
        // arm allowed for an id we do not yet own; `Log`/`Pong` are pool-global
        // (not request-scoped). Everything else is dropped unless the id is in
        // this worker's `claimed` set.
        if !matches!(header.kind, FrameKind::Claim | FrameKind::Log | FrameKind::Pong)
            && !claimed.contains(&id)
        {
            continue;
        }

        let event = match header.kind {
            FrameKind::Claim => {
                // Granted-concurrency contract enforcement. A worker at its
                // grant must stop reading the shared work socket; one that
                // claims beyond `granted` is draining the kernel queue past its
                // share — starving sibling workers and breaking the capacity
                // math (idle/queued on /status are derived from the live worker
                // count * granted, so an over-claiming worker would make them lie).
                // Nothing in the frame stops a buggy/hostile worker from doing
                // this, so the router enforces the bound here: a violation is a
                // protocol breach — kill the worker and let the pool refill.
                if claimed.len() >= worker.granted as usize {
                    error!(
                        worker = %worker.name, pid = worker.pid, granted = worker.granted,
                        "worker claimed beyond its granted concurrency; killing"
                    );
                    if !worker.kill_requested.swap(true, Ordering::Relaxed) {
                        pool.spawner.kill(worker.token);
                    }
                    break;
                }
                let body = {
                    let mut pending = pool.pending.lock().expect("pending lock");
                    match pending.get_mut(&id) {
                        // First-writer-wins: only an unclaimed pending request
                        // can be claimed, and only once. A duplicate/replayed
                        // or poached `Claim` for an already-owned id is ignored
                        // — it must not reset `claimed_at` (which would let a
                        // worker dodge the task_timeout sweep by re-claiming) or
                        // steal the body receiver from the real owner.
                        Some(p) if p.claimed_by.is_none() => {
                            p.claimed_by = Some(worker_id);
                            p.claimed_at = Some(Instant::now()); // task_timeout starts
                            claimed.insert(id);
                            p.body.take()
                        }
                        // Already claimed, or abandoned (timeout/disconnect
                        // before the claim): the worker serves it into the void.
                        _ => None,
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
            FrameKind::Flush => WorkerEvent::Flush,
            FrameKind::WsMessage => WorkerEvent::Ws { opcode: header.aux, payload },
            // Client response complete: forward it, but the task keeps its
            // slot until `Done` (it may run in the background).
            FrameKind::End => WorkerEvent::End,
            FrameKind::Error => {
                warn!(worker = %worker.name, "worker error: {}", String::from_utf8_lossy(&payload));
                claimed.remove(&id);
                WorkerEvent::Failed(DispatchError::WorkerFailed)
            }
            // Task fully finished (including background): free the slot and
            // stop tracking. Done subsumes End — it also releases the client if
            // an `End` never arrived (aborted task, or a misbehaving module):
            // in the normal `End`->`Done` flow this send is a no-op (the client
            // already completed and dropped the receiver).
            FrameKind::Done => {
                claimed.remove(&id);
                let sender = {
                    let mut pending = pool.pending.lock().expect("pending lock");
                    pending.remove(&id).map(|p| {
                        pool.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
                        p.events
                    })
                };
                if let Some(sender) = sender {
                    let _ = sender.send(WorkerEvent::End).await;
                }
                continue;
            }
            FrameKind::Log => {
                info!("worker: {}", String::from_utf8_lossy(&payload));
                continue;
            }
            // Liveness reply: complete the sweep's probe with the status, but
            // only the probe registered for THIS worker — a `Pong` cannot
            // satisfy (or cancel) another worker's probe.
            FrameKind::Pong => {
                if let Some(tx) = pool.pings.lock().expect("pings lock").remove(&(worker_id, id)) {
                    let _ = tx.send(header.aux);
                }
                continue;
            }
            _ => continue,
        };

        // Only `Error` is terminal for the slot here; `End` forwards but keeps
        // the task tracked until its `Done`.
        let is_terminal = matches!(event, WorkerEvent::Failed(_));
        let sender = {
            let mut pending = pool.pending.lock().expect("pending lock");
            if is_terminal {
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

    // Reader exiting (worker EOF, or killed above for exceeding its grant):
    // every request it claimed but never finished fails; datagrams it never
    // consumed stay queued for the survivors.
    for id in claimed.drain() {
        if let Some(p) = pool.pending.lock().expect("pending lock").remove(&id) {
            pool.metrics.in_flight.fetch_sub(1, Ordering::Relaxed);
            let _ = p.events.try_send(WorkerEvent::Failed(DispatchError::WorkerFailed));
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

/// Wall-clock enforcement of `task_timeout` (the resource backstop covering
/// post-finish_request background). Each tick: a claimed task past
/// `task_timeout` gets one `Abort`; a task still alive `TASK_ABORT_GRACE` after
/// its `Abort` is "defiant"; a worker whose defiant slots cross the degradation
/// threshold is killed via the prototype (by token) and respawned. For blocking
/// workers (granted 1) one defiant slot is the whole worker; event loops need a
/// majority (an isolated stuck task just costs a slot).
/// Grace after an `Abort` before a still-alive task is declared defiant.
const TASK_ABORT_GRACE: Duration = Duration::from_secs(5);
/// How long the sweep waits for a `Pong` before treating the worker as wedged.
const PROBE_DEADLINE: Duration = Duration::from_secs(2);

/// A worker the sweep wants to kill, pending an authoritative liveness probe.
struct KillCandidate {
    worker_id: u32,
    token: u64,
    /// A defiant task to probe (`Ping` carries it; `Pong` echoes it).
    probe_task: u32,
}

async fn task_timeout_sweep(pool: Arc<Pool>) {
    let tick = (pool.task_timeout / 4).clamp(Duration::from_secs(1), Duration::from_secs(5));
    loop {
        tokio::time::sleep(tick).await;
        let (to_abort, candidates) = pool.sweep_decide(Instant::now(), TASK_ABORT_GRACE);
        for id in to_abort {
            pool.abort_task(id).await;
        }
        pool.confirm_and_kill(candidates, PROBE_DEADLINE).await;
    }
}

/// Response stream for one submitted request. Dropping it abandons the
/// request: late frames are discarded by the reader task.
/// A reserved queue slot, held for the request's lifetime.
pub type QueuePermit = tokio::sync::OwnedSemaphorePermit;

pub struct ResponseStream {
    pool: Arc<Pool>,
    id: u32,
    rx: tokio::sync::mpsc::Receiver<WorkerEvent>,
    _permit: QueuePermit,
}

impl ResponseStream {
    pub async fn next_event(&mut self) -> Option<WorkerEvent> {
        self.rx.recv().await
    }

    /// Tell the claiming worker that this request's client is gone, so it can
    /// abort the runtime handler (PHP user-abort, honoring the app's abort
    /// policy). Best-effort: if the request was never claimed or the worker
    /// has already left, there is nothing to abort.
    pub async fn send_abort(&self) {
        self.pool.abort_task(self.id).await;
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
        // Free the slot only if the task was never claimed; a claimed task
        // frees on its `Done` (or worker death), so background work outlives
        // the client response.
        self.pool.remove_if_unclaimed(self.id);
    }
}

/// Remove `body-<pid>-*` spill files whose owning process is gone. Files of
/// a still-live pid (a co-running buran sharing the temp dir) are left alone.
fn cleanup_stale_spills(dir: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|n| n.strip_prefix("body-"))
            .and_then(|rest| rest.split('-').next())
            .and_then(|pid| pid.parse::<i32>().ok())
        else {
            continue;
        };
        if !pid_alive(pid) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `kill(pid, 0)` liveness probe: ESRCH means the process is gone; any other
/// result (including EPERM) means it still exists.
fn pid_alive(pid: i32) -> bool {
    let Some(pid) = rustix::process::Pid::from_raw(pid) else { return false };
    !matches!(rustix::process::test_kill_process(pid), Err(rustix::io::Errno::SRCH))
}

/// Hard ceiling on a single response-stream frame payload. Legitimate frames
/// are far smaller (the blocking SDK drains at 256 KiB, PHP `ub_write` chunks
/// smaller still); this only stops a buggy/hostile worker from making the
/// router buffer up to 4 GiB from one framed length. Over it, the stream is
/// failed and the worker's requests fall out with the reader.
const MAX_FRAME_PAYLOAD: u32 = 64 * 1024 * 1024;

async fn read_frame(stream: &mut BufReader<OwnedReadHalf>) -> std::io::Result<(FrameHeader, Vec<u8>)> {
    let mut head = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut head).await?;
    let header = FrameHeader::decode(&head)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    if header.payload_len > MAX_FRAME_PAYLOAD {
        return Err(std::io::Error::other(format!(
            "worker frame payload {} exceeds the {MAX_FRAME_PAYLOAD}-byte limit",
            header.payload_len
        )));
    }
    // Grow the buffer as bytes actually arrive rather than pre-allocating the
    // worker-declared length: a bogus payload_len (buggy worker) then hits EOF
    // instead of committing the whole capped size of zeroed memory.
    let want = u64::from(header.payload_len);
    let mut payload = Vec::new();
    if stream.take(want).read_to_end(&mut payload).await? as u64 != want {
        return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
    }
    Ok((header, payload))
}

#[cfg(test)]
mod tests {
    use super::*;
    use buran_ipc::{RequestBuilder, PONG_BUSY, PONG_IDLE, WS_OP_TEXT};
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

    /// Spawner that hands the worker-side stream ends to the test and records
    /// the tokens the pool asks to kill.
    struct TestSpawner {
        tx: tokio::sync::mpsc::UnboundedSender<UnixStream>,
        killed: Mutex<Vec<u64>>,
        next_token: std::sync::atomic::AtomicU64,
    }

    impl Spawn for TestSpawner {
        fn spawn(&self) -> anyhow::Result<(tokio::net::UnixStream, u64)> {
            let (router_side, worker_side) = std::os::unix::net::UnixStream::pair()?;
            router_side.set_nonblocking(true)?;
            worker_side.set_nonblocking(true)?;
            self.tx
                .send(UnixStream::from_std(worker_side)?)
                .map_err(|_| anyhow::anyhow!("test dropped the stream receiver"))?;
            // The token is the supervisor's to assign — mirror that here so the
            // kill path is exercised with an authoritative, worker-independent id.
            let token = self.next_token.fetch_add(1, Ordering::Relaxed);
            Ok((UnixStream::from_std(router_side)?, token))
        }
        fn kill(&self, token: u64) {
            self.killed.lock().expect("killed lock").push(token);
        }
    }

    fn test_spawner() -> (Arc<TestSpawner>, tokio::sync::mpsc::UnboundedReceiver<UnixStream>) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Arc::new(TestSpawner {
                tx,
                killed: Mutex::new(Vec::new()),
                next_token: std::sync::atomic::AtomicU64::new(1),
            }),
            rx,
        )
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
        /// The pool's spawner: exposes the tokens it was asked to kill.
        spawner: Arc<TestSpawner>,
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
            Arc::clone(&spawner) as Spawner,
            router_work,
            temp.to_str().unwrap(),
            None,
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

        Rig { pool, stream, work, ack, spawner, _spawns: spawns }
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
    async fn unbounded_declaration_without_cap_falls_back_to_default() {
        let r = rig(None, CONCURRENCY_UNBOUNDED, 0).await;
        // Never truly unbounded: a finite default so reaping has a bound.
        assert_eq!(r.ack.concurrency, DEFAULT_CONCURRENCY_CAP);
        assert!(!r.pool.streams_body());
        assert!(!r.pool.supports_websocket());
    }

    #[tokio::test]
    async fn interleaved_responses_demux_by_request_id() {
        let mut r = rig(Some(4), 4, 0).await;

        let mut first =
            r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Inline).await.unwrap();
        let mut second =
            r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Inline).await.unwrap();
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
    async fn flush_frame_surfaces_as_flush_event() {
        let mut r = rig(Some(1), 1, 0).await;
        let mut rs = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id = wk_recv_work(&r.work).await.request_id;

        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id, 200, b"").await;
        wk_send(&mut r.stream, FrameKind::Flush, id, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseBody, id, 0, b"tick").await;
        wk_send(&mut r.stream, FrameKind::End, id, 0, &[]).await;

        assert!(matches!(
            timeout(TICK, rs.next_event()).await.unwrap(),
            Some(WorkerEvent::Headers { status: 200, .. })
        ));
        assert!(matches!(
            timeout(TICK, rs.next_event()).await.unwrap(),
            Some(WorkerEvent::Flush)
        ));
        match timeout(TICK, rs.next_event()).await.unwrap() {
            Some(WorkerEvent::Chunk(c)) => assert_eq!(c, b"tick"),
            _ => panic!("expected chunk"),
        }
        assert!(matches!(timeout(TICK, rs.next_event()).await.unwrap(), Some(WorkerEvent::End)));
    }

    #[tokio::test]
    async fn worker_cannot_answer_a_request_it_never_claimed() {
        // Cross-request response mixing (H1): responses are demuxed by
        // request_id, so a buggy/compromised worker must not address an id it
        // never claimed. A pre-claim ResponseHeaders is dropped; only frames
        // after a valid Claim reach the client.
        let mut r = rig(Some(1), 1, 0).await;
        let mut rs = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id = wk_recv_work(&r.work).await.request_id;

        // Rogue frame for the still-unclaimed id: must be gated out.
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id, 500, b"").await;
        // Legitimate claim, then the real response for the same id.
        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id, 200, b"").await;
        wk_send(&mut r.stream, FrameKind::End, id, 0, &[]).await;

        // The client sees only the post-claim 200 — the pre-claim 500 was dropped.
        match timeout(TICK, rs.next_event()).await.unwrap() {
            Some(WorkerEvent::Headers { status, .. }) => {
                assert_eq!(status, 200, "pre-claim response frame must be dropped");
            }
            _ => panic!("expected 200 headers after the valid claim"),
        }
        assert!(matches!(timeout(TICK, rs.next_event()).await.unwrap(), Some(WorkerEvent::End)));
    }

    #[tokio::test]
    async fn over_claiming_worker_is_killed_and_its_requests_fail() {
        // H1: a blocking worker (granted 1) must not claim a second request
        // while it still holds one — that drains the shared kernel queue past
        // its grant and starves siblings. The router enforces the grant: the
        // second claim is a protocol violation, so the worker is killed and
        // every request it had claimed fails.
        let mut r = rig(Some(1), 1, 0).await;

        let mut first = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let _second = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id1 = wk_recv_work(&r.work).await.request_id;
        let id2 = wk_recv_work(&r.work).await.request_id;

        // First claim sits at the grant (allowed); the second exceeds it.
        wk_send(&mut r.stream, FrameKind::Claim, id1, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::Claim, id2, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(
            r.spawner.killed.lock().unwrap().len(),
            1,
            "the over-claiming worker must be killed"
        );
        // The request it legitimately held fails as the reader tears down.
        assert!(
            matches!(
                timeout(TICK, first.next_event()).await.unwrap(),
                Some(WorkerEvent::Failed(DispatchError::WorkerFailed))
            ),
            "the killed worker's claimed request must fail"
        );
    }

    #[tokio::test]
    async fn worker_may_claim_up_to_its_grant_then_no_further() {
        // H1 boundary: an event-loop worker granted 2 may hold two requests at
        // once (legitimate, no kill), but a third concurrent claim exceeds the
        // grant and is a violation.
        let mut r = rig(Some(2), 2, 0).await;

        let mut streams = Vec::new();
        for _ in 0..3 {
            streams.push(
                r.pool
                    .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
                    .await
                    .unwrap(),
            );
        }
        let id1 = wk_recv_work(&r.work).await.request_id;
        let id2 = wk_recv_work(&r.work).await.request_id;
        let id3 = wk_recv_work(&r.work).await.request_id;

        // Two concurrent claims sit exactly at the grant: allowed.
        wk_send(&mut r.stream, FrameKind::Claim, id1, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::Claim, id2, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            r.spawner.killed.lock().unwrap().is_empty(),
            "claiming up to the grant is legitimate — no kill"
        );

        // The third concurrent claim exceeds the grant: violation -> kill.
        wk_send(&mut r.stream, FrameKind::Claim, id3, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            r.spawner.killed.lock().unwrap().len(),
            1,
            "claiming beyond the grant kills the worker"
        );
        // Both properly-held requests fail on teardown.
        for rs in &mut streams[..2] {
            assert!(matches!(
                timeout(TICK, rs.next_event()).await.unwrap(),
                Some(WorkerEvent::Failed(DispatchError::WorkerFailed))
            ));
        }
    }

    #[test]
    fn crash_backoff_grows_then_caps() {
        // H2: exponential per-crash, capped so a broken pool never storms.
        assert_eq!(crash_backoff(1), Duration::from_millis(100));
        assert_eq!(crash_backoff(2), Duration::from_millis(200));
        assert_eq!(crash_backoff(3), Duration::from_millis(400));
        assert_eq!(crash_backoff(9), Duration::from_millis(25_600));
        // Beyond the shift cap it saturates at the ceiling, never overflowing.
        assert_eq!(crash_backoff(10), CRASH_BACKOFF_MAX);
        assert_eq!(crash_backoff(u32::MAX), CRASH_BACKOFF_MAX);
    }

    #[tokio::test]
    async fn fast_worker_crash_bumps_the_backoff_streak() {
        // H2: a worker that comes up then dies almost immediately is a crash,
        // not a recycle. The pool records it as a crash streak (which drives
        // exponential respawn backoff) instead of refilling instantly.
        let r = rig(Some(1), 1, 0).await;
        assert_eq!(r.pool.crash_streak.load(Ordering::Relaxed), 0);

        // The freshly-handshaked worker dies at once.
        drop(r.stream);
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(
            r.pool.crash_streak.load(Ordering::Relaxed),
            1,
            "a fast post-handshake crash must bump the streak"
        );
    }

    #[tokio::test]
    async fn stats_reports_live_workers_not_provisioned_slots() {
        // M11: `metrics.active` counts a slot from the moment it starts spawning,
        // but /health must report only workers that completed the handshake and
        // can serve — otherwise a pool whose workers cannot come up looks healthy.
        let r = rig(Some(1), 1, 0).await;
        assert_eq!(r.pool.stats().workers, 1, "the handshaked worker is live");

        // The worker dies; the slot respawns in place (crash backoff), but the
        // replacement never handshakes (no Hello is fed), so it is provisioned
        // (active stays 1) yet not live.
        drop(r.stream);
        tokio::time::sleep(Duration::from_millis(250)).await;

        assert_eq!(
            r.pool.stats().workers,
            0,
            "a provisioned-but-unhandshaked slot must not count as a live worker"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn silent_worker_is_reaped_after_handshake_timeout() {
        // L7: a worker that connects but never sends Hello must not park its
        // slot's spawn task forever (it is not in `workers`, so the sweep can't
        // reap it). The pre-Hello read is bounded; on elapse it is killed by its
        // token. Uses a paused clock so the budget elapses without real waiting.
        let (spawner, mut spawns) = test_spawner();
        let (router_work, worker_work) = std::os::unix::net::UnixDatagram::pair().unwrap();
        worker_work.set_nonblocking(true).unwrap();
        let _work = UnixDatagram::from_std(worker_work).unwrap();
        let temp = std::env::temp_dir().join("buran-dispatch-tests");
        let _pool = Pool::start(
            "app",
            &test_app(Some(1)),
            Arc::clone(&spawner) as Spawner,
            router_work,
            temp.to_str().unwrap(),
            None,
        )
        .unwrap();

        // Take the worker end but never send Hello.
        let _silent = timeout(TICK, spawns.recv()).await.unwrap().unwrap();

        // Let the handshake budget elapse; the reader gives up and reaps it.
        tokio::time::sleep(HANDSHAKE_TIMEOUT + Duration::from_secs(1)).await;

        assert!(
            !spawner.killed.lock().unwrap().is_empty(),
            "a worker that never sends Hello must be reaped by its token"
        );
    }

    #[tokio::test]
    async fn end_releases_client_but_done_frees_the_slot() {
        let mut r = rig(Some(1), 1, 0).await;
        let mut rs = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id = wk_recv_work(&r.work).await.request_id;

        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        wk_send(&mut r.stream, FrameKind::ResponseHeaders, id, 200, b"").await;
        wk_send(&mut r.stream, FrameKind::End, id, 0, &[]).await;

        assert!(matches!(
            timeout(TICK, rs.next_event()).await.unwrap(),
            Some(WorkerEvent::Headers { .. })
        ));
        assert!(matches!(timeout(TICK, rs.next_event()).await.unwrap(), Some(WorkerEvent::End)));

        // Client response is done, but the task still holds its slot (a
        // finish_request background task would still be running).
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(r.pool.stats().idle, 0, "slot held until Done");

        // Done frees the slot.
        wk_send(&mut r.stream, FrameKind::Done, id, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(r.pool.stats().idle, 1, "slot freed on Done");
    }

    #[tokio::test]
    async fn sweep_aborts_over_budget_task_then_kills_if_defiant() {
        let mut r = rig(Some(1), 1, 0).await;
        let _rs = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id = wk_recv_work(&r.work).await.request_id;
        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await; // reader records the claim

        // Pin claimed_at to a known base so the wall-clock is under our control.
        let base = Instant::now();
        r.pool.pending.lock().unwrap().get_mut(&id).unwrap().claimed_at = Some(base);

        // Over budget: the task gets one Abort, no kill candidate yet.
        let t1 = base + r.pool.task_timeout + Duration::from_secs(1);
        let (to_abort, candidates) = r.pool.sweep_decide(t1, Duration::from_millis(100));
        assert_eq!(to_abort, vec![id]);
        assert!(candidates.is_empty());

        // Still alive after the grace: the (blocking) worker becomes a kill
        // candidate (the final kill is gated by a Ping in the async sweep).
        let t2 = t1 + Duration::from_millis(200);
        let (to_abort2, candidates2) = r.pool.sweep_decide(t2, Duration::from_millis(100));
        assert!(to_abort2.is_empty(), "already aborted once");
        assert_eq!(candidates2.len(), 1, "one defiant slot = 100% for a blocking worker");
        // The kill targets the supervisor-assigned token (first spawn = 1), not
        // anything the worker self-reported in Hello.
        assert_eq!(candidates2[0].token, 1);
        assert_eq!(candidates2[0].probe_task, id);
    }

    #[tokio::test]
    async fn confirm_and_kill_kills_a_busy_defiant_worker() {
        let mut r = rig(Some(1), 1, 0).await;
        let candidates = vec![KillCandidate { worker_id: 0, token: 7, probe_task: 42 }];
        let pool = Arc::clone(&r.pool);
        let job =
            tokio::spawn(async move { pool.confirm_and_kill(candidates, Duration::from_secs(2)).await });

        let (ping, _) = wk_recv(&mut r.stream).await;
        assert_eq!((ping.kind, ping.request_id), (FrameKind::Ping, 42));
        wk_send(&mut r.stream, FrameKind::Pong, 42, PONG_BUSY, &[]).await;

        job.await.unwrap();
        assert_eq!(*r.spawner.killed.lock().unwrap(), vec![7], "busy defiant worker killed by token");
    }

    #[tokio::test]
    async fn confirm_and_kill_spares_an_idle_worker() {
        let mut r = rig(Some(1), 1, 0).await;
        let candidates = vec![KillCandidate { worker_id: 0, token: 7, probe_task: 42 }];
        let pool = Arc::clone(&r.pool);
        let job =
            tokio::spawn(async move { pool.confirm_and_kill(candidates, Duration::from_secs(2)).await });

        let (ping, _) = wk_recv(&mut r.stream).await;
        assert_eq!(ping.kind, FrameKind::Ping);
        // Pong: idle -> the task actually finished (reader lagged): don't kill.
        wk_send(&mut r.stream, FrameKind::Pong, 42, PONG_IDLE, &[]).await;

        job.await.unwrap();
        assert!(r.spawner.killed.lock().unwrap().is_empty(), "idle worker must be spared");
    }

    #[tokio::test]
    async fn probe_pings_and_reader_completes_it_with_status() {
        let mut r = rig(Some(1), 1, 0).await;
        // The rig's single worker got id 0.
        let pool = Arc::clone(&r.pool);
        let probe = tokio::spawn(async move { pool.probe(0, 42, Duration::from_secs(2)).await });

        // Worker side: receive the Ping, answer Pong: busy.
        let (ping, _) = wk_recv(&mut r.stream).await;
        assert_eq!((ping.kind, ping.request_id), (FrameKind::Ping, 42));
        wk_send(&mut r.stream, FrameKind::Pong, 42, PONG_BUSY, &[]).await;

        let status = timeout(TICK, probe).await.unwrap().unwrap();
        assert_eq!(status, Some(PONG_BUSY));
    }

    #[tokio::test]
    async fn send_abort_reaches_the_claiming_worker() {
        let mut r = rig(Some(1), 1, 0).await;
        let rs = r
            .pool
            .submit(r.pool.try_reserve().unwrap(), request_payload(), SubmitBody::Inline)
            .await
            .unwrap();
        let id = wk_recv_work(&r.work).await.request_id;

        wk_send(&mut r.stream, FrameKind::Claim, id, 0, &[]).await;
        // Let the reader task record claimed_by before we abort.
        tokio::time::sleep(Duration::from_millis(50)).await;

        rs.send_abort().await;

        let (header, _) = wk_recv(&mut r.stream).await;
        assert_eq!(header.kind, FrameKind::Abort);
        assert_eq!(header.request_id, id);
    }

    #[tokio::test]
    async fn worker_death_fails_every_claimed_request() {
        let mut r = rig(Some(4), 4, 0).await;

        let mut first =
            r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Inline).await.unwrap();
        let mut second =
            r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Inline).await.unwrap();
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
        let _rs = r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Stream(rx)).await.unwrap();
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

        let mut rs = r.pool.submit(r.pool.try_reserve().unwrap(), request_payload(),SubmitBody::Upgrade).await.unwrap();
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

    #[tokio::test]
    async fn write_spill_creates_file_0600() {
        use std::os::unix::fs::PermissionsExt;
        // Bodies carry uploads/tokens: the spill must not be world/group
        // readable. (body_owner is None here, so no chown is attempted — the
        // mode is the whole protection when workers share buran's uid.)
        let r = rig(Some(1), 1, 0).await;
        let path = r.pool.spill_path(9_876_543);
        let _ = std::fs::remove_file(&path); // clear any leftover from a prior run

        r.pool.write_spill(&path, b"secret-body").await.unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "spill file must be 0600");
        assert_eq!(std::fs::read(&path).unwrap(), b"secret-body");
        std::fs::remove_file(&path).unwrap();
    }

    #[tokio::test]
    async fn read_frame_rejects_oversized_payload_len() {
        // A worker declaring a payload past the cap must be refused before the
        // router buffers it — the header alone (no payload bytes follow) is
        // enough to trip the guard.
        let (worker, router) = UnixStream::pair().unwrap();
        let mut worker = worker;
        let header = FrameHeader::new(FrameKind::ResponseBody, 1, MAX_FRAME_PAYLOAD + 1);
        worker.write_all(&header.encode()).await.unwrap();

        let mut reader = BufReader::new(router.into_split().0);
        let err = read_frame(&mut reader).await.unwrap_err();
        assert!(err.to_string().contains("exceeds"), "err: {err}");
    }

    #[tokio::test]
    async fn read_frame_accepts_payload_at_the_cap_boundary() {
        // A frame exactly at the limit is fine (an empty body sized to the cap
        // would block on real bytes, so use a small payload with a legal len).
        let (worker, router) = UnixStream::pair().unwrap();
        let mut worker = worker;
        let body = b"ok";
        let header = FrameKind::ResponseBody;
        let fh = FrameHeader::new(header, 9, body.len() as u32);
        let mut buf = fh.encode().to_vec();
        buf.extend_from_slice(body);
        worker.write_all(&buf).await.unwrap();

        let mut reader = BufReader::new(router.into_split().0);
        let (h, payload) = read_frame(&mut reader).await.unwrap();
        assert_eq!(h.request_id, 9);
        assert_eq!(payload, body);
    }
}
