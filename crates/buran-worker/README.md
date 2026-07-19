# buran-worker

[![crates.io](https://img.shields.io/crates/v/buran-worker.svg)](https://crates.io/crates/buran-worker)
[![docs.rs](https://img.shields.io/docsrs/buran-worker)](https://docs.rs/buran-worker)
[![license](https://img.shields.io/crates/l/buran-worker.svg)](https://github.com/buran-project/buran/blob/main/LICENSE)
[![Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)

**The worker-side SDK for building runtime modules for the
[Buran](https://github.com/buran-project/buran) application server.**

Buran is an application server whose language runtimes attach as separate
pluggable **module** binaries. A module is the process that runs application
code; this crate implements the worker end of the
[Buran Worker Protocol (BWP)](https://crates.io/crates/buran-ipc), so a
blocking runtime can serve requests without touching protocol bytes. (New to
the architecture? Start with [`buran-ipc`](https://crates.io/crates/buran-ipc).)

## What it does — and what it doesn't

`run()` drives the **BWP request/response loop** on a pair of file descriptors:
the Hello/HelloAck handshake, receiving requests off the shared work queue,
your handler, the batched response write, worker recycling (`max_requests`, à
la FPM `pm.max_requests`) and graceful shutdown. You write one handler:
*request in, response out.*

What it does **not** do: the **process contract** around that loop — how the
server spawns your module's processes and passes it those two file descriptors
(the prototype/fork model, fd inheritance, the `--describe` / control-socket
wiring). That part is currently followed by example rather than provided as
API; the [`buran-php`](https://github.com/buran-project/buran/tree/main/crates/buran-php)
module is the complete, working reference to copy from.

> **Profile.** This SDK is the **blocking** profile: concurrency 1, one request
> at a time. Event-loop runtimes that want many concurrent requests per process
> implement BWP natively on top of [`buran-ipc`](https://crates.io/crates/buran-ipc)
> — see the `buran-echo` reference module.

## The part you write

```rust
use buran_ipc::RequestView;
use buran_worker::{run, Describe, Responder, WorkerError};

// Called once per request: read the request, write the response.
fn handle(req: &RequestView, _flags: u8, resp: &mut Responder) -> Result<(), WorkerError> {
    let _path = req.path()?; // zero-copy — no HTTP re-parsing
    resp.send_headers(200, b"content-type: text/plain\r\n")?;
    resp.send_body(b"hello from a Buran module\n")?;
    resp.finish()
}

fn main() {
    // The server probes capabilities at startup.
    if std::env::args().any(|a| a == "--describe") {
        Describe { runtime: "demo", version: "0.1.0".into(), source_extensions: &[] }.print();
        return;
    }

    // Your process inherits two fds from the server — a shared work socket and a
    // private response stream — plus a recycle limit and worker token. Setting
    // that up is the module contract (see buran-php). Hand them to `run`, which
    // drives the loop and calls `handle` for each request:
    //
    //     run(work, resp, max_requests, token, handle).expect("worker loop");
}
```

`Responder` also offers `flush()` (push a chunk out now), `finish_now()`
(release the client early and keep working — FastCGI `fastcgi_finish_request`
semantics) and `error()`.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
