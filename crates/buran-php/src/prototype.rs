//! Prototype process (spec 2.3): boots the PHP engine once (opcache SHM is
//! created here), then forks a warm worker per supervisor command. Strictly
//! single-threaded — fork safety depends on it; no tokio, no threads.
//!
//! Control protocol (supervisor -> prototype), one command byte each:
//! - `CMD_SPAWN`: fork a warm worker; the channel fd rides via SCM_RIGHTS.
//! - `CMD_KILL`:  followed by an 8-byte worker token; SIGKILL that worker.
//!
//! The prototype is the workers' parent, so killing by the pid it forked is
//! reuse-safe (the pid stays reserved until it reaps the child) — no pidfd
//! needed. EOF on the control channel means the supervisor is gone -> exit.

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
    /// Fork a warm worker on this channel fd, tagged with the given token.
    Spawn(OwnedFd),
    /// SIGKILL the worker with this token (over budget / wedged).
    Kill(u64),
}

pub fn run(control_fd: RawFd, work_fd: RawFd, app: AppConfig) -> ! {
    // Safety: both fds are inherited from the supervisor per the module
    // contract. The work socket is shared by every forked worker: the
    // kernel delivers each request datagram to exactly one of them.
    let control = unsafe { UnixStream::from_raw_fd(control_fd) };
    let work = unsafe { UnixDatagram::from_raw_fd(work_fd) };

    // Privilege drop before the engine boots; groups first, then gid, then
    // uid (setuid drops the right to change the others). Workers inherit the
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
                    eprintln!("buran-php prototype: user name contains NUL");
                    std::process::exit(1);
                }
            },
            None => nix::unistd::setgroups(&[gid]),
        };
        if let Err(e) = groups_res {
            eprintln!("buran-php prototype: dropping supplementary groups failed: {e}");
            std::process::exit(1);
        }
        if let Err(e) = nix::unistd::setgid(gid) {
            eprintln!("buran-php prototype: setgid({gid}) failed: {e}");
            std::process::exit(1);
        }
    }
    if let Some(uid) = app.user_id
        && let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)) {
            eprintln!("buran-php prototype: setuid({uid}) failed: {e}");
            std::process::exit(1);
        }

    if let Err(e) = worker::boot(&app) {
        eprintln!("buran-php prototype: engine boot failed: {e}");
        std::process::exit(1);
    }

    // token -> child pid, for reuse-safe kills by the parent.
    let mut workers: HashMap<u64, Pid> = HashMap::new();
    let mut next_token: u64 = 1;

    loop {
        reap_children(&mut workers);

        let worker_fd = match recv_command(&control) {
            Ok(Some(Command::Spawn(fd))) => fd,
            Ok(Some(Command::Kill(token))) => {
                if let Some(&pid) = workers.get(&token) {
                    // Parent kill: the pid is this exact child until we reap it.
                    let _ = kill(pid, Signal::SIGKILL);
                }
                continue;
            }
            Ok(None) => std::process::exit(0), // supervisor closed the channel
            Err(e) => {
                eprintln!("buran-php prototype: control channel error: {e}");
                std::process::exit(1);
            }
        };

        let token = next_token;
        next_token += 1;

        // Safety: single-threaded by construction (see module docs).
        match unsafe { fork() } {
            Ok(ForkResult::Child) => {
                drop(control);
                let stream = UnixStream::from(worker_fd);
                let _ = worker::serve(&work, stream, &app, token);
                std::process::exit(0);
            }
            Ok(ForkResult::Parent { child }) => {
                drop(worker_fd); // the child owns its copy
                workers.insert(token, child);
            }
            Err(e) => {
                eprintln!("buran-php prototype: fork failed: {e}");
                drop(worker_fd);
            }
        }
    }
}

/// Receive one control command; `Ok(None)` on clean EOF.
fn recv_command(control: &UnixStream) -> std::io::Result<Option<Command>> {
    let mut cmd = [0u8; 1];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

    // Read the command byte and any attached fd, then drop the iov borrow so
    // the command byte can be inspected.
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
                fd = fds.first().copied();
            }
        }
        (msg.bytes, fd)
    };

    if bytes == 0 {
        return Ok(None);
    }

    match cmd[0] {
        // Safety: freshly received fd, we are its sole owner.
        CMD_SPAWN => match spawn_fd {
            Some(fd) => Ok(Some(Command::Spawn(unsafe { OwnedFd::from_raw_fd(fd) }))),
            None => Err(std::io::Error::other("spawn command without an fd")),
        },
        CMD_KILL => {
            // The 8-byte token follows the command byte on the stream.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn recv_command_parses_kill_with_token() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut msg = vec![CMD_KILL];
        msg.extend_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());
        (&a).write_all(&msg).unwrap();

        match recv_command(&b).unwrap() {
            Some(Command::Kill(token)) => assert_eq!(token, 0x1122_3344_5566_7788),
            _ => panic!("expected a Kill command"),
        }
    }

    #[test]
    fn recv_command_reports_eof() {
        let (a, b) = UnixStream::pair().unwrap();
        drop(a); // supervisor closed the channel
        assert!(recv_command(&b).unwrap().is_none());
    }
}
