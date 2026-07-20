//! buran-echo — reference module for the concurrent BWP profile.
//!
//! Serves no language; it echoes requests back. Its purpose is to be the
//! executable specification of how an event-loop runtime (Node, Go, PHP
//! TrueAsync) talks to buran: unbounded concurrency declared in Hello,
//! many claimed requests at once, streamed request bodies, graceful
//! Retire. Modes mirror buran-php:
//!
//! - `--describe` — module contract, JSON to stdout
//! - `--prototype --control <fd> --work <fd>` — fork a worker per command.
//!   The app config is read from the control socket (length-prefixed JSON),
//!   not argv, so ini secrets stay out of `/proc/<pid>/cmdline`.
//! - `--channel <fd> --work <fd> --app-config <json>` —
//!   standalone BWP worker

use std::os::fd::FromRawFd;
use std::process::ExitCode;

use buran_worker::Describe;
use serde::Deserialize;

mod prototype;
mod worker;

/// Application config slice owned by this module. Everything the echo
/// runtime does not understand is ignored.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub max_requests: u64,
    #[serde(default)]
    pub user_id: Option<u32>,
    #[serde(default)]
    pub group_id: Option<u32>,
    /// User name, kept alongside `user_id` so the prototype can call
    /// initgroups (install the user's own supplementary groups) on drop.
    #[serde(default)]
    pub user_name: Option<String>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--describe") => {
            Describe {
                runtime: "echo",
                version: env!("CARGO_PKG_VERSION").to_string(),
                source_extensions: &[],
            }
            .print();
            ExitCode::SUCCESS
        }
        Some("--prototype") => {
            let (Some(fd), Some(work_fd)) = (
                arg_value(&args, "--control").and_then(|v| v.parse::<i32>().ok()),
                arg_value(&args, "--work").and_then(|v| v.parse::<i32>().ok()),
            ) else {
                eprintln!("buran-echo: --prototype requires --control <fd> and --work <fd>");
                return ExitCode::FAILURE;
            };
            // The app config arrives over the control socket, not argv (keeps
            // ini secrets out of /proc/<pid>/cmdline) — see prototype::run.
            prototype::run(fd, work_fd)
        }
        Some("--channel") => {
            let (Some(fd), Some(work_fd)) = (
                args.get(1).and_then(|v| v.parse::<i32>().ok()),
                arg_value(&args, "--work").and_then(|v| v.parse::<i32>().ok()),
            ) else {
                eprintln!("buran-echo: --channel requires an fd number and --work <fd>");
                return ExitCode::FAILURE;
            };
            let app = match parse_app_config(&args) {
                Ok(app) => app,
                Err(e) => {
                    eprintln!("buran-echo: bad app config: {e}");
                    return ExitCode::FAILURE;
                }
            };
            // Safety: fds are inherited from the supervisor per the module
            // contract and owned exclusively by this process.
            let stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(fd) };
            let work = unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(work_fd) };
            match worker::serve(work, stream, &app) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("buran-echo: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "buran-echo: module binary (--describe | --prototype ... | --channel <fd> --work <fd> --app-config <json>)"
            );
            ExitCode::FAILURE
        }
    }
}

fn parse_app_config(args: &[String]) -> Result<AppConfig, String> {
    match arg_value(args, "--app-config") {
        Some(json) => serde_json::from_str(json).map_err(|e| e.to_string()),
        None => Ok(AppConfig::default()),
    }
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}
