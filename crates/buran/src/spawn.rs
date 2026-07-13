//! Worker spawning via the prototype model (spec 2.3).
//!
//! One prototype process per application boots the module runtime once
//! (for PHP: opcache SHM is created there) and forks warm workers on
//! command. The supervisor talks to the prototype over a control channel:
//! one byte per spawn, the worker fd attached via SCM_RIGHTS.

use std::io::{IoSlice, Write};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::os::unix::net::{UnixDatagram, UnixStream as StdUnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex};

use anyhow::Context;
use buran_config::Application;
use buran_router::{Spawn, Spawner};
use nix::sys::socket::{sendmsg, ControlMessage, MsgFlags};
use tracing::{info, warn};

const CONTROL_FD: i32 = 3;
const WORK_FD: i32 = 4;

/// Resolved privilege-drop identity for an application.
#[derive(Default)]
struct DropIds {
    uid: Option<u32>,
    gid: Option<u32>,
    /// User name, forwarded so the prototype can call initgroups.
    user_name: Option<String>,
}

/// Resolve `user`/`group` names to numeric ids at startup (fail fast: a
/// typo in a username must not surface as a prototype crash loop).
fn resolve_ids(app: &Application) -> anyhow::Result<DropIds> {
    let user = app
        .user
        .as_deref()
        .map(|name| match nix::unistd::User::from_name(name) {
            Ok(Some(u)) => Ok(u),
            Ok(None) => Err(anyhow::anyhow!("unknown user \"{name}\"")),
            // A lookup error (EIO / NSS down) must not read as "no such user".
            Err(e) => Err(anyhow::anyhow!("looking up user \"{name}\": {e}")),
        })
        .transpose()?;
    let explicit_gid = app
        .group
        .as_deref()
        .map(|name| match nix::unistd::Group::from_name(name) {
            Ok(Some(g)) => Ok(g.gid.as_raw()),
            Ok(None) => Err(anyhow::anyhow!("unknown group \"{name}\"")),
            Err(e) => Err(anyhow::anyhow!("looking up group \"{name}\": {e}")),
        })
        .transpose()?;

    // When only `user` is set, fall back to that user's primary group so the
    // gid is dropped too; otherwise the worker would keep buran's gid.
    let gid = explicit_gid.or_else(|| user.as_ref().map(|u| u.gid.as_raw()));
    let uid = user.as_ref().map(|u| u.uid.as_raw());
    let user_name = user.map(|u| u.name);

    if (uid.is_some() || gid.is_some()) && !nix::unistd::Uid::effective().is_root() {
        anyhow::bail!("application user/group is set but buran is not running as root");
    }
    Ok(DropIds { uid, gid, user_name })
}

/// JSON slice of the application config owned by the module (spec 2.4:
/// main validates the common part, the module knows its own fields).
fn module_app_config(app: &Application, ids: &DropIds) -> String {
    let ini_file = app
        .options
        .as_ref()
        .and_then(|v| v.get("file"))
        .and_then(|v| v.as_str())
        .map(String::from);

    let ini_map = |key: &str| -> serde_json::Value {
        let mut out = serde_json::Map::new();
        if let Some(map) = app.options.as_ref().and_then(|v| v.get(key)).and_then(|v| v.as_mapping())
        {
            for (name, value) in map {
                let (Some(name), Some(value)) = (name.as_str(), yaml_scalar(value)) else {
                    continue;
                };
                out.insert(name.to_string(), serde_json::Value::String(value));
            }
        }
        serde_json::Value::Object(out)
    };

    // Roots are absolutized: PHP's VCWD resolves relative paths against the
    // worker cwd, which is an accident waiting to happen.
    let root = app
        .root
        .as_deref()
        .map(|r| {
            std::path::absolute(r)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|_| r.to_string())
        })
        .unwrap_or_default();

    serde_json::json!({
        "root": root,
        "script": app.script,
        "index": app.index,
        "ini_file": ini_file,
        "admin": ini_map("admin"),
        "user": ini_map("user"),
        "max_requests": app.limits.requests,
        "user_id": ids.uid,
        "group_id": ids.gid,
        "user_name": ids.user_name,
        "execute": app.execute,
    })
    .to_string()
}

/// ini values may be written as strings, numbers or booleans in YAML.
fn yaml_scalar(value: &serde_norway::Value) -> Option<String> {
    match value {
        serde_norway::Value::String(s) => Some(s.clone()),
        serde_norway::Value::Number(n) => Some(n.to_string()),
        serde_norway::Value::Bool(b) => Some(if *b { "1" } else { "0" }.to_string()),
        _ => None,
    }
}

struct Prototype {
    control: StdUnixStream,
    child: Child,
}

struct PrototypeSpawner {
    app_name: String,
    module_binary: PathBuf,
    app_config: String,
    prototype: Mutex<Option<Prototype>>,
    /// Per-application environment overlaid on the inherited environment
    /// (config `environment:`). Workers inherit it via fork.
    env: Vec<(String, String)>,
    /// Working directory pinned for the prototype and its workers (config
    /// `working_directory:`); `None` leaves buran's cwd in place.
    working_dir: Option<PathBuf>,
    /// Worker end of the shared work socket. Held here so the queue (and
    /// any datagrams in flight) survives a prototype restart.
    work_worker_end: OwnedFd,
}

impl PrototypeSpawner {
    /// Ask the prototype to fork a worker; (re)start the prototype if it is
    /// not running or the control channel is dead.
    fn spawn_worker(&self) -> anyhow::Result<tokio::net::UnixStream> {
        let mut guard = self.prototype.lock().expect("prototype lock");

        for attempt in 0..2 {
            if guard.is_none() {
                *guard = Some(self.start_prototype()?);
            }
            let proto = guard.as_mut().expect("just ensured");

            let (ours, theirs) = StdUnixStream::pair().context("worker socketpair")?;
            match send_fd(&proto.control, theirs.as_raw_fd()) {
                Ok(()) => {
                    drop(theirs);
                    ours.set_nonblocking(true).context("set_nonblocking")?;
                    return tokio::net::UnixStream::from_std(ours).context("tokio UnixStream");
                }
                Err(e) if attempt == 0 => {
                    // Prototype died: kill leftovers and escalate to restart
                    // of the whole application tree (spec 2.3).
                    warn!(app = %self.app_name, "prototype unreachable ({e}); restarting it");
                    let _ = proto.child.kill();
                    let _ = proto.child.wait();
                    *guard = None;
                }
                Err(e) => return Err(e).context("send spawn command"),
            }
        }
        unreachable!("two attempts always return");
    }

    fn start_prototype(&self) -> anyhow::Result<Prototype> {
        let (control, theirs) = StdUnixStream::pair().context("control socketpair")?;
        let theirs_fd = theirs.into_raw_fd();

        let work_fd = self.work_worker_end.as_raw_fd();
        let mut cmd = std::process::Command::new(&self.module_binary);
        cmd.arg("--prototype")
            .arg("--control")
            .arg(CONTROL_FD.to_string())
            .arg("--work")
            .arg(WORK_FD.to_string())
            .arg("--app-config")
            .arg(&self.app_config)
            // The prototype and every forked worker share these pipes:
            // one reader per application routes them into the error log.
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());

        // Per-application env is overlaid on the inherited environment; the
        // working directory, when set, is pinned before exec. Both propagate
        // to workers through fork.
        cmd.envs(self.env.iter().map(|(k, v)| (k, v)));
        if let Some(dir) = &self.working_dir {
            cmd.current_dir(dir);
        }

        // Safety: dup2 is async-signal-safe; it clears CLOEXEC on the
        // target fds so the child inherits exactly the control channel and
        // the shared work socket.
        unsafe {
            cmd.pre_exec(move || {
                if libc_dup2(theirs_fd, CONTROL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc_dup2(work_fd, WORK_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn prototype {}", self.module_binary.display()))?;
        libc_close(theirs_fd);

        if let Some(out) = child.stdout.take() {
            forward_output(self.app_name.clone(), "stdout", out);
        }
        if let Some(err) = child.stderr.take() {
            forward_output(self.app_name.clone(), "stderr", err);
        }

        info!(app = %self.app_name, pid = child.id(), "prototype started");
        Ok(Prototype { control, child })
    }
}

impl Spawn for PrototypeSpawner {
    fn spawn(&self) -> anyhow::Result<tokio::net::UnixStream> {
        self.spawn_worker()
    }

    fn kill(&self, token: u64) {
        // Best-effort: if the prototype is down there is nothing to kill (its
        // workers died with it). The command reaches the worker's parent, which
        // SIGKILLs by the pid it forked — pid-reuse safe.
        let guard = self.prototype.lock().expect("prototype lock");
        if let Some(proto) = guard.as_ref()
            && let Err(e) = send_kill(&proto.control, token) {
                warn!(app = %self.app_name, "kill worker token {token}: {e}");
            }
    }
}

/// Route application output into the error log, line by line, tagged with
/// the application name. One blocking thread per pipe, alive as long as
/// the prototype (workers inherit the same pipe via fork).
fn forward_output(app: String, channel: &'static str, pipe: impl std::io::Read + Send + 'static) {
    std::thread::spawn(move || {
        use std::io::BufRead;
        let reader = std::io::BufReader::new(pipe);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            if !line.is_empty() {
                warn!(app = %app, channel, "{line}");
            }
        }
    });
}

// Control protocol command bytes (must match the prototype).
const CMD_SPAWN: u8 = 1;
const CMD_KILL: u8 = 2;

/// One spawn command: the CMD_SPAWN byte with the worker fd attached.
fn send_fd(control: &StdUnixStream, fd: i32) -> std::io::Result<()> {
    let data = [CMD_SPAWN];
    let iov = [IoSlice::new(&data)];
    let fds = [fd];
    let cmsg = [ControlMessage::ScmRights(&fds)];
    sendmsg::<()>(control.as_raw_fd(), &iov, &cmsg, MsgFlags::empty(), None)
        .map_err(std::io::Error::from)?;
    Ok(())
}

/// One kill command: the CMD_KILL byte followed by the 8-byte worker token.
fn send_kill(control: &StdUnixStream, token: u64) -> std::io::Result<()> {
    let mut msg = [0u8; 9];
    msg[0] = CMD_KILL;
    msg[1..9].copy_from_slice(&token.to_le_bytes());
    (&mut &*control).write_all(&msg)
}

/// Returns the spawner plus the router end of the shared work socket
/// (the kernel-arbitrated request queue of this application).
pub fn make_spawner(
    app_name: &str,
    module_binary: PathBuf,
    app: &Application,
) -> anyhow::Result<(Spawner, UnixDatagram)> {
    let ids = resolve_ids(app).with_context(|| format!("application {app_name}"))?;

    let (router_end, worker_end) = UnixDatagram::pair().context("work socketpair")?;

    let spawner = Arc::new(PrototypeSpawner {
        app_name: app_name.to_string(),
        module_binary,
        app_config: module_app_config(app, &ids),
        prototype: Mutex::new(None),
        env: app.environment.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        working_dir: app.working_directory.as_ref().map(PathBuf::from),
        work_worker_end: OwnedFd::from(worker_end),
    });
    Ok((spawner, router_end))
}

fn libc_dup2(old: i32, new: i32) -> i32 {
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(old, new) }
}

fn libc_close(fd: i32) {
    unsafe extern "C" {
        fn close(fd: i32) -> i32;
    }
    unsafe {
        close(fd);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scalar(yaml: &str) -> serde_norway::Value {
        serde_norway::from_str(yaml).unwrap()
    }

    #[test]
    fn yaml_scalar_covers_scalar_kinds() {
        assert_eq!(yaml_scalar(&scalar("hello")).as_deref(), Some("hello"));
        assert_eq!(yaml_scalar(&scalar("42")).as_deref(), Some("42"));
        assert_eq!(yaml_scalar(&scalar("1.5")).as_deref(), Some("1.5"));
        assert_eq!(yaml_scalar(&scalar("true")).as_deref(), Some("1"));
        assert_eq!(yaml_scalar(&scalar("false")).as_deref(), Some("0"));
        // Non-scalars are dropped.
        assert_eq!(yaml_scalar(&scalar("[1, 2]")), None);
        assert_eq!(yaml_scalar(&scalar("~")), None);
    }

    /// Pull a single validated `Application` out of an inline config.
    fn app_from(yaml: &str) -> Application {
        buran_config::from_str(yaml).unwrap().applications.get("app").unwrap().clone()
    }

    #[test]
    fn module_app_config_serializes_expected_fields() {
        let app = app_from(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: php85
    root: www
    script: index.php
    index: index.php
    execute: [\".html\"]
    options:
      file: /etc/php.ini
      admin:
        memory_limit: 256M
      user:
        display_errors: \"1\"
",
        );

        let ids = DropIds {
            uid: Some(1000),
            gid: Some(33),
            user_name: Some("www-data".to_string()),
        };
        let json: serde_json::Value =
            serde_json::from_str(&module_app_config(&app, &ids)).unwrap();

        // Relative root is absolutized against the cwd.
        let root = json["root"].as_str().unwrap();
        assert!(root.starts_with('/'), "root not absolute: {root}");
        assert!(root.ends_with("/www"), "root: {root}");

        assert_eq!(json["script"], "index.php");
        assert_eq!(json["index"], "index.php");
        assert_eq!(json["ini_file"], "/etc/php.ini");
        assert_eq!(json["admin"]["memory_limit"], "256M");
        assert_eq!(json["user"]["display_errors"], "1");
        assert_eq!(json["max_requests"], 0);
        assert_eq!(json["user_id"], 1000);
        assert_eq!(json["group_id"], 33);
        assert_eq!(json["user_name"], "www-data");
        assert_eq!(json["execute"][0], ".html");
    }

    #[test]
    fn module_app_config_defaults_are_empty() {
        let app = app_from(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: php85
",
        );
        let json: serde_json::Value =
            serde_json::from_str(&module_app_config(&app, &DropIds::default())).unwrap();
        assert_eq!(json["root"], "");
        assert!(json["script"].is_null());
        assert!(json["ini_file"].is_null());
        assert!(json["admin"].as_object().unwrap().is_empty());
        assert!(json["user_id"].is_null());
    }

    #[test]
    fn resolve_ids_without_user_or_group_is_none() {
        // No user/group set: no privilege check, no lookups, both ids None.
        let app = app_from(
            "\
listeners:
  \"*:8080\": { route: main }
routes:
  main:
    - action: { application: app }
applications:
  app:
    module: php85
",
        );
        let ids = resolve_ids(&app).unwrap();
        assert_eq!((ids.uid, ids.gid, ids.user_name), (None, None, None));
    }
}
