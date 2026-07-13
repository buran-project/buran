//! Access log: combined format, one writer task fed over an unbounded
//! channel so connection tasks never block on disk.
//!
//! Deviation from nginx combined, documented: the bytes field counts wire
//! bytes of the response (headers included), not body bytes only.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

pub struct AccessLog {
    tx: mpsc::UnboundedSender<String>,
}

impl AccessLog {
    /// `path` per config: a file path, `/dev/stdout` works naturally.
    /// Must be called within a tokio runtime (spawns the writer task).
    pub fn open(path: &str) -> std::io::Result<AccessLog> {
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let file = tokio::fs::File::from_std(file);

        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tokio::spawn(async move {
            let mut out = tokio::io::BufWriter::new(file);
            while let Some(line) = rx.recv().await {
                if out.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
                // Flush per line: log lines must not sit in a buffer when
                // the container is killed.
                if out.flush().await.is_err() {
                    break;
                }
            }
        });

        Ok(AccessLog { tx })
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
            method = String::from_utf8_lossy(method),
            target = String::from_utf8_lossy(target),
            referer = referer.map(String::from_utf8_lossy).unwrap_or_default(),
            user_agent = user_agent.map(String::from_utf8_lossy).unwrap_or_default(),
        );
        let _ = self.tx.send(line);
    }
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
