//! Access log: combined format, one writer task fed over a *bounded* channel
//! so connection tasks never block on disk. When the disk cannot keep up the
//! channel fills and lines are dropped (never buffered without limit); the
//! loss count is surfaced in the log itself — honest for an edge server.
//!
//! Deviation from nginx combined, documented: the bytes field counts wire
//! bytes of the response (headers included), not body bytes only.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

/// Bounded backlog of pending log lines (~lines, not bytes). Beyond this the
/// disk is not keeping up and further lines are dropped.
const CHANNEL_CAPACITY: usize = 8192;

pub struct AccessLog {
    tx: mpsc::Sender<String>,
    /// Lines dropped since the last one written (disk backpressure).
    dropped: Arc<AtomicU64>,
}

impl AccessLog {
    /// `path` per config: a file path, `/dev/stdout` works naturally.
    /// Must be called within a tokio runtime (spawns the writer task).
    pub fn open(path: &str) -> std::io::Result<AccessLog> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let file = tokio::fs::File::from_std(file);

        let (tx, mut rx) = mpsc::channel::<String>(CHANNEL_CAPACITY);
        let dropped = Arc::new(AtomicU64::new(0));
        let dropped_writer = Arc::clone(&dropped);
        tokio::spawn(async move {
            let mut out = tokio::io::BufWriter::new(file);
            while let Some(line) = rx.recv().await {
                if out.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                // Surface any lines lost to backpressure since the last write.
                let lost = dropped_writer.swap(0, Ordering::Relaxed);
                if lost > 0 {
                    let notice = format!("#buran: dropped {lost} access log line(s) under backpressure\n");
                    if out.write_all(notice.as_bytes()).await.is_err() {
                        break;
                    }
                }
                // Flush per line: log lines must not sit in a buffer when
                // the container is killed.
                if out.flush().await.is_err() {
                    break;
                }
            }
        });

        Ok(AccessLog { tx, dropped })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn log(
        &self,
        remote: &str,
        method: &[u8],
        target: &[u8],
        status: u16,
        bytes: u64,
        referer: Option<&[u8]>,
        user_agent: Option<&[u8]>,
    ) {
        let line = format!(
            "{remote} - - [{time}] \"{method} {target} HTTP/1.1\" {status} {bytes} \"{referer}\" \"{user_agent}\"\n",
            time = format_clf_time(),
            method = escape(method),
            target = escape(target),
            referer = referer.map(escape).unwrap_or_default(),
            user_agent = user_agent.map(escape).unwrap_or_default(),
        );
        // Never block a connection task on disk: a full channel (disk behind)
        // or a gone writer drops the line and bumps the loss counter.
        if self.tx.try_send(line).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Escape a client-supplied field for the log line: control bytes (incl. the
/// CR/LF that would forge a new record) and the `"`/`\` that delimit the
/// quoted fields become `\xNN`. Printable UTF-8 is passed through lossily.
fn escape(value: &[u8]) -> String {
    let mut out = String::with_capacity(value.len());
    for chunk in String::from_utf8_lossy(value).chars() {
        match chunk {
            '"' | '\\' => {
                out.push('\\');
                out.push(chunk);
            }
            c if c.is_control() => {
                for b in c.to_string().bytes() {
                    out.push_str(&format!("\\x{b:02x}"));
                }
            }
            c => out.push(c),
        }
    }
    out
}

/// Common Log Format timestamp, UTC: `10/Oct/2000:13:55:36 +0000`.
fn format_clf_time() -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (hh, mm, ss) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    let (year, month, day) = crate::uri::civil_from_days(days as i64);

    format!(
        "{day:02}/{}/{year}:{hh:02}:{mm:02}:{ss:02} +0000",
        MONTHS[(month - 1) as usize]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_passes_plain_text_through() {
        assert_eq!(escape(b"Mozilla/5.0 (X11; Linux)"), "Mozilla/5.0 (X11; Linux)");
        // Valid multi-byte UTF-8 survives.
        assert_eq!(escape("Firefox/Café".as_bytes()), "Firefox/Café");
    }

    #[test]
    fn escape_neutralizes_crlf_injection() {
        // A forged log line smuggled through a header value must not break out
        // of its quoted field.
        let evil = b"ua\r\n1.2.3.4 - - [x] \"GET /admin\" 200 0";
        let escaped = escape(evil);
        assert!(!escaped.contains('\r'));
        assert!(!escaped.contains('\n'));
        assert!(escaped.starts_with("ua\\x0d\\x0a"));
    }

    #[test]
    fn escape_quotes_and_backslashes() {
        // The `"` delimiters and `\` (the escape char itself) are escaped so
        // the field stays unambiguous.
        assert_eq!(escape(br#"a"b\c"#), "a\\\"b\\\\c");
    }

    #[test]
    fn escape_control_bytes_become_hex() {
        assert_eq!(escape(b"\x00\x1f\x7f"), "\\x00\\x1f\\x7f");
    }
}
