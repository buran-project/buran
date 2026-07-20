//! Prototype process (spec 2.3), echo edition. Nothing to boot: the value
//! here is the fork discipline every event-loop module must follow —
//! strictly single-threaded until fork, the child builds its async runtime
//! only after it.
//!
//! Control protocol (supervisor -> prototype):
//! - `CMD_SPAWN`: an 8-byte supervisor-assigned token follows; fork a worker on
//!   the channel fd (attached via SCM_RIGHTS) and map the token to its pid.
//! - `CMD_KILL`:  followed by an 8-byte worker token; SIGKILL that worker.
//!
//! The prototype is the workers' parent, so killing by the pid it forked is
//! reuse-safe. EOF on the control channel means the supervisor is gone -> exit.

use std::collections::HashMap;
use std::io::{IoSliceMut, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixStream};

use nix::sys::signal::{kill, Signal};
use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};

use crate::{worker, AppConfig};

const CMD_SPAWN: u8 = 1;
const CMD_KILL: u8 = 2;

/// A control command from the supervisor.
enum Command {
    /// Fork a worker on this channel fd, tagged with the supervisor-assigned
    /// token (mapped to the child pid for reuse-safe kills).
    Spawn(OwnedFd, u64),
    Kill(u64),
}

/// Upper bound on the length-prefixed app-config the supervisor may send. The
/// config is tiny; this is only a guard against a corrupt length prefix.
const MAX_APP_CONFIG: usize = 1 << 20; // 1 MiB

/// Read the app config the supervisor sends over the control socket: a u32
/// little-endian length prefix followed by that many JSON bytes.
fn read_app_config(control: &UnixStream) -> Result<AppConfig, String> {
    let mut len_buf = [0u8; 4];
    (&mut &*control).read_exact(&mut len_buf).map_err(|e| format!("length: {e}"))?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len > MAX_APP_CONFIG {
        return Err(format!("app config length {len} exceeds {MAX_APP_CONFIG}"));
    }
    let mut buf = vec![0u8; len];
    (&mut &*control).read_exact(&mut buf).map_err(|e| format!("body: {e}"))?;
    serde_json::from_slice(&buf).map_err(|e| e.to_string())
}

pub fn run(control_fd: RawFd, work_fd: RawFd) -> ! {
    // Safety: both fds are inherited from the supervisor per the module
    // contract. The work socket is shared by every forked worker: the
    // kernel delivers each request datagram to exactly one of them.
    let control = unsafe { UnixStream::from_raw_fd(control_fd) };
    let work = unsafe { UnixDatagram::from_raw_fd(work_fd) };

    // The supervisor sends the app config over the control socket before any
    // command (never on argv: ini values may hold secrets and argv is
    // world-readable via /proc/<pid>/cmdline). Read it first, still as root.
    let app = match read_app_config(&control) {
        Ok(app) => app,
        Err(e) => {
            eprintln!("buran-echo prototype: reading app config: {e}");
            std::process::exit(1);
        }
    };

    // Privilege drop before workers fork; groups first, then gid, then uid
    // (setuid drops the right to change the others). Workers inherit the
    // identity via fork.
    if let Some(gid) = app.group_id {
        let gid = nix::unistd::Gid::from_raw(gid);
        // Reset supplementary groups before dropping. Skipping this leaves a
        // root-started worker carrying root's supplementary groups — an
        // incomplete privilege drop (php-fpm calls initgroups for the same
        // reason). With the user name known, initgroups installs that user's
        // own groups; otherwise we at least strip inherited ones down to the
        // primary gid.
        let groups_res = match app.user_name.as_deref() {
            Some(name) => match std::ffi::CString::new(name) {
                Ok(cname) => nix::unistd::initgroups(&cname, gid),
                Err(_) => {
                    eprintln!("buran-echo prototype: user name contains NUL");
                    std::process::exit(1);
                }
            },
            None => nix::unistd::setgroups(&[gid]),
        };
        if let Err(e) = groups_res {
            eprintln!("buran-echo prototype: dropping supplementary groups failed: {e}");
            std::process::exit(1);
        }
        if let Err(e) = nix::unistd::setgid(gid) {
            eprintln!("buran-echo prototype: setgid({gid}) failed: {e}");
            std::process::exit(1);
        }
    }
    if let Some(uid) = app.user_id
        && let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)) {
            eprintln!("buran-echo prototype: setuid({uid}) failed: {e}");
            std::process::exit(1);
        }

    // token -> child pid, for reuse-safe kills by the parent. The token is
    // assigned by the supervisor and delivered with CMD_SPAWN, not minted here.
    let mut workers: HashMap<u64, Pid> = HashMap::new();

    loop {
        reap_children(&mut workers);

        // Wait up to a second for a command; the timeout wakes us to reap
        // workers that exited on their own (Retire / max_requests) instead of
        // leaving them as zombies until the next command arrives.
        if !wait_readable(&control) {
            continue;
        }

        let (worker_fd, token) = match recv_command(&control) {
            Ok(Some(Command::Spawn(fd, token))) => (fd, token),
            Ok(Some(Command::Kill(token))) => {
                if let Some(&pid) = workers.get(&token) {
                    let _ = kill(pid, Signal::SIGKILL);
                }
                continue;
            }
            Ok(None) => std::process::exit(0), // supervisor closed the channel
            Err(e) => {
                eprintln!("buran-echo prototype: control channel error: {e}");
                std::process::exit(1);
            }
        };

        // Safety: single-threaded by construction — the tokio runtime
        // exists only in children, never here.
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                drop(control);
                let stream = UnixStream::from(worker_fd);
                let work = match work.try_clone() {
                    Ok(w) => w,
                    Err(e) => {
                        eprintln!("buran-echo worker: cannot clone work socket: {e}");
                        std::process::exit(1);
                    }
                };
                let _ = worker::serve(work, stream, &app);
                std::process::exit(0);
            }
            Ok(ForkResult::Parent { child }) => {
                drop(worker_fd); // the child owns its copy
                workers.insert(token, child);
            }
            Err(e) => {
                eprintln!("buran-echo prototype: fork failed: {e}");
                drop(worker_fd);
            }
        }
    }
}

/// Poll the control channel for up to a second. `true` = readable (a command
/// or EOF is waiting, `recv_command` will not block); `false` = the timeout
/// elapsed, so the loop just reaps and waits again. A poll error is treated as
/// readable so the recv path surfaces it.
fn wait_readable(control: &UnixStream) -> bool {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use std::os::fd::AsFd;

    let mut fds = [PollFd::new(control.as_fd(), PollFlags::POLLIN)];
    match poll(&mut fds, PollTimeout::from(1000u16)) {
        Ok(0) => false, // timeout: nothing to receive, go reap
        _ => true,      // readable, or an error to surface via recv_command
    }
}

/// Receive one control command; `Ok(None)` on clean EOF.
fn recv_command(control: &UnixStream) -> std::io::Result<Option<Command>> {
    let mut cmd = [0u8; 1];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

    let (bytes, spawn_fd) = {
        let mut iov = [IoSliceMut::new(&mut cmd)];
        let msg = recvmsg::<()>(
            control.as_raw_fd(),
            &mut iov,
            Some(&mut cmsg_buf),
            MsgFlags::empty(),
        )
        .map_err(std::io::Error::from)?;
        let mut fd = None;
        for cmsg in msg.cmsgs().map_err(std::io::Error::from)? {
            if let ControlMessageOwned::ScmRights(fds) = cmsg {
                for extra in fds {
                    // Keep the first fd; close any surplus so a misbehaving peer
                    // attaching several fds cannot leak them into the prototype.
                    // Safety: we are the sole owner of each freshly received fd.
                    match fd {
                        None => fd = Some(extra),
                        Some(_) => drop(unsafe { OwnedFd::from_raw_fd(extra) }),
                    }
                }
            }
        }
        (msg.bytes, fd)
    };

    if bytes == 0 {
        return Ok(None);
    }

    match cmd[0] {
        CMD_SPAWN => match spawn_fd {
            Some(fd) => {
                // The fd rode as ancillary data on the command byte; the 8-byte
                // token follows it on the stream. Read it byte-exact.
                let mut token = [0u8; 8];
                let mut reader: &UnixStream = control;
                reader.read_exact(&mut token)?;
                // Safety: freshly received fd, we are its sole owner.
                Ok(Some(Command::Spawn(
                    unsafe { OwnedFd::from_raw_fd(fd) },
                    u64::from_le_bytes(token),
                )))
            }
            None => Err(std::io::Error::other("spawn command without an fd")),
        },
        CMD_KILL => {
            let mut token = [0u8; 8];
            let mut reader: &UnixStream = control;
            reader.read_exact(&mut token)?;
            Ok(Some(Command::Kill(u64::from_le_bytes(token))))
        }
        other => Err(std::io::Error::other(format!("unknown control command {other}"))),
    }
}

/// Collect exited workers so they do not linger as zombies, dropping their
/// token mapping so a later kill for a reaped token is a harmless no-op.
fn reap_children(workers: &mut HashMap<u64, Pid>) {
    loop {
        match waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::StillAlive) | Err(_) => break,
            Ok(status) => {
                if let Some(pid) = status.pid() {
                    workers.retain(|_, &mut p| p != pid);
                }
            }
        }
    }
}
