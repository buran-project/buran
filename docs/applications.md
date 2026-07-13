# Applications & runtimes

An **application** binds a runtime **module** (e.g. PHP) to a document root and
a pool of worker processes. Routes dispatch dynamic requests to applications
via the `application` action.

```yaml
applications:
  site:
    module: php85
    root: /var/www
    index: index.php
    processes: { max: 8, spare: 1, idle_timeout: 2 }
    limits:
      timeout: 30
      requests: 500
    options:
      admin:
        opcache.enable: "1"
        memory_limit: 256M
```

## Module resolution

`module` is the binary suffix. `module: php85` resolves to
`<settings.modules>/buran-php85` — an **exact** match, no version fuzzing by
design. This is what lets several PHP branches live side by side in one modules
directory (`buran-php83`, `buran-php84`, `buran-php85`).

At startup Buran checks every referenced module: the binary must exist and
answer `--describe` with a compatible protocol version, or the server refuses
to start. List what is installed:

```bash
buran --modules
```

Module names must be non-empty `[a-z0-9_-]`.

## Application fields

| Field | Meaning |
|-------|---------|
| `module` | Runtime module suffix (required). |
| `root` | Document root for the application. |
| `script` | A fixed script to run for every request (instead of path-based resolution). |
| `index` | Directory index file (e.g. `index.php`). |
| `user` / `group` | Drop worker privileges to this user/group. |
| `working_directory` | Working directory for worker processes. |
| `execute` | Extra extensions the module must treat as executable (e.g. legacy "PHP in `.html`"). Also excluded from static serving. |
| `environment` | Environment variables passed to workers. |
| `processes` | Process pool sizing (see below). |
| `concurrency` | Cap on concurrent requests per worker (event-loop runtimes only). |
| `queue` | Request queue behaviour when all workers are busy. |
| `limits` | Per-request timeout and worker recycling. |
| `options` | Module-specific options, passed through verbatim (validated by the module). |

`execute` entries must look like `.html` (start with a dot, at least one more
character).

## Process pools

`processes` is either a fixed count or a dynamic pool.

**Fixed** — a constant number of workers:

```yaml
processes: 4
```

**Dynamic** — scale between spare-idle and a maximum:

```yaml
processes:
  max: 8            # hard ceiling on worker count
  spare: 1          # idle workers to keep warm (default 0)
  idle_timeout: 20  # seconds before an extra idle worker is retired (default 20)
```

Constraints: `max ≥ 1`, and `spare ≤ max`.

### `concurrency`

Cap on concurrent requests handled by a single worker process. The effective
value is `min(what the module declared, this cap)`:

- **Blocking runtimes** (like the standard PHP module) always run at
  concurrency **1** — one request per worker at a time — no matter what you
  set. Scale them with more `processes`.
- **Event-loop runtimes** (see `buran-echo`) can handle many concurrent
  requests per worker; `concurrency` bounds that.

Omit it to trust the module's own declaration.

### `queue`

When every worker is busy, requests park in a queue instead of being rejected
immediately:

```yaml
queue:
  max: 24000     # cap on parked requests (memory guard); default 24000
  timeout: 15    # seconds a request may wait before an instant 503; default 15
```

Parked requests are cheap, so `max` mainly bounds memory. `max ≥ 1`.

### `limits`

Per-request safety limits:

```yaml
limits:
  timeout: 30    # seconds a worker may spend on one request before SIGKILL + respawn
  requests: 500  # recycle the worker after N requests (0 = never); default 0
```

`requests` mirrors php-fpm's `pm.max_requests`: after the N-th response the
worker exits cleanly and the pool replaces it, which caps the impact of slow
memory leaks. `timeout` is the hard runaway guard — the worker is killed and
respawned; unconsumed requests stay queued for other workers.

## The PHP module

The `buran-php` module embeds `libphp` through a custom SAPI, so PHP executes
in-process inside each worker — no FastCGI hop, no separate php-fpm.

### PHP INI options

`options` under a PHP application carries INI settings, split into `admin`
(cannot be overridden at runtime by the script, like php-fpm `php_admin_value`)
and `user` (script-overridable defaults):

```yaml
applications:
  site:
    module: php85
    root: /var/www
    index: index.php
    options:
      admin:
        opcache.enable: "1"
        opcache.memory_consumption: 192
        memory_limit: 256M
      user:
        display_errors: "0"
```

Values are passed through to the module verbatim and validated by the module
itself (at `--check-config` time). Because opcache and extensions load through
the normal PHP INI scan directory, you extend a PHP image exactly like an
official `php` one (`docker-php-ext-install` / `pecl`) — see
[Deployment](deployment.md).

### Adding PHP extensions

The images ship only opcache. Add extensions the standard way in a derived
image:

```dockerfile
FROM ghcr.io/buran-project/buran:php
RUN docker-php-ext-install -j"$(nproc)" pdo_mysql
```

The generated INI lands in the scan dir and the Buran SAPI picks it up like any
other PHP.

## How modules work (BWP)

Runtime modules communicate with the router over the **Buran Worker Protocol
(BWP)** — a small binary framing protocol (`buran-ipc`). You only need this if
you are writing your own runtime module; using PHP requires none of it.

Key points:

- The main process forks a **prototype** per application, which forks the
  actual **workers**. Fork is only safe while single-threaded, which is why the
  blocking worker SDK is strictly single-threaded.
- Requests arrive on a **shared datagram socket** inherited by every worker of
  the application. The kernel wakes exactly one idle worker per request — no
  router round-trip to pick a worker.
- Responses go back over the worker's private stream, batched into a single
  write per request.
- Workers declare their **concurrency** and **capabilities** in a `Hello`
  handshake. The blocking profile pins concurrency to 1; event-loop runtimes
  implement BWP natively and can go higher.
- Crash isolation: a datagram not yet consumed survives a worker death and is
  served by the remaining workers; only the request in flight fails.

Two reference implementations live in the tree:

- `buran-worker` — the SDK for the **blocking** profile (what PHP uses).
- `buran-echo` — a reference **event-loop** module showing the concurrent
  profile.

A module is a standalone binary that answers `--describe` (JSON: protocol
version, runtime name, and its executable source extensions) and then runs the
worker loop. The `source_extensions` it reports are what the router refuses to
serve statically — see [Static files](static-files.md).
