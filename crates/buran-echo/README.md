# buran-echo

[![part of Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)
[![license](https://img.shields.io/badge/license-Apache--2.0-informational)](https://github.com/buran-project/buran/blob/main/LICENSE)

**The reference event-loop module for the
[Buran](https://github.com/buran-project/buran) application server** — the
executable spec for the *concurrent* profile of the
[Buran Worker Protocol](../buran-ipc).

It serves no language: it echoes requests back. Its job is to show — and prove
in CI — how an event-loop runtime (Node, Go, PHP TrueAsync, …) should talk to
Buran: unbounded concurrency declared in `Hello`, many claimed requests at
once, streamed request bodies, and graceful `Retire`.

Unlike [`buran-php`](../buran-php), which uses the blocking-profile
[`buran-worker`](../buran-worker) SDK, this module implements BWP **natively**
on top of [`buran-ipc`](../buran-ipc) with its own async runtime after the
fork — which is exactly the pattern such runtimes must follow.

- `main.rs` — the module CLI (`--describe`, `--prototype`, `--channel`).
- `prototype.rs` — the fork discipline (single-threaded until fork, then build
  the async runtime in the child).
- `worker.rs` — the concurrent BWP loop: interleaved frames, many in-flight
  requests, body streaming.

> Internal workspace crate — not published to crates.io. Reference/reading
> material, not a shipped runtime.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
