//! buran-php — PHP runtime module (spec section 2.5).
//!
//! Modes:
//! - `--describe` — module contract, JSON to stdout
//! - `--prototype --control <fd> --work <fd>` — boot once, fork warm workers.
//!   The app config is read from the control socket (length-prefixed JSON),
//!   not argv, so ini secrets stay out of `/proc/<pid>/cmdline`.
//! - `--channel <fd> --work <fd> --app-config <json>` — standalone BWP worker
//! - `--exec <script.php> [n]` — phase-0 PoC over the embed SAPI

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::process::ExitCode;
use std::time::Instant;

use buran_worker::Describe;
use serde::Deserialize;

mod prototype;
mod worker;

mod embed {
    use std::ffi::c_char;

    unsafe extern "C" {
        pub fn bphp_init() -> i32;
        pub fn bphp_exec(filename: *const c_char) -> i32;
        pub fn bphp_request_recycle() -> i32;
        pub fn bphp_shutdown();
    }
}

/// Application config slice owned by this module (spec: main validates the
/// common part, the module knows its own fields).
#[derive(Debug, Clone, Deserialize)]
pub struct AppConfig {
    pub root: String,
    #[serde(default)]
    pub script: Option<String>,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub ini_file: Option<String>,
    /// options.admin: system-level ini directives (zend_extension, limits).
    #[serde(default)]
    pub admin: std::collections::BTreeMap<String, String>,
    /// options.user: ini directives scripts may override via ini_set().
    #[serde(default)]
    pub user: std::collections::BTreeMap<String, String>,
    /// limits.requests: worker exits after N requests (0 = never).
    #[serde(default)]
    pub max_requests: u64,
    /// Resolved by the supervisor; applied by the prototype before boot.
    #[serde(default)]
    pub user_id: Option<u32>,
    #[serde(default)]
    pub group_id: Option<u32>,
    /// User name, kept alongside `user_id` so the prototype can call
    /// initgroups (install the user's own supplementary groups) on drop.
    #[serde(default)]
    pub user_name: Option<String>,
    /// Extra executable extensions on top of the intrinsic .php set.
    #[serde(default)]
    pub execute: Vec<String>,
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("--describe") => {
            Describe {
                runtime: "php",
                version: env!("BURAN_PHP_VERSION").to_string(),
                source_extensions: &[".php", ".phtml", ".phar"],
            }
            .print();
            ExitCode::SUCCESS
        }
        Some("--exec") => {
            let Some(file) = args.get(1) else {
                eprintln!("buran-php: --exec requires a script path");
                return ExitCode::FAILURE;
            };
            let requests: u32 = args.get(2).and_then(|v| v.parse().ok()).unwrap_or(1);
            exec_poc(file, requests)
        }
        Some("--prototype") => {
            let Some(fd) = arg_value(&args, "--control").and_then(|v| v.parse::<i32>().ok()) else {
                eprintln!("buran-php: --prototype requires --control <fd>");
                return ExitCode::FAILURE;
            };
            let Some(work_fd) = arg_value(&args, "--work").and_then(|v| v.parse::<i32>().ok())
            else {
                eprintln!("buran-php: --prototype requires --work <fd>");
                return ExitCode::FAILURE;
            };
            // The app config arrives over the control socket, not argv (keeps
            // ini secrets out of /proc/<pid>/cmdline) — see prototype::run.
            prototype::run(fd, work_fd)
        }
        Some("--channel") => {
            let Some(fd) = args.get(1).and_then(|v| v.parse::<i32>().ok()) else {
                eprintln!("buran-php: --channel requires an fd number");
                return ExitCode::FAILURE;
            };
            let app: AppConfig = match arg_value(&args, "--app-config")
                .ok_or_else(|| "missing --app-config".to_string())
                .and_then(|json| serde_json::from_str(json).map_err(|e| e.to_string()))
            {
                Ok(app) => app,
                Err(e) => {
                    eprintln!("buran-php: bad app config: {e}");
                    return ExitCode::FAILURE;
                }
            };

            let Some(work_fd) = arg_value(&args, "--work").and_then(|v| v.parse::<i32>().ok())
            else {
                eprintln!("buran-php: --channel requires --work <fd>");
                return ExitCode::FAILURE;
            };
            // Safety: fds are inherited from the supervisor per the module
            // contract and owned exclusively by this process.
            let stream = unsafe { UnixStream::from_raw_fd(fd) };
            let work =
                unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(work_fd) };
            match worker::run(&work, stream, app) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("buran-php: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        _ => {
            eprintln!(
                "buran-php: module binary (--describe | --channel <fd> --app-config <json> | --exec <script.php> [n])"
            );
            ExitCode::FAILURE
        }
    }
}

fn arg_value<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).map(String::as_str)
}

/// Phase-0 PoC: boot libphp once, execute the script `requests` times in
/// recycled request contexts, report per-request cost.
fn exec_poc(file: &str, requests: u32) -> ExitCode {
    let c_file = match std::ffi::CString::new(file) {
        Ok(s) => s,
        Err(_) => {
            eprintln!("buran-php: script path contains NUL");
            return ExitCode::FAILURE;
        }
    };

    // Safety: single-threaded process, engine booted exactly once.
    unsafe {
        let boot = Instant::now();
        if embed::bphp_init() != 0 {
            eprintln!("buran-php: php_embed_init failed");
            return ExitCode::FAILURE;
        }
        let boot_cost = boot.elapsed();

        let mut failed = false;
        let run = Instant::now();
        for i in 0..requests {
            if embed::bphp_exec(c_file.as_ptr()) < 0 {
                eprintln!("buran-php: script bailed out (fatal error)");
                failed = true;
                break;
            }
            let last = i + 1 == requests;
            if !last && embed::bphp_request_recycle() != 0 {
                eprintln!("buran-php: request recycle failed");
                failed = true;
                break;
            }
        }
        let run_cost = run.elapsed();

        embed::bphp_shutdown();

        if failed {
            return ExitCode::FAILURE;
        }

        eprintln!(
            "buran-php poc: boot {:.1?}, {} request(s) in {:.1?} ({:.1?}/req)",
            boot_cost,
            requests,
            run_cost,
            run_cost / requests.max(1),
        );
    }

    ExitCode::SUCCESS
}
