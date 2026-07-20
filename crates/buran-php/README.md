# buran-php

[![part of Buran](https://img.shields.io/badge/part%20of-Buran-c18b57)](https://github.com/buran-project/buran)
[![license](https://img.shields.io/badge/license-Apache--2.0-informational)](https://github.com/buran-project/buran/blob/main/LICENSE)

**The PHP runtime module for the
[Buran](https://github.com/buran-project/buran) application server** — and the
reference for how a real runtime plugs in.

Buran runs application code in pluggable **module** binaries; this is the first
one. It embeds **`libphp`** in-process through a custom SAPI and serves requests
over the [Buran Worker Protocol](../buran-ipc), built on the blocking-profile
SDK [`buran-worker`](../buran-worker). No FastCGI hop, no external php-fpm.

- `main.rs` — the module CLI (`--describe`, `--prototype`, `--channel`).
- `prototype.rs` — the fork discipline: boot the engine once (opcache SHM),
  drop privileges, fork warm workers on command.
- `worker.rs` — the per-request lifecycle: build `$_SERVER`/superglobals from
  the request, run the script, stream the response.
- `sapi_shim.c`, `embed_shim.c`, `build.rs` — the C SAPI glue and the `libphp`
  link (needs `php-config` and a `libphp` built with `--enable-embed`).

Because opcache and extensions load through the normal PHP INI scan directory,
you extend a Buran PHP image exactly like an official `php` one. See
[docs/applications.md](https://github.com/buran-project/buran/blob/main/docs/applications.md).

> Internal workspace crate — not published to crates.io. Shipped as the
> `buran-php<version>` module binary inside the official images.

## License

Licensed under the [Apache License 2.0](https://github.com/buran-project/buran/blob/main/LICENSE).
Part of the [Buran project](https://github.com/buran-project/buran).
