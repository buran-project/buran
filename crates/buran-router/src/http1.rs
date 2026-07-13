//! HTTP/1.1 connection handling: parse with httparse, route, respond.
//!
//! Current limitations (tracked in the spec phases): chunked request bodies
//! are not supported (411). Request bodies are buffered in memory before
//! dispatch, except for applications whose workers declared
//! CAP_BODY_STREAM — those get the body streamed straight off the socket.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt, BufWriter};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;

use buran_ipc::{RequestBuilder, WS_OP_BINARY, WS_OP_CLOSE, WS_OP_TEXT};

use crate::dispatch::{DispatchError, ResponseStream, SubmitBody, WorkerEvent};
use crate::routes::{Action, Decision, RequestMeta};
use crate::serve_static;
use crate::ws;
use crate::{AppState, ListenerKind};

const MAX_HEADER_BYTES: usize = 32 * 1024;
const MAX_HEADERS: usize = 64;
/// Full server token including the version.
pub(crate) const SERVER_FULL: &str = concat!("buran/", env!("CARGO_PKG_VERSION"));

/// Process-global `Server:` header value, fixed once at router startup from
/// settings.http.server_version. Defaults to the versioned token so unit
/// tests and standalone callers keep the historical behaviour.
static SERVER_HEADER: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();

/// Called once by `Router::new`. `false` suppresses the version, exposing
/// only `buran` in the `Server:` header.
pub(crate) fn init_server_header(with_version: bool) {
    let _ = SERVER_HEADER.set(if with_version { SERVER_FULL } else { "buran" });
}

pub(crate) fn server_header() -> &'static str {
    SERVER_HEADER.get().copied().unwrap_or(SERVER_FULL)
}

/// AsyncWrite passthrough counting wire bytes for the access log.
struct CountingWriter<W> {
    inner: W,
    count: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl<W: AsyncWriteExt + Unpin> tokio::io::AsyncWrite for CountingWriter<W> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let poll = std::pin::Pin::new(&mut self.inner).poll_write(cx, buf);
        if let std::task::Poll::Ready(Ok(n)) = &poll {
            self.count.fetch_add(*n as u64, std::sync::atomic::Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// AsyncWrite passthrough enforcing settings.http.send_timeout: the timer
/// arms while the socket accepts no bytes and disarms on any progress —
/// the budget is between two consecutive writes, not for the whole response.
struct TimedWriter<W> {
    inner: W,
    timeout: Duration,
    deadline: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
}

impl<W> TimedWriter<W> {
    fn new(inner: W, timeout: Duration) -> Self {
        Self { inner, timeout, deadline: None }
    }

    /// Polls the stall timer, arming it on first use. Ready = timed out.
    fn poll_deadline(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Error> {
        use std::future::Future;
        let sleep = self
            .deadline
            .get_or_insert_with(|| Box::pin(tokio::time::sleep(self.timeout)));
        sleep.as_mut().poll(cx).map(|()| std::io::ErrorKind::TimedOut.into())
    }
}

impl<W: AsyncWriteExt + Unpin> tokio::io::AsyncWrite for TimedWriter<W> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let me = self.get_mut();
        match std::pin::Pin::new(&mut me.inner).poll_write(cx, buf) {
            std::task::Poll::Ready(res) => {
                me.deadline = None;
                std::task::Poll::Ready(res)
            }
            std::task::Poll::Pending => me.poll_deadline(cx).map(Err),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let me = self.get_mut();
        match std::pin::Pin::new(&mut me.inner).poll_flush(cx) {
            std::task::Poll::Ready(res) => {
                me.deadline = None;
                std::task::Poll::Ready(res)
            }
            std::task::Poll::Pending => me.poll_deadline(cx).map(Err),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Parsed request parts needed past the httparse borrow.
struct Parsed {
    method: Vec<u8>,
    target: Vec<u8>,
    headers: Vec<(Vec<u8>, Vec<u8>)>,
    keep_alive: bool,
    content_length: u64,
    bad: Option<u16>,
}

pub async fn serve_connection(
    stream: TcpStream,
    peer: SocketAddr,
    kind: ListenerKind,
    state: Arc<AppState>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    stream.set_nodelay(true)?;
    let local_port = stream.local_addr().map(|a| a.port()).unwrap_or(0);
    let (rd, wr) = stream.into_split();
    // A streamed-body dispatch temporarily moves the read half into its
    // feed task and hands it back once the body is fully consumed.
    let mut rd = Some(rd);

    // settings.http (spec 2.8): idle_timeout while waiting for a request on
    // a keep-alive connection, header_read_timeout once bytes arrive,
    // body_read_timeout between body reads, send_timeout between writes.
    let idle_timeout = Duration::from_secs(state.http.idle_timeout);
    let header_timeout = Duration::from_secs(state.http.header_read_timeout);
    let body_timeout = Duration::from_secs(state.http.body_read_timeout);
    let send_timeout = Duration::from_secs(state.http.send_timeout);
    let max_body = state.http.max_body_size;

    let bytes_sent = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut wr = BufWriter::new(CountingWriter {
        inner: TimedWriter::new(wr, send_timeout),
        count: bytes_sent.clone(),
    });
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let remote = peer.ip().to_string();

    loop {
        // Read until end of headers.
        let headers_end = loop {
            if let Some(pos) = find_headers_end(&buf) {
                break pos;
            }
            if buf.len() > MAX_HEADER_BYTES {
                write_simple(&mut wr, 431, "Request Header Fields Too Large", false).await?;
                return Ok(());
            }
            let timeout = if buf.is_empty() { idle_timeout } else { header_timeout };
            let mut chunk = [0u8; 8 * 1024];
            // On shutdown, close idle keep-alive connections right away so
            // the drain does not wait out their idle_timeout.
            let reader = rd.as_mut().expect("read half present between requests");
            let n = tokio::select! {
                read = tokio::time::timeout(timeout, reader.read(&mut chunk)) => match read {
                    Ok(read) => read?,
                    Err(_) => return Ok(()), // idle/slow client, close quietly
                },
                _ = shutdown.changed(), if buf.is_empty() => return Ok(()),
            };
            if n == 0 {
                return Ok(()); // clean close between requests
            }
            buf.extend_from_slice(&chunk[..n]);
        };

        let parsed = match parse_request(&buf[..headers_end]) {
            Some(p) => p,
            None => {
                write_simple(&mut wr, 400, "Bad Request", false).await?;
                return Ok(());
            }
        };
        if let Some(status) = parsed.bad {
            write_simple(&mut wr, status, reason_phrase(status), false).await?;
            return Ok(());
        }

        if parsed.content_length > max_body {
            write_simple(&mut wr, 413, "Content Too Large", false).await?;
            return Ok(());
        }

        buf.drain(..headers_end);
        // buf now holds body bytes that arrived with the headers (and
        // possibly the next pipelined request).

        let (path, query) = split_target(&parsed.target);
        let path = crate::uri::normalize_path(path);
        let query = query.to_vec();
        let keep_alive = parsed.keep_alive;

        // Route first: the decision needs only headers and it determines
        // how the body travels — buffered here (inline/temp-file) or
        // streamed to the worker straight from the socket (pools that
        // declared CAP_BODY_STREAM).
        let host = parsed
            .headers
            .iter()
            .find(|(n, _)| n == b"host")
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let meta = RequestMeta {
            method: &parsed.method,
            host: &host,
            path: &path,
            query: &query,
            headers: &parsed.headers,
            remote: peer.ip(),
        };
        let outcome = match &kind {
            ListenerKind::Route(route) => Some(state.routes.decide(route, &meta)),
            ListenerKind::Status => None,
        };
        let streams = matches!(
            outcome.as_ref().map(|o| &o.decision),
            Some(Decision::Application(name))
                if parsed.content_length > 0
                    // A body that fully arrived with the headers takes the
                    // cheaper inline/spill path even on streaming pools.
                    && (buf.len() as u64) < parsed.content_length
                    && state.pools.get(*name).is_some_and(|p| p.streams_body())
        );

        // Well-formed WebSocket upgrade offer? Whether it becomes one is
        // decided per application: the pool must have CAP_WEBSOCKET,
        // otherwise the request passes through as plain HTTP.
        let ws_key = websocket_upgrade(&parsed);

        let mut body: Vec<u8> = Vec::new();
        if !streams {
            let reader = rd.as_mut().expect("read half present between requests");
            match take_body(reader, &mut buf, parsed.content_length, body_timeout).await {
                Ok(b) => body = b,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                    write_simple(&mut wr, 408, "Request Timeout", false).await?;
                    return Ok(());
                }
                Err(e) => return Err(e.into()),
            }
        }

        let bytes_before = bytes_sent.load(std::sync::atomic::Ordering::Relaxed);
        let mut force_close = false;

        let status: u16 = match (&kind, outcome) {
            (ListenerKind::Status, _) => {
                // /health: liveness probe; anything else: pool metrics.
                let body = if path == b"/health" {
                    "{\"status\":\"ok\"}\n".to_string()
                } else {
                    let mut apps = String::new();
                    for (i, (name, pool)) in state.pools.iter().enumerate() {
                        let s = pool.stats();
                        if i > 0 {
                            apps.push(',');
                        }
                        apps.push_str(&format!(
                            "\"{name}\":{{\"workers\":{},\"idle\":{},\"queued\":{}}}",
                            s.workers, s.idle, s.queued
                        ));
                    }
                    format!("{{\"status\":\"ok\",\"applications\":{{{apps}}}}}\n")
                };
                write_response(&mut wr, 200, "application/json", body.as_bytes(), &[], keep_alive)
                    .await?;
                200
            }
            (ListenerKind::Route(_), Some(outcome)) => {
                // A fired rewrite replaces path+query for the terminal;
                // REQUEST_URI keeps the original target (CGI semantics).
                let (eff_path, eff_query) = match outcome.rewritten {
                    Some((p, q)) => (p, q),
                    None => (path.clone(), query.clone()),
                };
                let extra = &outcome.response_headers;

                match outcome.decision {
                    Decision::Return { status, location } => {
                        write_return(&mut wr, status, location, keep_alive).await?;
                        status
                    }
                    Decision::Share {
                        template,
                        index,
                        types,
                        follow_symlinks,
                        serve_sources,
                        extra_source_exts,
                        fallback,
                    } => {
                        let static_ctx = serve_static::StaticContext {
                            types,
                            mime_overrides: &state.mime_overrides,
                            source_exts: &state.source_exts,
                            extra_source_exts,
                            follow_symlinks,
                            serve_sources,
                            req_headers: &parsed.headers,
                            head_only: parsed.method == b"HEAD",
                            extra_headers: extra,
                            keep_alive,
                        };
                        let served = serve_static::serve(
                            &mut wr, template, index, &eff_path, &static_ctx,
                        )
                        .await?;
                        match served {
                            Some(status) => status,
                            None => match fallback {
                                Some(Action::Return { status, location }) => {
                                    write_return(&mut wr, *status, location.as_deref(), keep_alive)
                                        .await?;
                                    *status
                                }
                                Some(Action::Application { name }) => {
                                    // Share fallbacks always carry a
                                    // buffered body: the miss is only known
                                    // after the static attempt.
                                    let res = dispatch_to_app(
                                        &mut wr, &state, name, &parsed, &eff_path, &eff_query,
                                        BodyPlan::Buffered(&body), false, peer, local_port,
                                        extra, keep_alive,
                                    )
                                    .await?;
                                    match res {
                                        AppRes::Http(st, _) => st,
                                        AppRes::Upgraded(..) => {
                                            unreachable!("no upgrade was offered")
                                        }
                                    }
                                }
                                _ => {
                                    write_simple(&mut wr, 404, "Not Found", keep_alive).await?;
                                    404
                                }
                            },
                        }
                    }
                    Decision::Application(name) => {
                        let name = name.to_string();
                        let upgrade = ws_key.is_some()
                            && state.pools.get(&name).is_some_and(|p| p.supports_websocket());
                        let plan = if streams {
                            BodyPlan::Streamed {
                                initial: std::mem::take(&mut buf),
                                rd: rd.take().expect("read half present"),
                                total: parsed.content_length,
                                read_timeout: body_timeout,
                            }
                        } else {
                            BodyPlan::Buffered(&body)
                        };
                        let res = dispatch_to_app(
                            &mut wr, &state, &name, &parsed, &eff_path, &eff_query, plan,
                            upgrade, peer, local_port, extra, keep_alive,
                        )
                        .await?;
                        match res {
                            AppRes::Http(st, rd_back) => {
                                if streams {
                                    match rd_back {
                                        Some(r) => rd = Some(r),
                                        // Body not fully drained: the connection
                                        // cannot carry another request.
                                        None => force_close = true,
                                    }
                                }
                                st
                            }
                            AppRes::Upgraded(worker, worker_headers) => {
                                let key = ws_key.as_deref().expect("upgrade implies key");
                                write_101(&mut wr, key, &worker_headers, extra).await?;
                                let mut trd = rd.take().expect("read half present");
                                ws_tunnel(
                                    &mut wr,
                                    &mut trd,
                                    &mut buf,
                                    worker,
                                    &state.http.websocket,
                                )
                                .await?;
                                force_close = true;
                                101
                            }
                        }
                    }
                    Decision::NotFound => {
                        write_simple(&mut wr, 404, "Not Found", keep_alive).await?;
                        404
                    }
                }
            }
            (ListenerKind::Route(_), None) => unreachable!("route listeners always decide"),
        };

        if let Some(log) = &state.access_log {
            let header = |name: &[u8]| {
                parsed.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
            };
            let bytes = bytes_sent.load(std::sync::atomic::Ordering::Relaxed) - bytes_before;
            log.log(
                &remote,
                &parsed.method,
                &parsed.target,
                status,
                bytes,
                header(b"referer"),
                header(b"user-agent"),
            );
        }

        if !keep_alive || force_close {
            break;
        }
    }

    wr.flush().await?;
    Ok(())
}

fn parse_request(head: &[u8]) -> Option<Parsed> {
    let mut headers = [httparse::EMPTY_HEADER; MAX_HEADERS];
    let mut req = httparse::Request::new(&mut headers);
    match req.parse(head) {
        Ok(httparse::Status::Complete(_)) => {}
        _ => return None,
    }

    let mut parsed = Parsed {
        method: req.method?.as_bytes().to_vec(),
        target: req.path?.as_bytes().to_vec(),
        headers: Vec::with_capacity(req.headers.len()),
        keep_alive: req.version == Some(1),
        content_length: 0,
        bad: None,
    };

    for h in req.headers.iter() {
        let name = h.name.to_ascii_lowercase().into_bytes();
        match name.as_slice() {
            b"content-length" => {
                parsed.content_length = std::str::from_utf8(h.value)
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(u64::MAX);
                if parsed.content_length == u64::MAX {
                    parsed.bad = Some(400);
                }
            }
            b"transfer-encoding" => {
                // v0: chunked request bodies are not supported yet.
                parsed.bad = Some(411);
            }
            b"connection" => {
                if h.value.eq_ignore_ascii_case(b"close") {
                    parsed.keep_alive = false;
                }
            }
            _ => {}
        }
        parsed.headers.push((name, h.value.to_vec()));
    }

    Some(parsed)
}

async fn read_exact_body(
    rd: &mut OwnedReadHalf,
    body: &mut Vec<u8>,
    content_length: u64,
    read_timeout: Duration,
) -> std::io::Result<()> {
    let mut chunk = [0u8; 16 * 1024];
    while (body.len() as u64) < content_length {
        let want = ((content_length - body.len() as u64) as usize).min(chunk.len());
        // body_read_timeout budgets each read, not the whole body: a slow
        // but moving upload is fine, a stalled one gets cut.
        let n = tokio::time::timeout(read_timeout, rd.read(&mut chunk[..want]))
            .await
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::TimedOut))??;
        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(())
}

/// How the request body reaches the application dispatch.
enum BodyPlan<'a> {
    /// Whole body in memory: inline in the datagram or spilled to a
    /// temp file (FLAG_BODY_FILE).
    Buffered(&'a [u8]),
    /// Streaming pool (CAP_BODY_STREAM): the body flows from the client
    /// socket into RequestBody frames while the worker already runs.
    Streamed {
        /// Body bytes that arrived together with the headers.
        initial: Vec<u8>,
        rd: OwnedReadHalf,
        /// Full body size (content-length).
        total: u64,
        read_timeout: Duration,
    },
}

fn abort_feed(feed: &Option<tokio::task::JoinHandle<(OwnedReadHalf, bool)>>) {
    if let Some(f) = feed {
        f.abort();
    }
}

/// Wait out the feed task — the body must be fully consumed before the
/// connection can carry another request — and recover the read half.
/// None = the connection is poisoned (short body or client stall).
async fn reclaim_rd(
    feed: Option<tokio::task::JoinHandle<(OwnedReadHalf, bool)>>,
) -> Option<OwnedReadHalf> {
    match feed?.await {
        Ok((rd, true)) => Some(rd),
        _ => None,
    }
}

/// Outcome of an application dispatch.
enum AppRes {
    /// Regular HTTP exchange: status for the access log plus the read half
    /// a Streamed plan borrowed (None = connection not reusable).
    Http(u16, Option<OwnedReadHalf>),
    /// The worker accepted a WebSocket upgrade (status 101): the caller
    /// writes the 101 response and drives the tunnel. The second field is
    /// the worker's 101 header block (e.g. sec-websocket-protocol).
    Upgraded(ResponseStream, Vec<u8>),
}

#[allow(clippy::too_many_arguments)]
async fn dispatch_to_app<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    state: &AppState,
    app: &str,
    parsed: &Parsed,
    path: &[u8],
    query: &[u8],
    body: BodyPlan<'_>,
    upgrade: bool,
    peer: SocketAddr,
    local_port: u16,
    extra_headers: &[(&str, Option<&str>)],
    keep_alive: bool,
) -> std::io::Result<AppRes> {
    let Some(pool) = state.pools.get(app) else {
        write_simple(wr, 500, "Internal Server Error", false).await?;
        return Ok(AppRes::Http(500, None));
    };

    let host = parsed
        .headers
        .iter()
        .find(|(n, _)| n == b"host")
        .map(|(_, v)| v.as_slice())
        .unwrap_or(b"");
    let remote = peer.ip().to_string();

    let mut builder = RequestBuilder::new();
    builder
        .method(&parsed.method)
        .path(path)
        .target(&parsed.target)
        .query(query)
        .version(b"HTTP/1.1")
        .remote_addr(remote.as_bytes())
        .server_name(host)
        .server_port(local_port);
    for (name, value) in &parsed.headers {
        builder.field(name, value);
    }

    let mut submit_body = SubmitBody::Inline;
    let mut feed: Option<tokio::task::JoinHandle<(OwnedReadHalf, bool)>> = None;
    match body {
        BodyPlan::Buffered(bytes) => {
            builder.content_length(bytes.len() as u64);
            // Oversized bodies spill to a temp file: datagrams have a size
            // budget (the worker unlinks the file right after opening).
            if bytes.len() > crate::dispatch::INLINE_BODY_LIMIT {
                static SPILL_SEQ: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let seq = SPILL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let spill = pool
                    .body_temp()
                    .join(format!("body-{}-{}", std::process::id(), seq))
                    .to_string_lossy()
                    .into_owned();
                if tokio::fs::write(&spill, bytes).await.is_err() {
                    write_simple(wr, 500, "Internal Server Error", keep_alive).await?;
                    return Ok(AppRes::Http(500, None));
                }
                builder.preread_body(spill.as_bytes());
                submit_body = SubmitBody::File;
            } else {
                builder.preread_body(bytes);
            }
        }
        BodyPlan::Streamed { initial, rd, total, read_timeout } => {
            builder.content_length(total).preread_body(&[]);
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
            submit_body = SubmitBody::Stream(rx);
            let remaining = total - initial.len() as u64;
            // The feed runs concurrently with the response: a worker may
            // answer before it consumed the whole body. Backpressure is the
            // bounded channel; the pump on the other side follows the
            // worker's stream.
            feed = Some(tokio::spawn(async move {
                let mut rd = rd;
                let ok = feed_body(&mut rd, initial, remaining, read_timeout, tx).await;
                (rd, ok)
            }));
        }
    }
    if upgrade {
        // Upgrade offers carry no body by construction (GET, length 0).
        submit_body = SubmitBody::Upgrade;
    }
    let payload = builder.finish();

    // Submit into the kernel work queue; any worker with a free slot picks
    // it up directly, the router is out of the pickup path (spec 2.9).
    let mut worker = match pool.submit(payload, submit_body).await {
        Ok(w) => w,
        Err(DispatchError::WorkerFailed) => {
            abort_feed(&feed);
            write_simple(wr, 502, "Bad Gateway", keep_alive).await?;
            return Ok(AppRes::Http(502, None));
        }
        Err(_saturated) => {
            abort_feed(&feed);
            write_simple(wr, 503, "Service Unavailable", keep_alive).await?;
            return Ok(AppRes::Http(503, None));
        }
    };
    let event_timeout = pool.event_timeout();

    // First event decides the status line (its budget includes queue wait).
    let (status, worker_headers) = match next_event(&mut worker, pool.first_event_timeout()).await
    {
        Some(WorkerEvent::Headers { status, headers }) => (status, headers),
        Some(_) | None => {
            abort_feed(&feed);
            write_simple(wr, 502, "Bad Gateway", keep_alive).await?;
            return Ok(AppRes::Http(502, None));
        }
    };

    if status == 101 {
        if upgrade {
            // The application accepted the offer: the caller takes over.
            return Ok(AppRes::Upgraded(worker, worker_headers));
        }
        // 101 without an upgrade offer is a worker bug.
        abort_feed(&feed);
        write_simple(wr, 502, "Bad Gateway", keep_alive).await?;
        return Ok(AppRes::Http(502, None));
    }

    // Hybrid framing: buffer up to the threshold; a response completing
    // within it goes out with content-length, larger ones stream chunked.
    const STREAM_THRESHOLD: usize = 64 * 1024;
    let mut buffered: Vec<u8> = Vec::new();
    let mut complete = false;
    let mut failed = false;

    while !complete && !failed && buffered.len() <= STREAM_THRESHOLD {
        match next_event(&mut worker, event_timeout).await {
            Some(WorkerEvent::Chunk(chunk)) => buffered.extend_from_slice(&chunk),
            Some(WorkerEvent::End) => {
                complete = true;
            }
            Some(WorkerEvent::Headers { .. } | WorkerEvent::Ws { .. }) => {}
            Some(WorkerEvent::Failed(_)) | None => {
                failed = true;
            }
        }
    }

    let mut head =
        format!("HTTP/1.1 {} {}\r\nserver: {}\r\n", status, reason_phrase(status), server_header());
    // Worker headers come as `name: value\r\n` lines; hop-by-hop and
    // framing headers are ours to control. response_headers ops: a None
    // value removes the worker's header, Some appends/overrides.
    let removed = |name: &[u8]| {
        // Any op naming this header strips the worker's version: removal
        // drops it, override replaces it via the append below.
        extra_headers.iter().any(|(n, _)| n.as_bytes().eq_ignore_ascii_case(name))
    };
    for line in worker_headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || framing_header(line) {
            continue;
        }
        let name_end = memchr::memchr(b':', line).unwrap_or(line.len());
        if removed(&line[..name_end]) {
            continue;
        }
        head.push_str(&String::from_utf8_lossy(line));
        head.push_str("\r\n");
    }
    for (name, value) in extra_headers {
        if let Some(value) = value {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if !keep_alive {
        head.push_str("connection: close\r\n");
    }

    if complete || failed {
        // The worker freed itself the moment it wrote End (self-service
        // queue); nothing to release here.
        drop(worker);
        // Small/complete (or truncated-before-threshold) response: exact
        // content-length. A failure this early degrades to a short body.
        head.push_str(&format!("content-length: {}\r\n\r\n", buffered.len()));
        wr.write_all(head.as_bytes()).await?;
        wr.write_all(&buffered).await?;
        wr.flush().await?;
        if failed {
            abort_feed(&feed);
            // The response is likely incomplete: poison keep-alive.
            return Err(std::io::Error::other("worker failed mid-response"));
        }
        return Ok(AppRes::Http(status, reclaim_rd(feed).await));
    }

    // Streaming path.
    head.push_str("transfer-encoding: chunked\r\n\r\n");
    wr.write_all(head.as_bytes()).await?;
    write_chunk(wr, &buffered).await?;
    drop(buffered);

    loop {
        match next_event(&mut worker, event_timeout).await {
            Some(WorkerEvent::Chunk(chunk)) => write_chunk(wr, &chunk).await?,
            Some(WorkerEvent::End) => {
                wr.write_all(b"0\r\n\r\n").await?;
                wr.flush().await?;
                drop(worker);
                return Ok(AppRes::Http(status, reclaim_rd(feed).await));
            }
            Some(WorkerEvent::Headers { .. } | WorkerEvent::Ws { .. }) => {}
            Some(WorkerEvent::Failed(_)) | None => {
                // Mid-stream failure: truncate; the missing final chunk
                // tells the client the response is broken.
                wr.flush().await?;
                abort_feed(&feed);
                return Err(std::io::Error::other("worker failed mid-stream"));
            }
        }
    }
}

/// One event with the limits.timeout budget; None = stall. The request is
/// abandoned and counted against the claiming worker's health — a worker
/// with every granted slot stuck gets killed by the pool.
async fn next_event(
    worker: &mut crate::dispatch::ResponseStream,
    timeout: Duration,
) -> Option<WorkerEvent> {
    match tokio::time::timeout(timeout, worker.next_event()).await {
        Ok(ev) => ev,
        Err(_) => {
            worker.mark_stuck();
            None
        }
    }
}

/// `Sec-WebSocket-Key` if this request is a well-formed WebSocket upgrade
/// offer (RFC 6455 section 4.2.1). Anything else — wrong method, a body,
/// missing headers, foreign version — is served as plain HTTP and the
/// application answers what it can.
fn websocket_upgrade(parsed: &Parsed) -> Option<Vec<u8>> {
    if parsed.method != b"GET" || parsed.content_length != 0 {
        return None;
    }
    let header = |name: &[u8]| {
        parsed.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    };
    if !header(b"upgrade")?.eq_ignore_ascii_case(b"websocket") {
        return None;
    }
    // `connection` is a comma-separated token list ("keep-alive, Upgrade").
    let has_upgrade_token = header(b"connection")?
        .split(|&b| b == b',')
        .any(|t| t.trim_ascii().eq_ignore_ascii_case(b"upgrade"));
    if !has_upgrade_token || header(b"sec-websocket-version")? != b"13".as_slice() {
        return None;
    }
    header(b"sec-websocket-key").map(|k| k.to_vec())
}

/// The 101 response: handshake crypto is ours, subprotocol and friends
/// come from the worker's header block.
async fn write_101<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    key: &[u8],
    worker_headers: &[u8],
    extra_headers: &[(&str, Option<&str>)],
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 101 Switching Protocols\r\nserver: {}\r\nupgrade: websocket\r\nconnection: upgrade\r\nsec-websocket-accept: {}\r\n",
        server_header(),
        ws::accept_key(key),
    );
    for line in worker_headers.split(|&b| b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if line.is_empty() || framing_header(line) || handshake_header(line) {
            continue;
        }
        head.push_str(&String::from_utf8_lossy(line));
        head.push_str("\r\n");
    }
    for (name, value) in extra_headers {
        if let Some(value) = value {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    head.push_str("\r\n");
    wr.write_all(head.as_bytes()).await?;
    wr.flush().await
}

/// Headers of the 101 response the router owns; the worker's copies are
/// dropped to avoid duplicates.
fn handshake_header(line: &[u8]) -> bool {
    let name = &line[..memchr::memchr(b':', line).unwrap_or(line.len())];
    name.eq_ignore_ascii_case(b"upgrade") || name.eq_ignore_ascii_case(b"sec-websocket-accept")
}

/// The tunnel: client bytes are decoded into whole messages for the
/// worker, worker WsMessage events are framed back. Ping/pong and the
/// closing handshake never leave the router. Returns when the tunnel is
/// down; the connection always closes after.
async fn ws_tunnel<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    mut worker: ResponseStream,
    cfg: &buran_config::WebsocketSettings,
) -> std::io::Result<()> {
    let idle = Duration::from_secs(cfg.idle_timeout);
    let mut dec = ws::Decoder::new(cfg.max_message_size as usize);
    let mut chunk = [0u8; 8 * 1024];

    loop {
        // Drain complete messages already buffered.
        loop {
            match dec.next(buf) {
                Ok(Some(msg)) => match msg {
                    ws::Message::Text(p) => {
                        if worker.send_ws(WS_OP_TEXT, &p).await.is_err() {
                            return close_client(wr, ws::CLOSE_INTERNAL, "").await;
                        }
                    }
                    ws::Message::Binary(p) => {
                        if worker.send_ws(WS_OP_BINARY, &p).await.is_err() {
                            return close_client(wr, ws::CLOSE_INTERNAL, "").await;
                        }
                    }
                    ws::Message::Ping(p) => {
                        wr.write_all(&ws::encode_frame(ws::OP_PONG, &p)).await?;
                        wr.flush().await?;
                    }
                    ws::Message::Pong(_) => {}
                    ws::Message::Close(p) => {
                        // Client-initiated close: the worker learns via
                        // WS_OP_CLOSE and must End; we echo and are done.
                        let _ = worker.send_ws(WS_OP_CLOSE, &p).await;
                        wr.write_all(&ws::encode_frame(ws::OP_CLOSE, &p)).await?;
                        return wr.flush().await;
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    let _ =
                        worker.send_ws(WS_OP_CLOSE, &e.close_code().to_be_bytes()).await;
                    return close_client(wr, e.close_code(), "").await;
                }
            }
        }

        tokio::select! {
            read = rd.read(&mut chunk) => match read {
                Ok(n) if n > 0 => buf.extend_from_slice(&chunk[..n]),
                // Client vanished without a close frame.
                _ => {
                    let _ = worker
                        .send_ws(WS_OP_CLOSE, &ws::CLOSE_GOING_AWAY.to_be_bytes())
                        .await;
                    return Ok(());
                }
            },
            ev = worker.next_event() => match ev {
                Some(WorkerEvent::Ws { opcode: WS_OP_TEXT, payload }) => {
                    wr.write_all(&ws::encode_frame(ws::OP_TEXT, &payload)).await?;
                    wr.flush().await?;
                }
                Some(WorkerEvent::Ws { opcode: WS_OP_BINARY, payload }) => {
                    wr.write_all(&ws::encode_frame(ws::OP_BINARY, &payload)).await?;
                    wr.flush().await?;
                }
                Some(WorkerEvent::Ws { opcode: WS_OP_CLOSE, payload }) => {
                    // Worker-initiated close with an explicit code.
                    wr.write_all(&ws::encode_frame(ws::OP_CLOSE, &payload)).await?;
                    return wr.flush().await;
                }
                Some(WorkerEvent::Ws { .. }) => {} // unknown opcode: drop
                Some(WorkerEvent::End) | None => {
                    return close_client(wr, ws::CLOSE_NORMAL, "").await;
                }
                Some(WorkerEvent::Failed(_)) => {
                    return close_client(wr, ws::CLOSE_INTERNAL, "").await;
                }
                Some(_) => {} // stray Headers/Chunk after 101: drop
            },
            // Recreated every iteration: any traffic in either direction
            // resets the idle budget.
            _ = tokio::time::sleep(idle) => {
                let _ = worker
                    .send_ws(WS_OP_CLOSE, &ws::CLOSE_GOING_AWAY.to_be_bytes())
                    .await;
                return close_client(wr, ws::CLOSE_GOING_AWAY, "idle timeout").await;
            }
        }
    }
}

async fn close_client<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    code: u16,
    reason: &str,
) -> std::io::Result<()> {
    wr.write_all(&ws::encode_close(code, reason)).await?;
    wr.flush().await
}

/// Consume exactly `content_length` body bytes: from what already sits in
/// `buf`, then from the socket. Bytes past the body (pipelining) stay in
/// `buf` for the next request.
async fn take_body(
    rd: &mut OwnedReadHalf,
    buf: &mut Vec<u8>,
    content_length: u64,
    read_timeout: Duration,
) -> std::io::Result<Vec<u8>> {
    let mut body = std::mem::take(buf);
    if body.len() as u64 > content_length {
        *buf = body.split_off(content_length as usize);
        return Ok(body);
    }
    read_exact_body(rd, &mut body, content_length, read_timeout).await?;
    Ok(body)
}

/// Feed a streamed request body from the socket into the dispatch channel.
/// True = exactly content_length bytes delivered; false poisons the
/// connection (client stall/EOF, or the request died with body bytes still
/// unread on the socket).
async fn feed_body(
    rd: &mut OwnedReadHalf,
    initial: Vec<u8>,
    mut remaining: u64,
    read_timeout: Duration,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> bool {
    if !initial.is_empty() && tx.send(initial).await.is_err() {
        return remaining == 0;
    }
    let mut chunk = [0u8; 16 * 1024];
    while remaining > 0 {
        let want = (remaining as usize).min(chunk.len());
        // read_timeout budgets each read, not the whole body: a slow but
        // moving upload is fine, a stalled one gets cut.
        let n = match tokio::time::timeout(read_timeout, rd.read(&mut chunk[..want])).await {
            Ok(Ok(n)) if n > 0 => n,
            _ => return false, // stall, clean EOF mid-body, or error
        };
        remaining -= n as u64;
        if tx.send(chunk[..n].to_vec()).await.is_err() {
            return false;
        }
    }
    true
}

async fn write_chunk<W: AsyncWriteExt + Unpin>(wr: &mut W, data: &[u8]) -> std::io::Result<()> {
    if data.is_empty() {
        return Ok(());
    }
    wr.write_all(format!("{:x}\r\n", data.len()).as_bytes()).await?;
    wr.write_all(data).await?;
    wr.write_all(b"\r\n").await
}

fn framing_header(line: &[u8]) -> bool {
    let name_end = memchr::memchr(b':', line).unwrap_or(line.len());
    let name = &line[..name_end];
    name.eq_ignore_ascii_case(b"content-length")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"connection")
}

fn find_headers_end(buf: &[u8]) -> Option<usize> {
    memchr::memmem::find(buf, b"\r\n\r\n").map(|pos| pos + 4)
}

fn split_target(target: &[u8]) -> (&[u8], &[u8]) {
    match memchr::memchr(b'?', target) {
        Some(pos) => (&target[..pos], &target[pos + 1..]),
        None => (target, &[][..]),
    }
}

pub async fn write_return<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    status: u16,
    location: Option<&str>,
    keep_alive: bool,
) -> std::io::Result<()> {
    let reason = reason_phrase(status);
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nserver: {}\r\ncontent-length: 0\r\n",
        server_header(),
    );
    if let Some(loc) = location {
        head.push_str(&format!("location: {loc}\r\n"));
    }
    head.push_str(if keep_alive { "\r\n" } else { "connection: close\r\n\r\n" });
    wr.write_all(head.as_bytes()).await?;
    wr.flush().await
}

pub async fn write_simple<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    status: u16,
    reason: &str,
    keep_alive: bool,
) -> std::io::Result<()> {
    let body = format!("{status} {reason}\n");
    write_response(wr, status, "text/plain", body.as_bytes(), &[], keep_alive).await
}

pub async fn write_response<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &[(&str, Option<&str>)],
    keep_alive: bool,
) -> std::io::Result<()> {
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nserver: {server}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\n",
        body.len(),
        reason = reason_phrase(status),
        server = server_header(),
    );
    for (name, value) in extra_headers {
        if let Some(value) = value {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if !keep_alive {
        head.push_str("connection: close\r\n");
    }
    head.push_str("\r\n");
    wr.write_all(head.as_bytes()).await?;
    wr.write_all(body).await?;
    wr.flush().await
}

pub fn reason_phrase(status: u16) -> &'static str {
    match status {
        101 => "Switching Protocols",
        200 => "OK",
        301 => "Moved Permanently",
        302 => "Found",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        411 => "Length Required",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head_of(resp: &[u8]) -> String {
        let end = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        String::from_utf8_lossy(&resp[..end]).into_owned()
    }

    #[test]
    fn parse_minimal_request() {
        let raw = b"GET /path?x=1 HTTP/1.1\r\nHost: example.test\r\n\r\n";
        let p = parse_request(raw).unwrap();
        assert_eq!(p.method, b"GET");
        assert_eq!(p.target, b"/path?x=1");
        assert!(p.keep_alive); // HTTP/1.1 defaults to keep-alive
        assert_eq!(p.content_length, 0);
        assert!(p.bad.is_none());
        // Header names are lowercased.
        assert_eq!(p.headers[0], (b"host".to_vec(), b"example.test".to_vec()));
    }

    #[test]
    fn parse_reads_content_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: 42\r\n\r\n";
        assert_eq!(parse_request(raw).unwrap().content_length, 42);
    }

    #[test]
    fn parse_flags_bad_content_length() {
        let raw = b"POST / HTTP/1.1\r\nContent-Length: notanumber\r\n\r\n";
        assert_eq!(parse_request(raw).unwrap().bad, Some(400));
    }

    #[test]
    fn parse_rejects_chunked_body() {
        let raw = b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n";
        assert_eq!(parse_request(raw).unwrap().bad, Some(411));
    }

    #[test]
    fn parse_honors_connection_close() {
        let raw = b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n";
        assert!(!parse_request(raw).unwrap().keep_alive);
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_request(b"not a request\r\n\r\n").is_none());
    }

    #[test]
    fn split_target_variants() {
        assert_eq!(split_target(b"/p?x=1"), (&b"/p"[..], &b"x=1"[..]));
        assert_eq!(split_target(b"/p"), (&b"/p"[..], &b""[..]));
        assert_eq!(split_target(b"/p?"), (&b"/p"[..], &b""[..]));
    }

    #[test]
    fn find_headers_end_locates_terminator() {
        assert_eq!(find_headers_end(b"GET / HTTP/1.1\r\n\r\nbody"), Some(18));
        assert_eq!(find_headers_end(b"incomplete\r\n"), None);
    }

    #[test]
    fn framing_headers_are_recognized() {
        assert!(framing_header(b"content-length: 5"));
        assert!(framing_header(b"Transfer-Encoding: chunked"));
        assert!(framing_header(b"CONNECTION: close"));
        assert!(!framing_header(b"content-type: text/html"));
        assert!(!framing_header(b"x-custom: 1"));
    }

    #[test]
    fn reason_phrases() {
        assert_eq!(reason_phrase(200), "OK");
        assert_eq!(reason_phrase(404), "Not Found");
        assert_eq!(reason_phrase(599), ""); // unknown codes have no phrase
    }

    #[tokio::test]
    async fn write_return_redirect() {
        let mut out = Vec::new();
        write_return(&mut out, 301, Some("/new"), true).await.unwrap();
        let head = head_of(&out);
        assert!(head.starts_with("HTTP/1.1 301 Moved Permanently"));
        assert!(head.contains("location: /new"));
        assert!(head.contains("content-length: 0"));
        assert!(!head.contains("connection: close"));
    }

    #[tokio::test]
    async fn write_return_closes_when_not_keep_alive() {
        let mut out = Vec::new();
        write_return(&mut out, 404, None, false).await.unwrap();
        assert!(head_of(&out).contains("connection: close"));
    }

    #[tokio::test]
    async fn write_response_has_body_and_length() {
        let mut out = Vec::new();
        write_response(&mut out, 200, "application/json", b"{}", &[], true).await.unwrap();
        let head = head_of(&out);
        assert!(head.contains("content-type: application/json"));
        assert!(head.contains("content-length: 2"));
        assert!(out.ends_with(b"{}"));
    }

    #[tokio::test]
    async fn write_response_appends_extra_headers() {
        let mut out = Vec::new();
        let extra = [("x-a", Some("1")), ("x-b", None)];
        write_response(&mut out, 200, "text/plain", b"", &extra, true).await.unwrap();
        let head = head_of(&out);
        assert!(head.contains("x-a: 1"));
        assert!(!head.contains("x-b")); // None value is skipped
    }

    #[tokio::test]
    async fn write_simple_wraps_response() {
        let mut out = Vec::new();
        write_simple(&mut out, 404, "Not Found", true).await.unwrap();
        assert!(head_of(&out).starts_with("HTTP/1.1 404 Not Found"));
        assert!(out.ends_with(b"404 Not Found\n"));
    }

    #[tokio::test]
    async fn write_chunk_frames_data() {
        let mut out = Vec::new();
        write_chunk(&mut out, b"hello").await.unwrap();
        assert_eq!(out, b"5\r\nhello\r\n");

        // Empty chunk writes nothing (the terminator is written by the caller).
        let mut empty = Vec::new();
        write_chunk(&mut empty, b"").await.unwrap();
        assert!(empty.is_empty());
    }

    fn upgrade_request(extra: &str) -> Parsed {
        let raw = format!(
            "GET /ws HTTP/1.1\r\nHost: h\r\nUpgrade: websocket\r\n\
             Connection: keep-alive, Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZQ==\r\n\
             Sec-WebSocket-Version: 13\r\n{extra}\r\n"
        );
        parse_request(raw.as_bytes()).unwrap()
    }

    #[test]
    fn websocket_upgrade_detects_valid_offer() {
        let key = websocket_upgrade(&upgrade_request("")).expect("valid offer");
        assert_eq!(key, b"dGhlIHNhbXBsZQ==");
    }

    #[test]
    fn websocket_upgrade_rejects_malformed_offers() {
        // Wrong method.
        let raw = b"POST /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\n\r\n";
        assert!(websocket_upgrade(&parse_request(raw).unwrap()).is_none());

        // A body has no place in an upgrade offer.
        let mut with_body = upgrade_request("");
        with_body.content_length = 5;
        assert!(websocket_upgrade(&with_body).is_none());

        // Foreign version: pass through as plain HTTP.
        let raw = b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Key: k\r\nSec-WebSocket-Version: 8\r\n\r\n";
        assert!(websocket_upgrade(&parse_request(raw).unwrap()).is_none());

        // `connection` without the upgrade token.
        let raw = b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: keep-alive\r\n\
Sec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\n\r\n";
        assert!(websocket_upgrade(&parse_request(raw).unwrap()).is_none());

        // Upgrade to something else entirely.
        let raw = b"GET /ws HTTP/1.1\r\nUpgrade: h2c\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Key: k\r\nSec-WebSocket-Version: 13\r\n\r\n";
        assert!(websocket_upgrade(&parse_request(raw).unwrap()).is_none());

        // No key, no deal.
        let raw = b"GET /ws HTTP/1.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
Sec-WebSocket-Version: 13\r\n\r\n";
        assert!(websocket_upgrade(&parse_request(raw).unwrap()).is_none());
    }

    /// Loopback TCP pair: (client write end, server read half).
    async fn tcp_pair() -> (TcpStream, OwnedReadHalf) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (server, client) =
            tokio::join!(listener.accept(), tokio::net::TcpStream::connect(addr));
        let (rd, _wr) = server.unwrap().0.into_split();
        // The write half is dropped: reads on `rd` still work, the client
        // just never receives anything back.
        (client.unwrap(), rd)
    }

    #[tokio::test]
    async fn take_body_splits_pipelined_bytes() {
        let (_client, mut rd) = tcp_pair().await;
        let mut buf = b"bodyNEXT REQUEST".to_vec();
        let body = take_body(&mut rd, &mut buf, 4, Duration::from_secs(1)).await.unwrap();
        assert_eq!(body, b"body");
        assert_eq!(buf, b"NEXT REQUEST");
    }

    #[tokio::test]
    async fn take_body_reads_remainder_from_socket() {
        let (client, mut rd) = tcp_pair().await;
        let mut buf = b"par".to_vec();
        let write = async {
            let mut client = client;
            client.write_all(b"tial").await.unwrap();
            client
        };
        let read = take_body(&mut rd, &mut buf, 7, Duration::from_secs(5));
        let (_client, body) = tokio::join!(write, read);
        assert_eq!(body.unwrap(), b"partial");
        assert!(buf.is_empty());
    }

    #[tokio::test]
    async fn feed_body_delivers_initial_and_socket_bytes() {
        let (client, mut rd) = tcp_pair().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);

        let write = async {
            let mut client = client;
            client.write_all(b"-from-socket").await.unwrap();
            client
        };
        let feed = feed_body(&mut rd, b"initial".to_vec(), 12, Duration::from_secs(5), tx);
        let (ok, _client) = tokio::join!(feed, write);
        assert!(ok, "body fully delivered");

        let mut got = Vec::new();
        while let Some(chunk) = rx.recv().await {
            got.extend_from_slice(&chunk);
        }
        assert_eq!(got, b"initial-from-socket");
    }

    #[tokio::test]
    async fn feed_body_reports_client_eof() {
        let (client, mut rd) = tcp_pair().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        drop(client); // EOF with 10 bytes still owed
        let ok = feed_body(&mut rd, Vec::new(), 10, Duration::from_secs(5), tx).await;
        assert!(!ok, "short body must poison the connection");
        assert!(rx.recv().await.is_none());
    }
}
