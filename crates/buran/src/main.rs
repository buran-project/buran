//! buran — main process: CLI, config loading, module checks, supervision.
//!
//! Loads and validates the config, verifies module binaries via
//! `--describe`, starts per-application prototypes (see `spawn`) and runs
//! the router in-process.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Context;
use buran_config::Validated;
use tracing::{error, info};

mod spawn;

const DEFAULT_CONFIG: &str = "/etc/buran/buran.yaml";
const USAGE: &str = "\
Buran Application Server

Usage:
  buran [--config <path>]     run the server
  buran --check-config [--config <path>]   validate config and modules, then exit
  buran --modules [--config <path>]        list runtime modules found in modules dir
  buran --version
  buran --help
";

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(code) => code,
        Err(e) => {
            error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

struct Cli {
    config: PathBuf,
    check: bool,
    modules: bool,
}

fn parse_cli() -> anyhow::Result<Option<Cli>> {
    let mut config = PathBuf::from(DEFAULT_CONFIG);
    let mut check = false;
    let mut modules = false;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                config = PathBuf::from(args.next().context("--config requires a path")?);
            }
            "--check-config" => check = true,
            "--modules" => modules = true,
            "--version" | "-V" => {
                println!("buran {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            other => anyhow::bail!("unknown argument \"{other}\"\n\n{USAGE}"),
        }
    }

    Ok(Some(Cli { config, check, modules }))
}

fn run() -> anyhow::Result<ExitCode> {
    let Some(cli) = parse_cli()? else {
        return Ok(ExitCode::SUCCESS);
    };

    let validated = buran_config::from_file(&cli.config)
        .with_context(|| format!("config {}", cli.config.display()))?;

    if cli.modules {
        list_modules(&validated)?;
        return Ok(ExitCode::SUCCESS);
    }

    let source_exts = check_modules(&validated)?;

    if cli.check {
        println!("config ok: {}", cli.config.display());
        return Ok(ExitCode::SUCCESS);
    }

    serve(validated, source_exts)
}

/// Every referenced module must exist as `<modules>/buran-<module>` and
/// answer `--describe` with a compatible BWP version. Returns the union of
/// module source extensions (lowercase, no dot): the router refuses to
/// serve these as static files, so a forgotten `!*.php` in a share cannot
/// leak sources.
fn check_modules(validated: &Validated) -> anyhow::Result<std::collections::BTreeSet<String>> {
    let dir = Path::new(&validated.config.settings.modules);
    let mut source_exts = std::collections::BTreeSet::new();
    let mut described: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();

    for (name, app) in &validated.applications {
        let binary = dir.join(format!("buran-{}", app.module));
        if !binary.is_file() {
            anyhow::bail!(
                "applications.{name}.module: module \"{}\" not found in {}; available: {}",
                app.module,
                dir.display(),
                available_modules(dir),
            );
        }
        if !described.insert(app.module.as_str()) {
            continue;
        }

        let out = std::process::Command::new(&binary)
            .arg("--describe")
            .output()
            .with_context(|| format!("cannot run {} --describe", binary.display()))?;
        anyhow::ensure!(out.status.success(), "{} --describe failed", binary.display());
        let describe: serde_json::Value = serde_json::from_slice(&out.stdout)
            .with_context(|| format!("bad --describe JSON from {}", binary.display()))?;

        let bwp = describe.get("bwp").and_then(|v| v.as_u64()).unwrap_or(0);
        anyhow::ensure!(
            bwp == u64::from(buran_ipc::BWP_VERSION),
            "module \"{}\" speaks BWP v{bwp}, this buran speaks v{}",
            app.module,
            buran_ipc::BWP_VERSION,
        );

        if let Some(exts) = describe.get("source_extensions").and_then(|v| v.as_array()) {
            for ext in exts.iter().filter_map(|v| v.as_str()) {
                source_exts.insert(ext.trim_start_matches('.').to_ascii_lowercase());
            }
        }
        info!(module = %app.module, "module ok: {}", describe);
    }
    Ok(source_exts)
}

fn available_modules(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return "(modules directory does not exist)".to_string();
    };
    let list: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_prefix("buran-").map(String::from))
        .collect();
    if list.is_empty() {
        "(none)".to_string()
    } else {
        list.join(", ")
    }
}

fn list_modules(validated: &Validated) -> anyhow::Result<()> {
    let dir = Path::new(&validated.config.settings.modules);
    println!("modules directory: {}", dir.display());
    println!("{}", available_modules(dir));
    Ok(())
}

fn serve(
    validated: Validated,
    source_exts: std::collections::BTreeSet<String>,
) -> anyhow::Result<ExitCode> {
    let threads = validated
        .config
        .settings
        .listen_threads
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .enable_all()
        .build()?;

    let modules_dir = Path::new(&validated.config.settings.modules);
    let mut spawners = std::collections::BTreeMap::new();
    for (name, app) in &validated.applications {
        let binary = modules_dir.join(format!("buran-{}", app.module));
        let (spawner, work) = spawn::make_spawner(name, binary, app)?;
        spawners.insert(name.clone(), (spawner, work));
    }

    runtime.block_on(async {
        let router = buran_router::Router::new(&validated, spawners, source_exts)?;
        info!(threads, "buran starting");

        reap_orphans_as_pid1();

        // Graceful shutdown: SIGTERM (container stop) / SIGINT. The router
        // stops accepting, drains in-flight connections, then main exits;
        // prototypes and workers follow via closed channels.
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        tokio::spawn(async move {
            tokio::select! {
                _ = sigterm.recv() => info!("SIGTERM received, shutting down"),
                _ = sigint.recv() => info!("SIGINT received, shutting down"),
            }
            let _ = shutdown_tx.send(true);
        });

        router.serve(shutdown_rx).await
    })?;

    info!("bye");
    Ok(ExitCode::SUCCESS)
}

/// As PID 1 in a container, adopted orphans (e.g. workers outliving their
/// prototype) reparent to us; collect them so they do not stay zombies.
/// Statuses of direct children are also consumed by their per-child wait()
/// threads — whoever reaps first wins, the loser gets ECHILD and moves on.
fn reap_orphans_as_pid1() {
    if std::process::id() != 1 {
        return;
    }
    tokio::spawn(async {
        let Ok(mut sigchld) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())
        else {
            return;
        };
        loop {
            sigchld.recv().await;
            loop {
                match nix::sys::wait::waitpid(
                    Some(nix::unistd::Pid::from_raw(-1)),
                    Some(nix::sys::wait::WaitPidFlag::WNOHANG),
                ) {
                    Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => break,
                    Ok(_) => continue,
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TempDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "buran-modules-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    #[test]
    fn available_modules_missing_dir() {
        assert_eq!(
            available_modules(Path::new("/nonexistent/buran/modules")),
            "(modules directory does not exist)"
        );
    }

    #[test]
    fn available_modules_empty_dir() {
        let dir = temp_dir();
        assert_eq!(available_modules(&dir.0), "(none)");
    }

    #[test]
    fn available_modules_lists_stripped_names() {
        let dir = temp_dir();
        std::fs::write(dir.0.join("buran-php85"), b"").unwrap();
        std::fs::write(dir.0.join("buran-php74"), b"").unwrap();
        std::fs::write(dir.0.join("unrelated"), b"").unwrap(); // no buran- prefix
        let listed = available_modules(&dir.0);
        assert!(listed.contains("php85"), "{listed}");
        assert!(listed.contains("php74"), "{listed}");
        assert!(!listed.contains("unrelated"), "{listed}");
    }
}
