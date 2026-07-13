//! The real PHP worker: BWP loop feeding requests into libphp through the
//! "buran" SAPI (sapi_shim.c). Single-threaded by contract.

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_long, c_void, CString};
use std::io::Read;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::path::PathBuf;

use buran_ipc::{RequestView, FLAG_BODY_FILE};
use buran_worker::{Responder, WorkerError};

use crate::AppConfig;

mod ffi {
    use super::*;

    unsafe extern "C" {
        pub fn bphp_sapi_boot(
            ini_path_override: *const c_char,
            ini_entries: *const c_char,
        ) -> c_int;
        pub fn bphp_sapi_shutdown();
        pub fn bphp_sapi_request(
            filename: *const c_char,
            method: *const c_char,
            request_uri: *const c_char,
            query_string: *const c_char,
            content_type: *const c_char,
            content_length: c_long,
            auth_header: *const c_char,
        ) -> c_int;
        pub fn bphp_register_var(
            track_vars_array: *mut c_void,
            name: *const c_char,
            value: *const c_char,
            value_len: usize,
        );
    }
}

/// Per-request state reachable from the C callbacks. Single worker thread,
/// set strictly for the duration of one `bphp_sapi_request` call.
struct RequestCtx {
    /// Type-erased `&mut Responder<'_>`; valid while the handler runs.
    responder: *mut c_void,
    body: Vec<u8>,
    body_pos: usize,
    /// Large bodies spill to a temp file (FLAG_BODY_FILE): streamed from
    /// here instead of `body`. Unlinked right after open.
    body_file: Option<std::fs::File>,
    cookies: Option<CString>,
    /// $_SERVER arena: names are NUL-terminated in-place, values are
    /// (offset, len) ranges. One buffer, reused across requests.
    vars_arena: Vec<u8>,
    /// (name_off, value_off, value_len) into `vars_arena`.
    vars_entries: Vec<(usize, usize, usize)>,
    /// Response header block under construction.
    resp_status: u16,
    resp_headers: Vec<u8>,
    headers_sent: bool,
    /// Set by fastcgi_finish_request(): the client is gone, swallow output.
    client_released: bool,
    /// Set when a write to the router fails (client hung up mid-stream):
    /// ub_write then reports a short write so PHP aborts the connection and
    /// the worker is freed instead of looping forever (SSE).
    client_gone: bool,
}

thread_local! {
    static CTX: RefCell<Option<RequestCtx>> = const { RefCell::new(None) };
}

fn with_ctx<R>(f: impl FnOnce(&mut RequestCtx) -> R) -> Option<R> {
    CTX.with(|ctx| ctx.borrow_mut().as_mut().map(f))
}

fn responder<'a>(ctx: &mut RequestCtx) -> &'a mut Responder<'static> {
    // Safety: set from the handler right before bphp_sapi_request and
    // cleared right after; callbacks only fire in between.
    unsafe { &mut *(ctx.responder as *mut Responder<'static>) }
}

/// Boot the engine: SAPI registration + module startup (opcache SHM is
/// created here). Once per process — in the prototype, before any fork.
///
/// `options.admin`/`options.user` become pre-startup ini entries: that is
/// the only stage where zend_extensions (opcache) can be loaded. The
/// admin/user distinction (PHP_INI_SYSTEM vs ini_set-able) is phase 2.
pub fn boot(app: &AppConfig) -> Result<(), WorkerError> {
    let ini = app.ini_file.as_deref().map(|p| CString::new(p).unwrap());
    let ini_ptr = ini.as_ref().map_or(std::ptr::null(), |c| c.as_ptr());

    let mut entries = String::new();
    for (name, value) in app.admin.iter().chain(app.user.iter()) {
        entries.push_str(name);
        entries.push('=');
        entries.push_str(value);
        entries.push('\n');
    }
    // The engine keeps the pointer past boot: leak deliberately (lives for
    // the whole process anyway).
    let entries_ptr = if entries.is_empty() {
        std::ptr::null()
    } else {
        let c = CString::new(entries).map_err(|_| WorkerError::Closed)?;
        let ptr = c.as_ptr();
        std::mem::forget(c);
        ptr
    };

    // Safety: single-threaded, once per process.
    if unsafe { ffi::bphp_sapi_boot(ini_ptr, entries_ptr) } != 0 {
        return Err(WorkerError::Closed);
    }
    Ok(())
}

/// Serve requests on an already-booted engine. Forked workers land here;
/// they exit without engine shutdown (FPM practice — the request boundary
/// is `php_request_startup/shutdown`, the process just dies).
pub fn serve(
    work: &UnixDatagram,
    stream: UnixStream,
    app: &AppConfig,
    token: u64,
) -> Result<(), WorkerError> {
    let work = work.try_clone()?;
    buran_worker::run(work, stream, app.max_requests, token, |req, flags, resp| {
        handle(req, flags, resp, app)
    })
}

/// Entry point for standalone `--channel` mode (no prototype, so no token).
pub fn run(work: &UnixDatagram, stream: UnixStream, app: AppConfig) -> Result<(), WorkerError> {
    boot(&app)?;
    let result = serve(work, stream, &app, 0);
    unsafe { ffi::bphp_sapi_shutdown() };
    result
}

fn handle(
    req: &RequestView<'_>,
    flags: u8,
    resp: &mut Responder<'_>,
    app: &AppConfig,
) -> Result<(), WorkerError> {
    let path = req.path()?;

    // php-fpm behavior: a missing script is a plain 404 ("File not found."),
    // the engine is never started. stat only: with opcache a cache hit
    // never opens the file, so pre-opening would waste an fd per request.
    let script = match resolve_script(app, path).and_then(|s| {
        match std::fs::metadata(&s.filename) {
            Ok(m) if m.is_file() => Ok(s),
            Ok(_) => Err(403),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(403),
            Err(_) => Err(404),
        }
    }) {
        Ok(s) => s,
        Err(status) => {
            let reason = match status {
                301 => "Moved Permanently",
                403 => "Forbidden",
                404 => "Not Found",
                _ => "Error",
            };
            let mut headers = String::from("content-type: text/html\r\n");
            if status == 301 {
                // Directory without a trailing slash: absolute redirect
                // built from the Host header, like the reference stack.
                let host = String::from_utf8_lossy(req.server_name()?);
                let path_str = String::from_utf8_lossy(path);
                let query = String::from_utf8_lossy(req.query()?);
                let mut location = if host.is_empty() {
                    format!("{path_str}/")
                } else {
                    format!("http://{host}{path_str}/")
                };
                if !query.is_empty() {
                    location.push('?');
                    location.push_str(&query);
                }
                headers.push_str(&format!("location: {location}\r\n"));
            }
            let body = format!(
                "<html>\n<head><title>{status} {reason}</title></head>\n<body><h1>{status} {reason}</h1></body>\n</html>\n"
            );
            resp.send_headers(status, headers.as_bytes())?;
            resp.send_body(body.as_bytes())?;
            return resp.finish();
        }
    };

    let (vars_arena, vars_entries) = build_server_vars(req, app, &script)?;

    let method = CString::new(req.method()?).unwrap_or_default();
    let uri = CString::new(req.target()?).unwrap_or_default();
    let query = req.query()?;
    let query_c = if query.is_empty() { None } else { CString::new(query).ok() };
    let filename = CString::new(script.filename.as_str()).unwrap_or_default();

    let mut content_type: Option<CString> = None;
    let mut cookies: Option<CString> = None;
    let mut auth: Option<CString> = None;
    for field in req.fields() {
        let field = field?;
        match field.name {
            b"content-type" => content_type = CString::new(field.value).ok(),
            b"cookie" => cookies = CString::new(field.value).ok(),
            b"authorization" => auth = CString::new(field.value).ok(),
            _ => {}
        }
    }

    // Body either inline or spilled to a temp file by the router.
    let (body, body_file) = if flags & FLAG_BODY_FILE != 0 {
        let path = String::from_utf8_lossy(req.preread_body()?).into_owned();
        let file = std::fs::File::open(&path).ok();
        // Unlink immediately: the fd keeps the data, a crash leaks nothing.
        let _ = std::fs::remove_file(&path);
        (Vec::new(), file)
    } else {
        (req.preread_body()?.to_vec(), None)
    };

    let ctx = RequestCtx {
        responder: resp as *mut Responder<'_> as *mut c_void,
        body,
        body_pos: 0,
        body_file,
        cookies,
        vars_arena,
        vars_entries,
        resp_status: 200,
        resp_headers: Vec::with_capacity(256),
        headers_sent: false,
        client_released: false,
        client_gone: false,
    };

    CTX.with(|slot| *slot.borrow_mut() = Some(ctx));

    // Safety: all CStrings above outlive the call; ctx is set.
    let status = unsafe {
        ffi::bphp_sapi_request(
            filename.as_ptr(),
            method.as_ptr(),
            uri.as_ptr(),
            query_c.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            content_type.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            req.content_length() as c_long,
            auth.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
        )
    };

    let leftover = CTX.with(|slot| slot.borrow_mut().take());

    if status < 0 {
        resp.error("php engine failure")?;
        return Ok(());
    }

    // Script produced no output at all: headers were never flushed by PHP.
    if let Some(ctx) = leftover {
        if !ctx.headers_sent {
            resp.send_headers(ctx.resp_status.max(status as u16), &ctx.resp_headers)?;
        }
    }

    resp.finish()
}

struct ResolvedScript {
    filename: String,
    script_name: String,
    path_info: Option<String>,
}

/// Intrinsic executable extensions of this runtime; `app.execute` extends
/// the set (legacy "PHP in .html" deployments).
const INTRINSIC_EXTS: [&str; 2] = [".php", ".phtml"];

/// CGI-like script resolution per spec section 2.5 (matches what frameworks
/// expect from php-fpm).
fn resolve_script(app: &AppConfig, path: &[u8]) -> Result<ResolvedScript, u16> {
    let path = std::str::from_utf8(path).map_err(|_| 400u16)?;

    // Front-controller mode: everything goes to one script.
    if let Some(script) = &app.script {
        return Ok(ResolvedScript {
            filename: format!("{}/{}", app.root.trim_end_matches('/'), script.trim_start_matches('/')),
            script_name: format!("/{}", script.trim_start_matches('/')),
            path_info: None,
        });
    }

    let root = app.root.trim_end_matches('/');
    let executable = |candidate: &str| {
        INTRINSIC_EXTS.iter().any(|ext| candidate.ends_with(ext))
            || app.execute.iter().any(|ext| candidate.ends_with(ext.as_str()))
    };

    // `<script>.<ext>/extra` -> PATH_INFO split.
    for ext in INTRINSIC_EXTS.iter().copied().chain(app.execute.iter().map(String::as_str)) {
        let marker = format!("{ext}/");
        if let Some(pos) = path.find(&marker) {
            let (script_part, extra) = path.split_at(pos + ext.len());
            return Ok(ResolvedScript {
                filename: format!("{root}{script_part}"),
                script_name: script_part.to_string(),
                path_info: Some(extra.to_string()),
            });
        }
    }

    // Trailing slash -> index.
    if path.ends_with('/') {
        let index = app.index.as_deref().unwrap_or("index.php");
        return Ok(ResolvedScript {
            filename: format!("{root}{path}{index}"),
            script_name: format!("{path}{index}"),
            path_info: None,
        });
    }

    if executable(path) {
        return Ok(ResolvedScript {
            filename: format!("{root}{path}"),
            script_name: path.to_string(),
            path_info: None,
        });
    }

    // Not a PHP target: directory -> the router already redirected; here it
    // means 404 (static is served by the router, not the PHP module).
    let full: PathBuf = PathBuf::from(format!("{root}{path}"));
    match std::fs::metadata(&full) {
        Ok(m) if m.is_dir() => Err(301),
        Ok(_) => Err(403),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Err(403),
        Err(_) => Err(404),
    }
}

fn build_server_vars(
    req: &RequestView<'_>,
    app: &AppConfig,
    script: &ResolvedScript,
) -> Result<(Vec<u8>, Vec<(usize, usize, usize)>), WorkerError> {
    // Single arena instead of ~25 CString allocations per request: names
    // NUL-terminated in-place, values addressed by (offset, len).
    let mut arena: Vec<u8> = Vec::with_capacity(1024);
    let mut entries: Vec<(usize, usize, usize)> = Vec::with_capacity(24 + req.fields_count());

    let mut push = |name: &str, value: &[u8]| {
        debug_assert!(!name.as_bytes().contains(&0));
        if value.contains(&0) {
            return; // NUL in a header value: drop rather than truncate
        }
        let name_off = arena.len();
        arena.extend_from_slice(name.as_bytes());
        arena.push(0);
        let value_off = arena.len();
        arena.extend_from_slice(value);
        entries.push((name_off, value_off, value.len()));
    };

    push("SERVER_SOFTWARE", concat!("buran/", env!("CARGO_PKG_VERSION")).as_bytes());
    push("SERVER_PROTOCOL", req.version()?);
    push("REQUEST_METHOD", req.method()?);
    push("REQUEST_URI", req.target()?);
    push("QUERY_STRING", req.query()?);
    push("SCRIPT_FILENAME", script.filename.as_bytes());
    push("SCRIPT_NAME", script.script_name.as_bytes());
    push("DOCUMENT_ROOT", app.root.as_bytes());
    push("DOCUMENT_URI", script.script_name.as_bytes());
    // fpm parity: PHP_SELF includes path info. PATH_INFO mirrors the
    // reference nginx wiring — the dynamic/split location always passes it
    // (empty when none), the front-controller location does not at all.
    let path_info = script.path_info.as_deref().unwrap_or("");
    push("PHP_SELF", format!("{}{path_info}", script.script_name).as_bytes());
    if app.script.is_none() {
        push("PATH_INFO", path_info.as_bytes());
    }
    push("REMOTE_ADDR", req.remote_addr()?);
    push("SERVER_NAME", req.server_name()?);
    push("SERVER_PORT", req.server_port().to_string().as_bytes());
    push("GATEWAY_INTERFACE", b"CGI/1.1");
    // No TLS in v1: termination happens in front of buran (spec 2.2).
    push("REQUEST_SCHEME", b"http");
    push("REDIRECT_STATUS", b"200");

    // fpm parity: CONTENT_LENGTH/CONTENT_TYPE exist even without a body
    // (empty, like nginx's $content_length for a body-less request).
    if req.content_length() > 0 {
        push("CONTENT_LENGTH", req.content_length().to_string().as_bytes());
    } else {
        push("CONTENT_LENGTH", b"");
    }

    let mut content_type_seen = false;
    for field in req.fields() {
        let field = field?;
        if field.name == b"content-type" {
            push("CONTENT_TYPE", field.value);
            content_type_seen = true;
            continue;
        }
        let mut name = String::with_capacity(5 + field.name.len());
        name.push_str("HTTP_");
        for &b in field.name {
            name.push(match b {
                b'-' => '_',
                c => (c as char).to_ascii_uppercase(),
            });
        }
        push(&name, field.value);
    }
    if !content_type_seen {
        push("CONTENT_TYPE", b"");
    }

    Ok((arena, entries))
}

/* --- C callbacks (called from sapi_shim.c) ------------------------------ */

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_ub_write(str_: *const c_char, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let chunk = unsafe { std::slice::from_raw_parts(str_ as *const u8, len) };
    with_ctx(|ctx| {
        if ctx.client_released {
            return len; // fastcgi_finish_request: swallow, keep running
        }
        if ctx.client_gone {
            return 0; // disconnected earlier: keep signalling the abort
        }
        let resp = responder(ctx);
        if resp.send_body(chunk).is_err() {
            // Broken pipe: a short write makes PHP abort the connection so
            // the worker stops instead of looping (SSE with the client gone).
            ctx.client_gone = true;
            return 0;
        }
        len
    })
    .unwrap_or(0)
}

/// PHP flush() / ob_flush(): push buffered output to the client now and keep
/// the response open (SSE, progressive output).
#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_flush() {
    with_ctx(|ctx| {
        if ctx.client_released || ctx.client_gone {
            return;
        }
        // Headers must precede the first flushed body bytes.
        if !ctx.headers_sent {
            let status = ctx.resp_status;
            let headers = std::mem::take(&mut ctx.resp_headers);
            let resp = responder(ctx);
            if resp.send_headers(status, &headers).is_err() {
                ctx.client_gone = true;
                return;
            }
            ctx.headers_sent = true;
        }
        let resp = responder(ctx);
        if resp.flush().is_err() {
            ctx.client_gone = true;
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_headers_begin(status: c_int) {
    with_ctx(|ctx| {
        ctx.resp_status = status.clamp(100, 599) as u16;
        ctx.resp_headers.clear();
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_header_line(line: *const c_char, len: usize) {
    let bytes = unsafe { std::slice::from_raw_parts(line as *const u8, len) };
    with_ctx(|ctx| {
        ctx.resp_headers.extend_from_slice(bytes);
        ctx.resp_headers.extend_from_slice(b"\r\n");
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_headers_end() {
    with_ctx(|ctx| {
        if ctx.client_released {
            return;
        }
        if !ctx.headers_sent {
            let status = ctx.resp_status;
            let headers = std::mem::take(&mut ctx.resp_headers);
            let resp = responder(ctx);
            let _ = resp.send_headers(status, &headers);
            ctx.headers_sent = true;
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_read_post(buffer: *mut c_char, count: usize) -> usize {
    with_ctx(|ctx| {
        if let Some(file) = ctx.body_file.as_mut() {
            // Safety: PHP hands us a writable buffer of at least `count`.
            let out =
                unsafe { std::slice::from_raw_parts_mut(buffer as *mut u8, count) };
            return file.read(out).unwrap_or(0);
        }
        let remaining = &ctx.body[ctx.body_pos.min(ctx.body.len())..];
        let n = remaining.len().min(count);
        unsafe {
            std::ptr::copy_nonoverlapping(remaining.as_ptr(), buffer as *mut u8, n);
        }
        ctx.body_pos += n;
        n
    })
    .unwrap_or(0)
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_cookies() -> *const c_char {
    with_ctx(|ctx| ctx.cookies.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()))
        .unwrap_or(std::ptr::null())
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_register_vars(track_vars_array: *mut c_void) {
    with_ctx(|ctx| {
        let base = ctx.vars_arena.as_ptr();
        for &(name_off, value_off, value_len) in &ctx.vars_entries {
            // Safety: offsets index into vars_arena, names NUL-terminated
            // by construction in build_server_vars.
            unsafe {
                ffi::bphp_register_var(
                    track_vars_array,
                    base.add(name_off) as *const c_char,
                    base.add(value_off) as *const c_char,
                    value_len,
                );
            }
        }
    });
}

/// fastcgi_finish_request(): flush what we have and release the client;
/// the script keeps running, later output is swallowed.
#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_finish_request() {
    with_ctx(|ctx| {
        if ctx.client_released {
            return;
        }
        if !ctx.headers_sent {
            let status = ctx.resp_status;
            let headers = std::mem::take(&mut ctx.resp_headers);
            let resp = responder(ctx);
            let _ = resp.send_headers(status, &headers);
            ctx.headers_sent = true;
        }
        let resp = responder(ctx);
        let _ = resp.finish_now();
        ctx.client_released = true;
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn buran_cb_log(message: *const c_char) {
    let msg = unsafe { std::ffi::CStr::from_ptr(message) };
    eprintln!("php: {}", msg.to_string_lossy());
}
