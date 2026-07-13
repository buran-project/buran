//! Phase-0 benchmark: BWP round-trip time router -> worker -> router.
//!
//! Spawns a module binary with an inherited socketpair fd and measures the
//! full request/response cycle over the placeholder handler, i.e. pure
//! protocol + scheduling overhead without PHP.
//!
//! Usage: bwp-rtt <path-to-module-binary> [requests] [extra module args...]
//! With BWP_RTT_DUMP=1 the first response is printed to stderr.

use std::io::{Read, Write};
use std::os::fd::IntoRawFd;
use std::os::unix::net::{UnixDatagram, UnixStream};
use std::os::unix::process::CommandExt;
use std::process::ExitCode;
use std::time::Instant;

use buran_ipc::{FrameHeader, FrameKind, HelloAck, RequestBuilder, BWP_VERSION, FRAME_HEADER_LEN};

const CHANNEL_FD: i32 = 3;
const WORK_FD: i32 = 4;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(module) = args.next() else {
        eprintln!("usage: bwp-rtt <module-binary> [requests]");
        return ExitCode::FAILURE;
    };
    let requests: u32 = args.next().and_then(|v| v.parse().ok()).unwrap_or(10_000);
    let extra_args: Vec<String> = args.collect();

    let (mut ours, theirs) = UnixStream::pair().expect("socketpair");
    let theirs_fd = theirs.into_raw_fd();
    let (work_ours, work_theirs) = UnixDatagram::pair().expect("work socketpair");
    let work_theirs_fd = work_theirs.into_raw_fd();

    let mut child = {
        let mut cmd = std::process::Command::new(&module);
        cmd.arg("--channel").arg(CHANNEL_FD.to_string());
        cmd.arg("--work").arg(WORK_FD.to_string());
        cmd.args(&extra_args);
        // Safety: dup2 in pre_exec is async-signal-safe; it also clears
        // CLOEXEC on the target fds so the child inherits both channels.
        unsafe {
            cmd.pre_exec(move || {
                if libc_dup2(theirs_fd, CHANNEL_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc_dup2(work_theirs_fd, WORK_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().expect("spawn module")
    };
    drop_fd(theirs_fd);
    drop_fd(work_theirs_fd);

    // Handshake: Hello <- worker, HelloAck -> worker.
    let (hello, payload) = read_frame(&mut ours);
    assert_eq!(hello.kind, FrameKind::Hello, "expected Hello");
    assert_eq!(&payload[..4], buran_ipc::BWP_MAGIC, "bad magic");
    // Grant concurrency 1: blocking profile, one in-flight request per worker.
    let ack = HelloAck { version: BWP_VERSION, concurrency: 1 }.encode();
    write_frame(&mut ours, &FrameHeader::new(FrameKind::HelloAck, 0, ack.len() as u32), &ack);

    // Pre-build one request payload (typical small GET).
    let mut builder = RequestBuilder::new();
    builder
        .method(b"GET")
        .path(b"/index.php")
        .target(b"/index.php?x=1")
        .query(b"x=1")
        .version(b"HTTP/1.1")
        .remote_addr(b"127.0.0.1")
        .server_name(b"localhost")
        .field(b"host", b"localhost")
        .field(b"user-agent", b"bwp-rtt/0.1")
        .field(b"accept", b"*/*")
        .preread_body(b"");
    let request = builder.finish();

    if std::env::var_os("BWP_RTT_DUMP").is_some() {
        dump_round_trip(&work_ours, &mut ours, 0, &request);
    }

    // Warmup.
    for i in 1..101u32 {
        round_trip(&work_ours, &mut ours, i, &request);
    }

    let mut samples = Vec::with_capacity(requests as usize);
    let total = Instant::now();
    for i in 0..requests {
        let t = Instant::now();
        round_trip(&work_ours, &mut ours, 101 + i, &request);
        samples.push(t.elapsed());
    }
    let elapsed = total.elapsed();

    samples.sort_unstable();
    let p = |q: f64| samples[((samples.len() - 1) as f64 * q) as usize];
    println!(
        "bwp-rtt: {} req in {:.2?} ({:.0} req/s)\n  min {:.1?}  p50 {:.1?}  p99 {:.1?}  max {:.1?}",
        requests,
        elapsed,
        requests as f64 / elapsed.as_secs_f64(),
        samples[0],
        p(0.50),
        p(0.99),
        samples[samples.len() - 1],
    );

    drop(ours); // closes the channel; worker exits cleanly
    let _ = child.wait();
    ExitCode::SUCCESS
}

fn send_work(work: &UnixDatagram, id: u32, request: &[u8]) {
    let mut msg = Vec::with_capacity(FRAME_HEADER_LEN + request.len());
    msg.extend_from_slice(
        &FrameHeader::new(FrameKind::Request, id, request.len() as u32).encode(),
    );
    msg.extend_from_slice(request);
    work.send(&msg).expect("send work datagram");
}

fn dump_round_trip(work: &UnixDatagram, stream: &mut UnixStream, id: u32, request: &[u8]) {
    send_work(work, id, request);
    loop {
        let (fh, payload) = read_frame(stream);
        match fh.kind {
            FrameKind::ResponseHeaders => {
                eprintln!("-- status {} --\n{}", fh.aux, String::from_utf8_lossy(&payload));
            }
            FrameKind::ResponseBody => eprint!("{}", String::from_utf8_lossy(&payload)),
            FrameKind::End => break,
            FrameKind::Error => {
                eprintln!("-- worker error: {}", String::from_utf8_lossy(&payload));
                break;
            }
            _ => {}
        }
    }
}

fn round_trip(work: &UnixDatagram, stream: &mut UnixStream, id: u32, request: &[u8]) {
    send_work(work, id, request);
    loop {
        let (fh, _payload) = read_frame(stream);
        match fh.kind {
            FrameKind::End => break,
            FrameKind::Error => panic!("worker error on request {id}"),
            _ => {}
        }
    }
}

fn write_frame(stream: &mut UnixStream, header: &FrameHeader, payload: &[u8]) {
    stream.write_all(&header.encode()).expect("write header");
    if !payload.is_empty() {
        stream.write_all(payload).expect("write payload");
    }
}

fn read_frame(stream: &mut UnixStream) -> (FrameHeader, Vec<u8>) {
    let mut head = [0u8; FRAME_HEADER_LEN];
    stream.read_exact(&mut head).expect("read header");
    let header = FrameHeader::decode(&head).expect("decode header");
    let mut payload = vec![0u8; header.payload_len as usize];
    stream.read_exact(&mut payload).expect("read payload");
    (header, payload)
}

fn libc_dup2(old: i32, new: i32) -> i32 {
    // Minimal libc shim to avoid a direct libc dependency in this bin.
    unsafe extern "C" {
        fn dup2(oldfd: i32, newfd: i32) -> i32;
    }
    unsafe { dup2(old, new) }
}

fn drop_fd(fd: i32) {
    unsafe extern "C" {
        fn close(fd: i32) -> i32;
    }
    unsafe {
        close(fd);
    }
}
