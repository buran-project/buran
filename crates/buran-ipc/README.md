# buran-ipc

[![crates.io](https://img.shields.io/crates/v/buran-ipc.svg)](https://crates.io/crates/buran-ipc)
[![docs.rs](https://img.shields.io/docsrs/buran-ipc)](https://docs.rs/buran-ipc)
[![license](https://img.shields.io/crates/l/buran-ipc.svg)](https://github.com/buran-project/buran/blob/main/LICENSE)
[![Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)

**The wire protocol of the [Buran](https://github.com/buran-project/buran)
application server** — the bytes spoken between the server and the runtime
**modules** that run application code.

This crate is *just the protocol*: framing and a flat, zero-copy request
encoding. No transport, no runtime, no async — only the definition of what
goes on the wire. It is the shared vocabulary that both the server and the
[`buran-worker`](https://crates.io/crates/buran-worker) SDK speak.

## Where this fits

Buran is an application server. If you have run **nginx + FastCGI**, the shape
is familiar — except the workers run **in-process under the server's
supervision**, and language runtimes attach as separate pluggable **module**
binaries rather than being compiled in. The server does the networking,
routing and static files; a module runs the application code.

The **Buran Worker Protocol (BWP)** is how the server hands a parsed HTTP
request to a module and streams the response back. Two ideas keep it cheap:

- **Flat request encoding.** A request is one contiguous blob with an offset
  table. The worker never re-parses HTTP — it reads `method`, `path`, headers,
  `content_length`, etc. straight out of the buffer, without copying.
- **Kernel-arbitrated work queue.** Requests arrive on a shared `SOCK_DGRAM`
  socket inherited by every worker of an application; the kernel wakes exactly
  one idle worker per request — no round-trip through the server to pick one.

The protocol serves both **blocking** runtimes (one request at a time) and
**event-loop** runtimes (many concurrent requests per process), with a small
concurrency contract, graceful `Retire`, streamed request bodies, and a
WebSocket-upgrade path where the server owns RFC 6455 (masking, fragment
reassembly, ping/pong) and the worker only sees whole messages. The full
contract is in the [crate docs](https://docs.rs/buran-ipc).

## What's in the crate

| Item | Purpose |
|------|---------|
| `FrameHeader`, `FrameKind` | The fixed 16-byte frame header and frame kinds (`Claim`, `ResponseHeaders`, `WsMessage`, …). |
| `RequestView` | Zero-copy reader over the flat request blob: `method()`, `path()`, `query()`, header fields, `content_length()`, … |
| `RequestBuilder` | The encoder side (used by the server; handy in tests). |
| `Hello` / `HelloAck` | The concurrency + capability handshake. |
| `BWP_VERSION`, flags & caps | `FLAG_BODY_STREAM`, `CAP_WEBSOCKET`, `WS_OP_*`, `PONG_*`, … |

## Reading a request

```rust
use buran_ipc::RequestView;

fn log_request(payload: &[u8]) -> Result<(), buran_ipc::BwpError> {
    let req = RequestView::parse(payload)?;
    println!(
        "{} {}  ({} header field(s), {} body bytes)",
        String::from_utf8_lossy(req.method()?),
        String::from_utf8_lossy(req.path()?),
        req.fields_count(),
        req.content_length(),
    );
    Ok(())
}
```

All integers are little-endian; the crate docs describe the full frame and
blob layout.

## Building a module?

You rarely use `buran-ipc` on its own. To write a **blocking** runtime module,
reach for [`buran-worker`](https://crates.io/crates/buran-worker), which drives
the worker loop for you. Use this crate directly when you implement the
protocol yourself — e.g. an event-loop runtime handling many requests at once
(the `buran-echo` module in the repository is the reference for that).

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
