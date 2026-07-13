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
  response_timeout: 60   # seconds the router waits for worker output while a client waits; default 60
  task_timeout: 300      # seconds a worker may spend on one task total (incl. background); default 300
  requests: 500          # recycle the worker after N requests (0 = never); default 0
```

- `response_timeout` — how long the router waits for the worker's next output
  while a client is attached (like nginx `fastcgi_read_timeout`). Exceeded: the
  client gets `504` and the worker is told to abort — but the worker is **not**
  killed.
- `task_timeout` — the total wall-clock a worker may spend on one task, counting
  background work after `fastcgi_finish_request` (like php-fpm
  `request_terminate_timeout`, but wall-clock and background-inclusive). Exceeded:
  the task is aborted; the worker is killed only if it refuses to wind down. Must
  be `>= response_timeout`. Set it above the script's `max_execution_time` so PHP
  bails a CPU runaway itself first (its timer counts CPU time, not sleep/IO, so it
  cannot bound background jobs — `task_timeout` is the real wall-clock backstop).
- `requests` mirrors php-fpm's `pm.max_requests`: after the N-th response the
  worker exits cleanly and the pool replaces it, capping the impact of slow leaks.

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

### Response framing & streaming (SSE, progressive output)

Buran follows the **canonical Apache/mod_php contract** here — the script
controls framing, not the server:

- **No `flush()` — buffered.** The response is collected until the script
  returns and sent with an exact `Content-Length`, in one shot. This is the
  common case (a rendered page, a JSON payload).
- **`flush()` (or `ob_flush()` + `flush()`) — streamed.** The flush pushes
  whatever has been written so far to the client immediately; the router
  switches that response to chunked transfer and forwards every subsequent
  write as it arrives. This is what Server-Sent Events, long-polling and
  progressive rendering need — and it means `flush()` deliberately gives up
  `Content-Length`, exactly as it does under Apache.
- **Oversized buffered responses.** A response that grows past **256 KiB**
  without a `flush()` is streamed chunked rather than held whole in memory —
  the same way Apache's core output filter flushes on a full buffer. Below the
  threshold you always get `Content-Length`.

nginx + php-fpm differs: it re-buffers the FastCGI response and by default
absorbs the script's `flush()`, so the client framing is nginx's decision, not
the app's. Buran stays with the older, more predictable PHP semantics.

```php
header('Content-Type: text/event-stream');
while (true) {
    echo "data: " . time() . "\n\n";
    flush();               // delivered to the client now
    sleep(1);
}
```

Client disconnects surface as a normal **PHP connection abort**, so the usual
controls apply:

- By default (`ignore_user_abort(false)`) the script is aborted when the client
  goes away — `register_shutdown_function` still runs.
- With `ignore_user_abort(true)` the script keeps running after a disconnect
  (e.g. to finish a write), and `connection_aborted()` returns `1`.

A streaming response with no output for longer than `settings.http.idle_timeout`
is closed and the worker released.

> **Pool sizing.** In the blocking model a streaming request occupies its
> worker for the whole time it streams — exactly like php-fpm. A pool of *N*
> workers serves at most *N* concurrent streams, so size `processes` for the
> expected number of long-lived connections (SSE especially).

### Behavior notes

A few intentional behaviors worth knowing about:

- **`php_sapi_name()` returns `cli-server`.** The Buran SAPI reports the name
  `cli-server` (it lets opcache's SAPI whitelist accept the runtime). Apps or
  libraries that branch on the SAPI name — some frameworks special-case PHP's
  built-in dev server — will see `cli-server`, not `fpm-fcgi`.
- **Directory redirects are `http://`.** A request for a directory without a
  trailing slash gets a `301` to `http://<Host>/…`. Behind a TLS terminator
  this is an `https → http` downgrade in the `Location`. For now, avoid relying
  on directory-redirects behind TLS (link to the trailing-slash form
  directly); `X-Forwarded-Proto` awareness is planned.
- **`max_execution_time` is CPU time, not wall-clock.** It does not count
  `sleep()`/IO, so it cannot bound a background job — that is what
  `limits.task_timeout` is for (see [limits](#limits)). Set
  `max_execution_time` below `limits.task_timeout` so PHP kills a CPU runaway
  cleanly first.

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
- Streaming: a worker sends a `Flush` frame to make the router forward buffered
  output immediately (chunked). When a client disconnects, the router sends an
  `Abort` frame for that request; the blocking SDK surfaces it as a failed
  write from `send_body`/`flush` so the runtime can abort (PHP user-abort).

Two reference implementations live in the tree:

- `buran-worker` — the SDK for the **blocking** profile (what PHP uses).
- `buran-echo` — a reference **event-loop** module showing the concurrent
  profile.

A module is a standalone binary that answers `--describe` (JSON: protocol
version, runtime name, and its executable source extensions) and then runs the
worker loop. The `source_extensions` it reports are what the router refuses to
serve statically — see [Static files](static-files.md).
