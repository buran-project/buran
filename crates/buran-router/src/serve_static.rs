//! Static file serving with kernel-level containment.
//!
//! The template is split at `$uri` into a literal directory prefix (the share
//! root) and the rest; the file is opened via openat2(RESOLVE_IN_ROOT) confined
//! to that prefix, so symlink/`..` escapes are impossible by construction (spec
//! 2.6) for every template shape — including non-suffix ones like
//! `/srv/data$uri.bak`, which jail to `/srv/data`, not the whole filesystem.
//!
//! Conditional requests: strong ETag (size+mtime) with If-None-Match, and
//! Last-Modified with exact-match If-Modified-Since (the header we sent,
//! echoed back — the common browser case; full date parsing is phase 3).
//! Range: single `bytes=` range with If-Range (ETag form). Bodies stream in
//! chunks; sendfile is a later optimization behind the same interface.

use std::collections::BTreeMap;

use buran_config::ServeSources;
use rustix::fs::{Mode, OFlags, ResolveFlags};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::http1::{server_header, write_return};
use crate::matching::PatternSet;

const COPY_CHUNK: usize = 64 * 1024;

pub struct StaticContext<'a> {
    pub types: Option<&'a PatternSet>,
    pub mime_overrides: &'a BTreeMap<String, String>,
    /// Module source extensions: refused unless `serve_sources` opts in
    /// (all, or the specific extension).
    pub source_exts: &'a std::collections::BTreeSet<String>,
    /// Per-share extras (fallback application's `execute` list).
    pub extra_source_exts: &'a [String],
    /// Follow symlinks during resolution; when false, openat2 refuses any
    /// path component that is a symlink.
    pub follow_symlinks: bool,
    pub serve_sources: &'a ServeSources,
    pub req_headers: &'a [(Vec<u8>, Vec<u8>)],
    pub head_only: bool,
    pub extra_headers: &'a [(&'a str, Option<&'a str>)],
    pub keep_alive: bool,
}

/// Serve a file resolved from `template` (supports `$uri`). Returns the
/// response status, or `None` if the file was not served (caller applies
/// the fallback action).
pub async fn serve<W: AsyncWriteExt + Unpin>(
    wr: &mut W,
    template: &str,
    index: &str,
    path: &[u8],
    ctx: &StaticContext<'_>,
) -> anyhow::Result<Option<u16>> {
    let uri = String::from_utf8_lossy(path);

    // Split the template at `$uri` into a literal directory prefix (the share
    // root) and the rest. openat2(IN_ROOT) then confines resolution to that
    // prefix for EVERY template shape, not just `/base$uri`: a non-suffix
    // template like `/srv/data$uri.bak` still jails to `/srv/data` instead of
    // the whole filesystem. `$uri` is normalized (no `..`), so the request can
    // only append path segments within the prefix.
    let (base, mut target) = match template.split_once("$uri") {
        Some((prefix, suffix)) => {
            (prefix.to_string(), format!("{uri}{}", suffix.replace("$uri", &uri)))
        }
        // No `$uri`: a fixed path served from the filesystem root.
        None => (String::from("/"), template.to_string()),
    };

    if target.ends_with('/') {
        target.push_str(index);
    }

    // Source-leak protection: extensions the runtime modules declared as
    // executable sources are never static, no matter what the share says,
    // unless `serve_sources` opts that extension in (all, or a named list).
    // Case-insensitive.
    // Trim trailing dots/spaces before extracting the extension: on a
    // case-/dot-insensitive mount `app.php.` opens `app.php`, so the ext check
    // must see `php`, not the empty string a naive rsplit('.') would yield.
    let ext = target
        .trim_end_matches(['.', ' '])
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let is_source =
        ctx.source_exts.contains(&ext) || ctx.extra_source_exts.iter().any(|e| e == &ext);
    if is_source {
        let opted_in = match ctx.serve_sources {
            ServeSources::None => false,
            ServeSources::All => true,
            ServeSources::Only(exts) => {
                exts.iter().any(|e| e.trim_start_matches('.').eq_ignore_ascii_case(&ext))
            }
        };
        if !opted_in {
            return Ok(None);
        }
    }

    let mime = mime_for(&target, ctx.mime_overrides);

    // MIME filter: a non-matching type is "not served" -> fallback.
    if let Some(types) = ctx.types
        && !types.matches(mime.as_bytes(), true) {
            return Ok(None);
        }

    let rel = target.trim_start_matches('/').to_string();
    if rel.is_empty() {
        return Ok(None);
    }

    // IN_ROOT confines resolution to the share root; NO_SYMLINKS additionally
    // refuses any symlink component when `follow_symlinks: false`.
    let mut resolve = ResolveFlags::IN_ROOT;
    if !ctx.follow_symlinks {
        resolve |= ResolveFlags::NO_SYMLINKS;
    }

    // Blocking syscalls (fast): open base, then contained open of the file.
    let opened = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let dir = rustix::fs::open(
            base.as_str(),
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let fd = rustix::fs::openat2(
            &dir,
            rel.as_str(),
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
            resolve,
        )?;
        Ok(std::fs::File::from(fd))
    })
    .await?;

    let file = match opened {
        Ok(f) => f,
        Err(_) => return Ok(None), // missing/denied/escape attempt -> fallback
    };

    let meta = file.metadata()?;
    if meta.is_dir() {
        // Directory without a trailing slash: external redirect, so
        // relative links inside the index document resolve correctly.
        let location = format!("{uri}/");
        write_return(wr, 301, Some(&location), ctx.keep_alive).await?;
        return Ok(Some(301));
    }

    let size = meta.len();
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let etag = format!("\"{mtime:x}-{size:x}\"");
    let last_modified = crate::uri::http_date(mtime);

    let req = |name: &[u8]| {
        ctx.req_headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_slice())
    };

    // Conditional GET: If-None-Match wins over If-Modified-Since (RFC 9110).
    let not_modified = match req(b"if-none-match") {
        Some(inm) => etag_listed(inm, etag.as_bytes()),
        None => req(b"if-modified-since").is_some_and(|v| v == last_modified.as_bytes()),
    };
    if not_modified {
        let head = format!(
            "HTTP/1.1 304 Not Modified\r\nserver: {}\r\netag: {etag}\r\nlast-modified: {last_modified}\r\n{}\r\n",
            server_header(),
            if ctx.keep_alive { "" } else { "connection: close\r\n" },
        );
        wr.write_all(head.as_bytes()).await?;
        wr.flush().await?;
        return Ok(Some(304));
    }

    // Range: honored unless If-Range names a different entity.
    let range_allowed = match req(b"if-range") {
        Some(ir) => ir == etag.as_bytes(),
        None => true,
    };
    let range = if range_allowed {
        match req(b"range").map(|r| parse_range(r, size)) {
            Some(Ok(r)) => r,
            Some(Err(())) => {
                let head = format!(
                    "HTTP/1.1 416 Range Not Satisfiable\r\nserver: {}\r\ncontent-range: bytes */{size}\r\ncontent-length: 0\r\n{}\r\n",
                    server_header(),
                    if ctx.keep_alive { "" } else { "connection: close\r\n" },
                );
                wr.write_all(head.as_bytes()).await?;
                wr.flush().await?;
                return Ok(Some(416));
            }
            None => None,
        }
    } else {
        None
    };

    let (status, offset, length) = match range {
        Some((start, end)) => (206u16, start, end - start + 1),
        None => (200u16, 0, size),
    };

    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nserver: {}\r\ncontent-type: {mime}\r\ncontent-length: {length}\r\netag: {etag}\r\nlast-modified: {last_modified}\r\naccept-ranges: bytes\r\n",
        if status == 206 { "Partial Content" } else { "OK" },
        server_header(),
    );
    if let Some((start, end)) = range {
        head.push_str(&format!("content-range: bytes {start}-{end}/{size}\r\n"));
    }
    for (name, value) in ctx.extra_headers {
        if let Some(value) = value {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if !ctx.keep_alive {
        head.push_str("connection: close\r\n");
    }
    head.push_str("\r\n");
    wr.write_all(head.as_bytes()).await?;

    if !ctx.head_only && length > 0 {
        let mut async_file = tokio::fs::File::from_std(file);
        if offset > 0 {
            async_file.seek(std::io::SeekFrom::Start(offset)).await?;
        }
        let mut remaining = length;
        let mut chunk = vec![0u8; COPY_CHUNK.min(length as usize)];
        while remaining > 0 {
            let want = (remaining as usize).min(chunk.len());
            let n = async_file.read(&mut chunk[..want]).await?;
            if n == 0 {
                break; // file truncated under us; header already sent
            }
            wr.write_all(&chunk[..n]).await?;
            remaining -= n as u64;
        }
    }
    wr.flush().await?;
    Ok(Some(status))
}

/// `If-None-Match: "a", "b"` | `*` — is our ETag listed?
fn etag_listed(header: &[u8], etag: &[u8]) -> bool {
    if header == b"*" {
        return true;
    }
    header.split(|&b| b == b',').any(|part| part.trim_ascii() == etag)
}

/// Single-range `bytes=start-end` / `start-` / `-suffix`.
/// Ok(None) = ignore (unsupported shape), Err = unsatisfiable (416).
#[allow(clippy::result_unit_err)]
fn parse_range(header: &[u8], size: u64) -> Result<Option<(u64, u64)>, ()> {
    let Some(spec) = header.strip_prefix(b"bytes=") else {
        return Ok(None); // unknown unit: ignore the header
    };
    if spec.contains(&b',') {
        return Ok(None); // multipart ranges: serve full body instead
    }
    let spec = spec.trim_ascii();
    let dash = memchr::memchr(b'-', spec).ok_or(())?;
    let (start_s, end_s) = (&spec[..dash], &spec[dash + 1..]);

    let parse = |s: &[u8]| -> Result<u64, ()> {
        std::str::from_utf8(s).map_err(|_| ())?.parse().map_err(|_| ())
    };

    if size == 0 {
        return Err(());
    }

    if start_s.is_empty() {
        // suffix: last N bytes
        let n = parse(end_s)?;
        if n == 0 {
            return Err(());
        }
        let start = size.saturating_sub(n);
        return Ok(Some((start, size - 1)));
    }

    let start = parse(start_s)?;
    if start >= size {
        return Err(());
    }
    let end = if end_s.is_empty() { size - 1 } else { parse(end_s)?.min(size - 1) };
    if end < start {
        return Err(());
    }
    Ok(Some((start, end)))
}

fn mime_for<'a>(path: &str, overrides: &'a BTreeMap<String, String>) -> &'a str
where
    'static: 'a,
{
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    if let Some(custom) = overrides.get(&ext) {
        return custom;
    }
    match ext.as_str() {
        "html" | "htm" => "text/html",
        "css" => "text/css",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "txt" => "text/plain",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "pdf" => "application/pdf",
        "xml" => "application/xml",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn etag_listed_star_and_list() {
        assert!(etag_listed(b"*", b"\"abc\""));
        assert!(etag_listed(b"\"x\", \"abc\", \"y\"", b"\"abc\""));
        assert!(!etag_listed(b"\"x\", \"y\"", b"\"abc\""));
    }

    #[test]
    fn parse_range_shapes() {
        assert_eq!(parse_range(b"bytes=0-3", 10), Ok(Some((0, 3))));
        assert_eq!(parse_range(b"bytes=5-", 10), Ok(Some((5, 9))));
        assert_eq!(parse_range(b"bytes=-3", 10), Ok(Some((7, 9)))); // suffix
        assert_eq!(parse_range(b"bytes=0-100", 10), Ok(Some((0, 9)))); // end clamped
    }

    #[test]
    fn parse_range_ignored_and_unsatisfiable() {
        assert_eq!(parse_range(b"items=0-3", 10), Ok(None)); // unknown unit
        assert_eq!(parse_range(b"bytes=0-3,5-6", 10), Ok(None)); // multipart
        assert_eq!(parse_range(b"bytes=10-", 10), Err(())); // start past end
        assert_eq!(parse_range(b"bytes=5-3", 10), Err(())); // end before start
        assert_eq!(parse_range(b"bytes=-0", 10), Err(())); // empty suffix
        assert_eq!(parse_range(b"bytes=0-0", 0), Err(())); // empty file
    }

    #[test]
    fn mime_for_lookup() {
        let empty = BTreeMap::new();
        assert_eq!(mime_for("x.html", &empty), "text/html");
        assert_eq!(mime_for("x.HTML", &empty), "text/html"); // case-insensitive ext
        assert_eq!(mime_for("x.unknownext", &empty), "application/octet-stream");
        assert_eq!(mime_for("noext", &empty), "application/octet-stream");
    }

    #[test]
    fn mime_override_wins() {
        let mut overrides = BTreeMap::new();
        overrides.insert("html".to_string(), "text/x-custom".to_string());
        assert_eq!(mime_for("x.html", &overrides), "text/x-custom");
    }

    // --- async serve() over real files -------------------------------------

    /// Unique scratch directory, removed on drop.
    struct TempDir(std::path::PathBuf);

    impl TempDir {
        fn new() -> Self {
            use std::sync::atomic::{AtomicU64, Ordering};
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "buran-static-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        fn write(&self, name: &str, contents: &[u8]) {
            std::fs::write(self.0.join(name), contents).unwrap();
        }

        fn template(&self) -> String {
            format!("{}$uri", self.0.to_str().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn ctx<'a>(
        mime_overrides: &'a BTreeMap<String, String>,
        source_exts: &'a BTreeSet<String>,
        req_headers: &'a [(Vec<u8>, Vec<u8>)],
        serve_sources: &'a ServeSources,
        head_only: bool,
    ) -> StaticContext<'a> {
        StaticContext {
            types: None,
            mime_overrides,
            source_exts,
            extra_source_exts: &[],
            follow_symlinks: true,
            serve_sources,
            req_headers,
            head_only,
            extra_headers: &[],
            keep_alive: true,
        }
    }

    fn head_of(resp: &[u8]) -> String {
        let end = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        String::from_utf8_lossy(&resp[..end]).into_owned()
    }

    fn header(resp: &[u8], name: &str) -> Option<String> {
        head_of(resp)
            .lines()
            .find_map(|l| l.strip_prefix(&format!("{name}: ")).map(str::to_string))
    }

    fn body_of(resp: &[u8]) -> Vec<u8> {
        let end = resp.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        resp[end..].to_vec()
    }

    #[tokio::test]
    async fn serves_a_file() {
        let dir = TempDir::new();
        dir.write("hello.txt", b"hello world");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let mut out = Vec::new();
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/hello.txt", &ctx(&mime, &src, &[], &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(200));
        assert!(head_of(&out).starts_with("HTTP/1.1 200 OK"));
        assert_eq!(header(&out, "content-type").as_deref(), Some("text/plain"));
        assert_eq!(header(&out, "content-length").as_deref(), Some("11"));
        assert_eq!(body_of(&out), b"hello world");
    }

    #[tokio::test]
    async fn non_suffix_template_serves_within_prefix() {
        // `$uri` is not the suffix: the share root is the literal prefix and
        // `/page` resolves to `<dir>/page.html`, jailed to <dir> (not `/`).
        let dir = TempDir::new();
        dir.write("page.html", b"<h1>hi</h1>");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let template = format!("{}$uri.html", dir.0.to_str().unwrap());
        let mut out = Vec::new();
        let status =
            serve(&mut out, &template, "index.html", b"/page", &ctx(&mime, &src, &[], &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(200));
        assert_eq!(body_of(&out), b"<h1>hi</h1>");
    }

    #[tokio::test]
    async fn head_request_omits_body() {
        let dir = TempDir::new();
        dir.write("hello.txt", b"hello world");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let mut out = Vec::new();
        serve(&mut out, &dir.template(), "index.html", b"/hello.txt", &ctx(&mime, &src, &[], &ServeSources::None, true))
            .await
            .unwrap();
        assert_eq!(header(&out, "content-length").as_deref(), Some("11"));
        assert!(body_of(&out).is_empty());
    }

    #[tokio::test]
    async fn missing_file_falls_back() {
        let dir = TempDir::new();
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let mut out = Vec::new();
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/nope.txt", &ctx(&mime, &src, &[], &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, None);
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn source_extensions_are_not_served() {
        let dir = TempDir::new();
        dir.write("app.php", b"<?php echo 1;");
        let mut src = BTreeSet::new();
        let mime = BTreeMap::new();
        src.insert("php".to_string());
        let mut out = Vec::new();
        // Source-leak protection: .php is refused -> fallback.
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/app.php", &ctx(&mime, &src, &[], &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, None);
        // Opt-in (all) serves it.
        let mut out2 = Vec::new();
        let status2 =
            serve(&mut out2, &dir.template(), "index.html", b"/app.php", &ctx(&mime, &src, &[], &ServeSources::All, false))
                .await
                .unwrap();
        assert_eq!(status2, Some(200));

        // Opt-in via a matching extension list also serves it; a
        // non-matching list keeps protection.
        let only_php = ServeSources::Only(vec![".php".to_string()]);
        let mut out3 = Vec::new();
        let s3 = serve(&mut out3, &dir.template(), "index.html", b"/app.php", &ctx(&mime, &src, &[], &only_php, false))
            .await
            .unwrap();
        assert_eq!(s3, Some(200));

        let only_inc = ServeSources::Only(vec!["inc".to_string()]);
        let mut out4 = Vec::new();
        let s4 = serve(&mut out4, &dir.template(), "index.html", b"/app.php", &ctx(&mime, &src, &[], &only_inc, false))
            .await
            .unwrap();
        assert_eq!(s4, None, "a list not containing php keeps .php protected");
    }

    #[tokio::test]
    async fn range_request_is_partial() {
        let dir = TempDir::new();
        dir.write("data.txt", b"0123456789");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let req = [(b"range".to_vec(), b"bytes=2-5".to_vec())];
        let mut out = Vec::new();
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/data.txt", &ctx(&mime, &src, &req, &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(206));
        assert_eq!(header(&out, "content-range").as_deref(), Some("bytes 2-5/10"));
        assert_eq!(header(&out, "content-length").as_deref(), Some("4"));
        assert_eq!(body_of(&out), b"2345");
    }

    #[tokio::test]
    async fn unsatisfiable_range_is_416() {
        let dir = TempDir::new();
        dir.write("data.txt", b"0123456789");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let req = [(b"range".to_vec(), b"bytes=99-".to_vec())];
        let mut out = Vec::new();
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/data.txt", &ctx(&mime, &src, &req, &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(416));
        assert!(head_of(&out).contains("416 Range Not Satisfiable"));
    }

    #[tokio::test]
    async fn conditional_get_returns_304() {
        let dir = TempDir::new();
        dir.write("hello.txt", b"hello world");
        let src = BTreeSet::new();
        let mime = BTreeMap::new();

        // First fetch to learn the ETag.
        let mut out = Vec::new();
        serve(&mut out, &dir.template(), "index.html", b"/hello.txt", &ctx(&mime, &src, &[], &ServeSources::None, false))
            .await
            .unwrap();
        let etag = header(&out, "etag").unwrap();

        // Re-request with If-None-Match: same entity -> 304, no body.
        let req = [(b"if-none-match".to_vec(), etag.into_bytes())];
        let mut out2 = Vec::new();
        let status =
            serve(&mut out2, &dir.template(), "index.html", b"/hello.txt", &ctx(&mime, &src, &req, &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(304));
        assert!(body_of(&out2).is_empty());
    }

    #[tokio::test]
    async fn directory_without_slash_redirects() {
        let dir = TempDir::new();
        std::fs::create_dir(dir.0.join("sub")).unwrap();
        let src = BTreeSet::new();
        let mime = BTreeMap::new();
        let mut out = Vec::new();
        let status =
            serve(&mut out, &dir.template(), "index.html", b"/sub", &ctx(&mime, &src, &[], &ServeSources::None, false))
                .await
                .unwrap();
        assert_eq!(status, Some(301));
        assert_eq!(header(&out, "location").as_deref(), Some("/sub/"));
    }
}
