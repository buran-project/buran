# Buran Application Server — Documentation

Buran is a universal application server. It terminates HTTP, serves static
files, routes requests, and dispatches dynamic requests to **runtime modules**
that embed a language runtime. The runtime is not part of the core: modules are
pluggable, and support for additional languages is planned. **PHP is the first
supported runtime** (the reference module embeds it via `libphp`); the docs use
it in most examples because it ships today, not because Buran is PHP-specific.

If you have used nginx + php-fpm or NGINX Unit before, Buran will feel
familiar: one process handles the network and routing, a pool of workers
executes application code, and everything is driven by a single declarative
YAML config.

## Why Buran

- **One binary, one config.** The server, router, static file handler and
  process supervisor are a single native binary. Applications are described in
  one YAML file.
- **Pluggable embedded runtimes.** Language runtimes are separate modules, not
  baked into the core — more languages can be added without touching the server.
  The PHP module runs PHP in-process inside worker processes through a custom
  SAPI over `libphp` — no FastCGI hop, no separate php-fpm to manage.
- **Static configuration by design.** There is no live admin API. A config
  change means a reload/restart (or, in containers, a new container). This
  keeps the running state predictable and auditable.
- **Safe defaults.** Source files a runtime declares as executable (for PHP:
  `.php`, `.phtml`, …) are never served as static content, even if a `share`
  rule would otherwise match them.
- **Container-native.** Runs correctly as PID 1 (reaps orphans, handles
  `SIGTERM`/`SIGINT` for graceful shutdown). Official runtime images (PHP today)
  ship per version.

## Documentation map

| Document | What is inside |
|----------|----------------|
| [Getting started](getting-started.md) | Run Buran with Docker or from source in a few minutes. |
| [Configuration reference](configuration.md) | Every top-level section: `settings`, `listeners`, `access_log`, env substitution, the status endpoint. |
| [Routing](routing.md) | Routes, `match` conditions, actions, pattern syntax, rewrites and template variables. |
| [Static files](static-files.md) | The `share` action, index files, MIME types, source-leak protection. |
| [Applications & runtimes](applications.md) | Applications, process pools, limits, the PHP module, and how modules work (BWP). |
| [Deployment](deployment.md) | Docker images, distro packages, systemd, running as PID 1. |
| [CLI reference](cli.md) | Command-line flags and environment variables. |

## Core concepts in one minute

A request flows through four kinds of config objects:

```
                       ┌──────────────┐
   TCP :8080  ───────► │  listener    │  binds an address, points at a route
                       └──────┬───────┘
                              ▼
                       ┌──────────────┐
                       │   route      │  ordered list of match → action steps
                       └──────┬───────┘
              ┌───────────────┼────────────────┐
              ▼               ▼                 ▼
        serve static    return 30x/       hand off to an
        (share)         status code       application (worker pool)
                                                 │
                                                 ▼
                                          ┌──────────────┐
                                          │ application  │  runtime module + a
                                          └──────────────┘  pool of worker procs
```

- A **listener** binds a `host:port` and either enters a named route or serves
  the built-in **status endpoint**.
- A **route** is an ordered list of steps. Each step has an optional `match`
  and exactly one terminal `action`. The first step whose `match` succeeds wins.
- An **action** either serves static files (`share`), returns a status code
  (`return`), jumps to another route (`route`), or dispatches to an
  **application** (`application`).
- An **application** binds a runtime **module** (e.g. `php85`) to a document
  root and a pool of worker processes.

See [Routing](routing.md) and [Applications](applications.md) for the details.

## Project layout

Buran is a Rust workspace:

| Crate | Role |
|-------|------|
| `buran` | Main process: CLI, config loading, module checks, supervision. |
| `buran-router` | HTTP/1.1, routing, rewrites, static files, worker dispatch, WebSocket. |
| `buran-config` | Config schema, validation, `${ENV}` substitution. |
| `buran-ipc` | Buran Worker Protocol (BWP): framing and flat request encoding. |
| `buran-worker` | Worker-side SDK for building runtime modules. |
| `buran-php` | PHP runtime module: embedded `libphp` via a custom SAPI. |
| `buran-echo` | Reference event-loop module (concurrent BWP profile). |

The repository is published under the Apache-2.0 license at
<https://github.com/buran-project/buran>.
