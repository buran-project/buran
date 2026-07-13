//! Buran router: accept loops, HTTP/1.1 handling, route matching, actions.
//!
//! Actions: `return`, `share` (openat2-contained static files), `rewrite`,
//! `route` jumps, and application dispatch over BWP.

mod access_log;
mod dispatch;
mod http1;
mod matching;
mod routes;
mod serve_static;
mod template;
mod uri;
mod ws;

pub use dispatch::{DispatchError, Pool, ResponseStream, Spawner, SubmitBody, WorkerEvent};
pub use routes::CompiledRoutes;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use anyhow::Context;
use buran_config::Validated;
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;
use tracing::info;

/// Shared state reachable from every connection task.
pub struct AppState {
    pub routes: CompiledRoutes,
    pub pools: BTreeMap<String, Arc<Pool>>,
    pub http: buran_config::HttpSettings,
    pub access_log: Option<access_log::AccessLog>,
    /// settings.http.static.mime_types inverted: extension -> mime.
    pub mime_overrides: BTreeMap<String, String>,
    /// Union of module source extensions (lowercase, no dot): never served
    /// as static files unless a share opts in with `serve_sources: true`.
    pub source_exts: std::collections::BTreeSet<String>,
}

pub struct Router {
    state: Arc<AppState>,
    listeners: Vec<(SocketAddr, ListenerKind)>,
}

#[derive(Debug, Clone)]
pub enum ListenerKind {
    Route(String),
    Status,
}

impl Router {
    /// Build routes and start application worker pools. `spawners` provides
    /// a process-spawning closure per application (owned by the supervisor).
    pub fn new(
        validated: &Validated,
        mut spawners: BTreeMap<String, (Spawner, std::os::unix::net::UnixDatagram)>,
        source_exts: std::collections::BTreeSet<String>,
    ) -> anyhow::Result<Self> {
        let routes = routes::compile(validated)?;

        let body_temp = validated.config.settings.http.body_temp_path.clone();
        let mut pools = BTreeMap::new();
        for (name, app) in &validated.applications {
            let (spawner, work) = spawners
                .remove(name)
                .with_context(|| format!("no spawner for application {name}"))?;
            pools.insert(
                name.clone(),
                Pool::start(name, app, spawner, work, &body_temp)
                    .with_context(|| format!("cannot start pool for {name}"))?,
            );
        }

        let access_log = validated
            .config
            .access_log
            .as_deref()
            .map(access_log::AccessLog::open)
            .transpose()
            .context("cannot open access log")?;

        let mut listeners = Vec::new();
        for (addr, l) in &validated.config.listeners {
            let sockaddr = parse_listener_addr(addr)?;
            let kind = match (&l.route, l.status) {
                (Some(route), _) => ListenerKind::Route(route.clone()),
                (None, true) => ListenerKind::Status,
                _ => unreachable!("validated config"),
            };
            listeners.push((sockaddr, kind));
        }

        let http = validated.config.settings.http.clone();
        let mut mime_overrides = BTreeMap::new();
        if let Some(static_) = &http.static_ {
            for (mime, exts) in &static_.mime_types {
                for ext in exts {
                    mime_overrides
                        .insert(ext.trim_start_matches('.').to_ascii_lowercase(), mime.clone());
                }
            }
        }
        Ok(Self {
            state: Arc::new(AppState {
                routes,
                pools,
                http,
                access_log,
                mime_overrides,
                source_exts,
            }),
            listeners,
        })
    }

    /// Bind all listeners and serve until `shutdown` flips to true, then
    /// stop accepting and drain in-flight connections (bounded grace).
    pub async fn serve(
        self,
        shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> anyhow::Result<()> {
        let tracker = Arc::new(ConnTracker {
            count: std::sync::atomic::AtomicUsize::new(0),
            drained: tokio::sync::Notify::new(),
        });
        let mut joins = Vec::new();

        for (addr, kind) in self.listeners {
            let listener = bind_reuseport(addr)
                .with_context(|| format!("cannot bind listener {addr}"))?;
            info!(%addr, ?kind, "listening");
            let state = Arc::clone(&self.state);
            let tracker = Arc::clone(&tracker);
            let shutdown = shutdown.clone();
            joins.push(tokio::spawn(accept_loop(listener, kind, state, tracker, shutdown)));
        }

        for join in joins {
            join.await??;
        }

        // Drain: in-flight connections may hold PHP requests; bounded grace,
        // then exit regardless (workers die with the process anyway).
        const DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(30);
        let deadline = tokio::time::Instant::now() + DRAIN_GRACE;
        while tracker.count.load(std::sync::atomic::Ordering::Acquire) > 0 {
            if tokio::time::timeout_at(deadline, tracker.drained.notified()).await.is_err() {
                let left = tracker.count.load(std::sync::atomic::Ordering::Acquire);
                tracing::warn!(connections = left, "drain grace expired, closing anyway");
                break;
            }
        }
        info!("router drained");
        Ok(())
    }
}

/// Live connection accounting for graceful drain.
struct ConnTracker {
    count: std::sync::atomic::AtomicUsize,
    drained: tokio::sync::Notify,
}

async fn accept_loop(
    listener: TcpListener,
    kind: ListenerKind,
    state: Arc<AppState>,
    tracker: Arc<ConnTracker>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = shutdown.changed() => return Ok(()),
        };
        // Accept errors must never take the listener down: dropping it here
        // turns one transient failure into permanent connection-refused for
        // everyone. Peer-side aborts are free to skip; fd exhaustion means
        // the queue holds connections we cannot take yet, so back off
        // instead of spinning on EMFILE.
        let (stream, peer) = match accepted {
            Ok(pair) => pair,
            Err(e) => {
                let errno = e.raw_os_error();
                if errno == Some(rustix::io::Errno::MFILE.raw_os_error())
                    || errno == Some(rustix::io::Errno::NFILE.raw_os_error())
                {
                    tracing::warn!(error = %e, "accept: out of file descriptors, backing off");
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                } else {
                    tracing::debug!(error = %e, "accept error");
                }
                continue;
            }
        };
        let kind = kind.clone();
        let state = Arc::clone(&state);
        let tracker = Arc::clone(&tracker);
        tracker.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let conn_shutdown = shutdown.clone();
        tokio::spawn(async move {
            if let Err(e) = http1::serve_connection(stream, peer, kind, state, conn_shutdown).await {
                tracing::debug!(%peer, error = %e, "connection closed with error");
            }
            if tracker.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
                tracker.drained.notify_waiters();
            }
        });
    }
}

fn parse_listener_addr(addr: &str) -> anyhow::Result<SocketAddr> {
    let (host, port) = addr.rsplit_once(':').context("host:port expected")?;
    let port: u16 = port.parse()?;
    let ip = if host == "*" { "0.0.0.0".parse()? } else { host.parse()? };
    Ok(SocketAddr::new(ip, port))
}

fn bind_reuseport(addr: SocketAddr) -> anyhow::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    // The kernel caps this at net.core.somaxconn; ask high so the effective
    // backlog is whatever the host allows.
    socket.listen(8192)?;
    Ok(TcpListener::from_std(socket.into())?)
}
