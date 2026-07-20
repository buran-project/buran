# buran-router

[![part of Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)
[![license](https://img.shields.io/badge/license-Apache--2.0-informational)](https://github.com/buran-project/buran/blob/main/LICENSE)

**The networking and routing engine of the
[Buran](https://github.com/buran-project/buran) application server** — the edge
that turns a TCP connection into either a served file or a dispatched request.

A hand-rolled, `tokio`-based HTTP/1.1 stack with the request smuggling families
closed by construction, kernel-contained static serving, and a worker-pool
dispatcher speaking the [Buran Worker Protocol](../buran-ipc).

- `http1.rs` — HTTP/1.1 parsing (via `httparse`), keep-alive, request/header/
  body budgets, and the response path.
- `routes.rs`, `matching.rs`, `template.rs`, `uri.rs` — the routing engine:
  `match → action` steps, pattern/CIDR matching, rewrites, path normalization.
- `serve_static.rs` — the `share` action: static files under
  `openat2(RESOLVE_IN_ROOT)` containment, with source-leak protection.
- `dispatch.rs` — the worker pool: request/response demultiplexing, streamed
  bodies, liveness probes, and graceful recycling.
- `ws.rs` — WebSocket: the server owns RFC 6455 so workers see whole messages.
- `access_log.rs` — non-blocking combined-format access logging.

> Internal workspace crate — not published to crates.io. Driven by the
> [`buran`](../buran) server binary.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
