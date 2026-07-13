# 🚀 Buran

[![Project Status: Active](https://www.repostatus.org/badges/latest/active.svg)](https://www.repostatus.org/#active)
[![Tests](https://github.com/buran-project/buran/actions/workflows/tests.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/tests.yml)
[![Security Scan](https://github.com/buran-project/buran/actions/workflows/scan.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/scan.yml)
[![Release](https://github.com/buran-project/buran/actions/workflows/release.yml/badge.svg)](https://github.com/buran-project/buran/actions/workflows/release.yml)
[![Rust](https://img.shields.io/badge/Rust-1.85+-CE422B?logo=rust&logoColor=white)](https://www.rust-lang.org/)
[![PHP](https://img.shields.io/badge/PHP-7.3_–_8.5-777BB4?logo=php&logoColor=white)](https://www.php.net/)
[![Container](https://img.shields.io/badge/ghcr.io-buran--project%2Fburan-2496ED?logo=docker&logoColor=white)](https://github.com/buran-project/buran/pkgs/container/buran)
[![License](https://img.shields.io/github/license/buran-project/buran)](LICENSE)

**Buran** is a universal application server written in Rust. It terminates HTTP,
serves static files, routes requests, and runs your application code in a pool
of embedded worker processes — one native binary, one declarative YAML config.

If you have used **nginx + php-fpm** or **NGINX Unit**, Buran will feel
instantly familiar: the network and routing live in one process, workers execute
application code, and there is no separate FastCGI daemon to babysit.

> 🌍 **Runtime-agnostic core.** Language runtimes are pluggable modules, not
> baked into the server. **PHP is the first supported runtime** (embedded
> in-process via `libphp`); support for more languages is planned.

## ✨ Why Buran?

- 📦 **One binary, one config** – The server, router, static file handler and
  process supervisor are a single native binary. Everything is described in one
  YAML file.
- ⚡ **Embedded runtimes** – PHP runs in-process inside workers through a custom
  SAPI over `libphp`. No FastCGI hop, no php-fpm to manage.
- 🔌 **Pluggable modules** – Runtimes are separate `buran-<runtime>` binaries.
  Several versions coexist side by side (`buran-php83`, `buran-php84`, …); more
  languages can be added without touching the core.
- 🛡️ **Safe by default** – Source files a runtime declares as executable (for
  PHP: `.php`, `.phtml`, …) are **never** served as static content, even if a
  rule would otherwise match them.
- 🧊 **Static configuration** – No live admin API. A config change means a
  reload/restart (or, in containers, a new container) — running state stays
  predictable and auditable.
- 🐳 **Container-native** – Runs correctly as PID 1 (reaps orphans, graceful
  `SIGTERM`/`SIGINT` shutdown). Official runtime images ship per version.

## 📋 Prerequisites

Pick your path:

- **Docker** – nothing but a container runtime. The official images bundle
  Buran, a PHP module and opcache.
- **From source** – Rust **1.85+**. The PHP module additionally needs a
  `libphp` built with `--enable-embed` plus `php-config` and a C toolchain (the
  core server has **no** PHP dependency).

## ⚡ Quick Start

### 1. Write a config

Create `buran.yaml` — serve static files, fall back to a PHP front controller:

```yaml
settings:
  modules: /usr/lib/buran/modules   # where the images install runtime modules

listeners:
  "*:8080":
    route: main

routes:
  main:
    - action:
        share: /www$uri            # try a real file first
        fallback:
          application: app         # anything else → PHP

applications:
  app:
    module: php85                  # → /usr/lib/buran/modules/buran-php85
    root: /www
    index: index.php
    processes: 2
```

### 2. Run it 🐳

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/buran.yaml:/etc/buran/buran.yaml:ro" \
  -v "$PWD/public:/www:ro" \
  ghcr.io/buran-project/buran:php
```

Open <http://localhost:8080>. Done. 🎉

Not sure which module name to use? List what an image ships:

```bash
docker run --rm ghcr.io/buran-project/buran:php buran --modules
```

### 3. …or build from source 🦀

```bash
git clone https://github.com/buran-project/buran
cd buran

# Core server (no PHP toolchain needed):
cargo run -p buran -- --config examples/buran.yaml
```

### 4. Validate before you deploy ✅

```bash
buran --check-config --config /etc/buran/buran.yaml
```

This checks the schema **and** probes every runtime module for protocol
compatibility, then exits. Perfect for CI.

## 🧭 How it works

A request flows through four kinds of config objects:

```
   TCP :8080  ─►  listener  ─►  route  ─►  action  ─►  application
                (bind addr)   (match →    (share /     (runtime module
                              action)     return /      + worker pool)
                                          app / route)
```

- A **listener** binds a `host:port` and enters a route (or serves the built-in
  status endpoint).
- A **route** is an ordered list of `match → action` steps; the first match wins.
- An **action** serves static files (`share`), returns a code (`return`), jumps
  to another route (`route`), or dispatches to an **application**.
- An **application** binds a runtime **module** to a document root and a pool of
  worker processes.

## 📚 Documentation

Full docs live in [`docs/`](docs/):

| Document | What is inside |
|----------|----------------|
| 📖 [Getting started](docs/getting-started.md) | Run Buran with Docker or from source in minutes. |
| ⚙️ [Configuration reference](docs/configuration.md) | `settings`, `listeners`, `access_log`, env substitution, the status endpoint. |
| 🧭 [Routing](docs/routing.md) | Routes, `match` conditions, actions, pattern syntax, rewrites. |
| 📁 [Static files](docs/static-files.md) | The `share` action, index files, MIME types, source-leak protection. |
| 🧩 [Applications & runtimes](docs/applications.md) | Process pools, limits, the PHP module, how modules work (BWP). |
| 🚢 [Deployment](docs/deployment.md) | Docker images, distro packages, systemd, running as PID 1. |
| 🖥️ [CLI reference](docs/cli.md) | Command-line flags, environment variables, signals. |

## 🐳 Container images

Official images are published to the GitHub Container Registry:

| Image | Contents |
|-------|----------|
| `ghcr.io/buran-project/buran:php` | Buran + latest PHP runtime module + opcache. |
| `ghcr.io/buran-project/buran:php-alpine` | Same, Alpine flavor. |
| `ghcr.io/buran-project/buran:minimal` | Buran only — static files & routing, no runtime module. |

PHP branches **7.3 → 8.5** are built as a matrix, in Debian and Alpine flavors.
Prebuilt **distro packages** (Alpine / Debian / RPM) are attached to every
[release](https://github.com/buran-project/buran/releases). See
[Deployment](docs/deployment.md) for the full list and tags.

## 🛠️ Development

Buran is a Rust workspace:

| Crate | Role |
|-------|------|
| `buran` | Main process: CLI, config loading, module checks, supervision. |
| `buran-router` | HTTP/1.1, routing, rewrites, static files, dispatch, WebSocket. |
| `buran-config` | Config schema, validation, `${ENV}` substitution. |
| `buran-ipc` | Buran Worker Protocol (BWP): framing & flat request encoding. |
| `buran-worker` | Worker-side SDK for building runtime modules. |
| `buran-php` | PHP runtime module: embedded `libphp` via a custom SAPI. |
| `buran-echo` | Reference event-loop module (concurrent BWP profile). |

```bash
cargo build            # build the workspace
cargo test             # run the test suite
cargo run -p buran -- --config examples/buran.yaml
```

## 📄 License

Licensed under the [Apache License 2.0](LICENSE).

---

**Repository**: [github.com/buran-project/buran](https://github.com/buran-project/buran)
**Container registry**: [ghcr.io/buran-project/buran](https://github.com/buran-project/buran/pkgs/container/buran)

♥️ Issues and pull requests are welcome!
