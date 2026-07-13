//! Prototype process (spec 2.3), echo edition. Nothing to boot: the value
//! here is the fork discipline every event-loop module must follow —
//! strictly single-threaded until fork, the child builds its async runtime
//! only after it.
//!
//! Control protocol: the supervisor sends one byte per spawn command with
//! the worker channel fd attached via SCM_RIGHTS. EOF on the control
//! channel means the supervisor is gone -> exit.

use std::io::IoSliceMut;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixDatagram, UnixStream};

use nix::sys::socket::{recvmsg, ControlMessageOwned, MsgFlags};
use nix::sys::wait::{waitpid, WaitPidFlag};
use nix::unistd::{fork, ForkResult, Pid};

use crate::{worker, AppConfig};

pub fn run(control_fd: RawFd, work_fd: RawFd, app: AppConfig) -> ! {
    // Safety: both fds are inherited from the supervisor per the module
    // contract. The work socket is shared by every forked worker: the
    // kernel delivers each request datagram to exactly one of them.
    let control = unsafe { UnixStream::from_raw_fd(control_fd) };
    let work = unsafe { UnixDatagram::from_raw_fd(work_fd) };

    // Privilege drop; group first (setuid drops the right to setgid).
    // Workers inherit the identity via fork.
    if let Some(gid) = app.group_id {
        if let Err(e) = nix::unistd::setgid(nix::unistd::Gid::from_raw(gid)) {
            eprintln!("buran-echo prototype: setgid({gid}) failed: {e}");
            std::process::exit(1);
        }
    }
    if let Some(uid) = app.user_id {
        if let Err(e) = nix::unistd::setuid(nix::unistd::Uid::from_raw(uid)) {
            eprintln!("buran-echo prototype: setuid({uid}) failed: {e}");
            std::process::exit(1);
        }
    }

    loop {
        reap_children();

        let worker_fd = match recv_worker_fd(&control) {
            Ok(Some(fd)) => fd,
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
            Ok(ForkResult::Parent { .. }) => {
                drop(worker_fd); // the child owns its copy
            }
            Err(e) => {
                eprintln!("buran-echo prototype: fork failed: {e}");
                drop(worker_fd);
            }
        }
    }
}

/// Receive one spawn command; `Ok(None)` on clean EOF.
fn recv_worker_fd(control: &UnixStream) -> std::io::Result<Option<OwnedFd>> {
    let mut data = [0u8; 1];
    let mut iov = [IoSliceMut::new(&mut data)];
    let mut cmsg_buf = nix::cmsg_space!([RawFd; 1]);

    let msg = recvmsg::<()>(
        control.as_raw_fd(),
        &mut iov,
        Some(&mut cmsg_buf),
        MsgFlags::empty(),
    )
    .map_err(std::io::Error::from)?;

    if msg.bytes == 0 {
        return Ok(None);
    }

    for cmsg in msg.cmsgs().map_err(std::io::Error::from)? {
        if let ControlMessageOwned::ScmRights(fds) = cmsg {
            if let Some(&fd) = fds.first() {
                // Safety: freshly received fd, we are its sole owner.
                return Ok(Some(unsafe { OwnedFd::from_raw_fd(fd) }));
            }
        }
    }

    Err(std::io::Error::other("spawn command without an fd"))
}

/// Collect exited workers so they do not linger as zombies.
fn reap_children() {
    loop {
        match waitpid(Some(Pid::from_raw(-1)), Some(WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => break,
            Ok(_) => continue,
        }
    }
}
