# buran

[![part of Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)
[![license](https://img.shields.io/badge/license-Apache--2.0-informational)](https://github.com/buran-project/buran/blob/main/LICENSE)

**The Buran server binary** — the main process of the
[Buran](https://github.com/buran-project/buran) application server.

This crate is the entry point and the supervisor. It loads and validates the
YAML config, probes every runtime **module** for protocol compatibility, starts
a prototype process per application, forks and watches worker pools, and runs
the router in-process. It is built to run correctly as **PID 1** in a container
(reaps orphans, graceful `SIGTERM`/`SIGINT` shutdown).

- `main.rs` — CLI (`--config`, `--check-config`, `--modules`, …), config
  loading, module `--describe` probing, the diagnostic log, and PID-1 signal
  handling.
- `spawn.rs` — the prototype/fork supervision model: worker spawn and respawn,
  fd passing (`SCM_RIGHTS`), privilege drop, and pid-reuse-safe kills.

The networking and routing live in
[`buran-router`](../buran-router); config parsing in
[`buran-config`](../buran-config); runtimes attach as separate module binaries
([`buran-php`](../buran-php), …).

> Internal workspace crate — not published to crates.io. See the
> [repository](https://github.com/buran-project/buran) for the full picture.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
