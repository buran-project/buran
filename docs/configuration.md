# Configuration reference

Buran is configured by a single YAML file. By default it is read from
`/etc/buran/buran.yaml`; override with `--config <path>`.

The parser is **strict**: unknown fields are rejected, so a typo fails fast at
load time instead of being silently ignored. Validate any config without
starting the server:

```bash
buran --check-config --config /etc/buran/buran.yaml
```

## Top-level structure

```yaml
settings:      # global server settings (optional, sane defaults)
listeners:     # address → route/status bindings (at least one required)
routes:        # named ordered lists of match → action steps
applications:  # named runtime application definitions
access_log:    # optional access log path
error_log:     # optional diagnostic log path (default: stderr)
```

All sections except `listeners` are optional. A minimal static file server:

```yaml
listeners:
  "*:8080":
    route: main
routes:
  main:
    - action:
        share: /var/www$uri
```

## `settings`

Global server settings. Every field has a default.

```yaml
settings:
  listen_threads: 4                 # tokio worker threads; default: CPU count
  modules: /usr/lib/buran/modules   # directory holding runtime module binaries
  http:
    header_read_timeout: 30         # seconds to read the request headers
    body_read_timeout: 30           # seconds of silence between body reads
    send_timeout: 30                # seconds to write the response
    idle_timeout: 180               # seconds a keep-alive connection may idle
    max_body_size: 8388608          # largest request body (bytes), 8 MiB
    min_body_rate: 256              # min body throughput (bytes/s); 0 disables
    max_connections: 4096           # process-wide connection cap; 0 disables
    body_temp_path: /var/tmp/buran  # spill directory for large bodies
    server_version: true            # include the version in the Server header
    static:
      mime_types:
        text/x-buran: [".brn"]      # extra extension → MIME mappings
    websocket:
      idle_timeout: 600             # seconds of two-way silence before close
      max_message_size: 1048576     # largest reassembled client message (bytes)
```

### `settings` reference

| Key | Default | Description |
|-----|---------|-------------|
| `listen_threads` | CPU count | Number of tokio worker threads serving connections. |
| `modules` | `/usr/lib/buran/modules` | Directory holding runtime module binaries. A module `php85` resolves to `<modules>/buran-php85`. |

### `settings.http` reference

| Key | Default | Unit | Description |
|-----|---------|------|-------------|
| `header_read_timeout` | `30` | seconds | Time allowed to read the full request headers. |
| `body_read_timeout` | `30` | seconds | Max silence between successive body reads before the request is cut (a hard stall). |
| `send_timeout` | `30` | seconds | Time allowed to write the response. |
| `idle_timeout` | `180` | seconds | How long a keep-alive connection may sit idle between requests. |
| `max_body_size` | `8388608` | bytes | Hard cap on request body size; larger requests get `413`. |
| `min_body_rate` | `256` | bytes/s | Sustained minimum body throughput after a `body_read_timeout` grace window; a slower client is cut (slow-POST / RUDY defence). `0` disables. |
| `max_connections` | `4096` | count | Process-wide cap on concurrent connections (all listeners). At the cap the server stops accepting; surplus connections wait in the kernel backlog (no fd consumed). **Counts long-lived WebSocket tunnels** — raise it above expected concurrency for WS-heavy apps. `0` disables. |
| `body_temp_path` | `/var/tmp/buran` | path | Directory where oversized bodies spill to a temp file (see below). |
| `server_version` | `true` | bool | Include the Buran version in the `Server` response header. |
| `static.mime_types` | — | map | Extra `MIME → [extensions]` mappings, **added to** the built-in table. |
| `websocket.idle_timeout` | `600` | seconds | Two-way silence before an upgraded tunnel is closed (`1001`). |
| `websocket.max_message_size` | `1048576` | bytes | Largest reassembled client message; larger ones close with `1009`. |

Notes:

- All `http.*` timeouts (and both `websocket.*` values) must be **≥ 1**; `0` is
  rejected because it would either stall or instantly kill every connection.
- **Request body handling.** A body up to ~96 KiB is passed to the worker
  inline. A larger body (but still within `max_body_size`) **spills to a temp
  file** under `body_temp_path`, and the worker receives the file path instead
  of the bytes — so `body_temp_path` must be writable and ideally on fast local
  storage (e.g. tmpfs). Streaming-capable runtimes receive large bodies as a
  frame stream rather than a file. Spill files are created with mode `0600`
  (request bodies may carry uploads, tokens or passwords, so they are never
  world- or group-readable); when an application sets `user`, Buran chowns each
  spill file to that uid so the worker can still open it. Use a directory only
  Buran and its workers can reach.
- **`static.mime_types`** extends, never replaces, the built-in MIME table
  (see [Static files](static-files.md)).
- **WebSocket** connections live outside the regular HTTP budgets: the request
  limits (`limits.response_timeout` / `limits.task_timeout`) and
  `http.idle_timeout` do **not** apply to an upgraded tunnel; only
  `websocket.idle_timeout` does.
- **Keep-alive is HTTP/1.1 only.** An HTTP/1.0 request with
  `Connection: keep-alive` is served, but the connection is closed after the
  response (no HTTP/1.0 persistent connections). Put a modern reverse proxy in
  front if HTTP/1.0 keep-alive matters.

### WebSocket security: gate the upgrade on `Origin`

WebSocket connections are **not** subject to the browser's CORS/same-origin
policy, but the browser still attaches the target site's cookies to them. So a
page on `evil.example` can open `wss://your-app/ws`, and if the endpoint
authenticates by cookie it rides the victim's session — **Cross-Site WebSocket
Hijacking (CSWSH)**.

Like nginx and Apache, Buran does **not** validate `Origin` on the handshake by
default — that policy is deployment-specific and yours to set. Buran gives you
two places to enforce it:

1. **In the route**, match the `origin` header before dispatching the upgrade
   and reject everything else. Browsers always send `Origin` on a WS handshake
   and JavaScript cannot forge it, so an allowlist match is a reliable barrier:

   ```yaml
   routes:
     main:
       - match:
           uri: "/ws"
           headers: { origin: "https://app.example.com" }   # trusted origin(s)
         action: { application: site }
       - match: { uri: "/ws" }        # any other origin (or none)
         action: { return: 403 }
   ```

2. **In the application**, which receives the header as
   `$_SERVER['HTTP_ORIGIN']` and can check it against its own allowlist.

If your WS endpoint carries authentication, one of these is mandatory.

## `listeners`

Each key is a `host:port` address. A listener either enters a **route** or
exposes the **status endpoint** — the two are mutually exclusive.

```yaml
listeners:
  "*:8080":            # "*" = any interface
    route: main
  "127.0.0.1:9000":    # localhost only
    status: true
```

- Host `*` binds all IPv4 interfaces. IPv6 literals must be bracketed:
  `"[::1]:8080"`, `"[::]:8080"` for all IPv6 interfaces. The host is an IP
  literal or `*` — hostnames are not resolved.
- The referenced `route` must exist. `route` + `status: true` together is an
  error; so is a listener with neither.

### The status endpoint

A listener with `status: true` answers two fixed paths and never enters routing.
It still exposes pool topology, so keep it on an internal address:

| Path | Response |
|------|----------|
| `/health` | `{"status":"ok"}` — a cheap liveness probe. |
| `/health/applications` | Pool metrics for every application. |
| anything else | `404` — no metrics are served on unlisted paths. |

Metrics response shape:

```json
{
  "status": "ok",
  "applications": {
    "app": { "workers": 2, "idle": 1, "queued": 0 }
  }
}
```

- `workers` — active worker processes in the pool.
- `idle` — free request slots (total capacity minus what workers hold right
  now); for blocking runtimes this equals the idle worker count.
- `queued` — requests accepted but not yet claimed by any worker.

Use `/health` for container liveness/readiness probes and
`/health/applications` for scraping pool saturation.

## `routes` and `applications`

These have dedicated pages:

- [Routing](routing.md) — the `routes` section, `match`, actions, patterns.
- [Applications](applications.md) — the `applications` section, process pools,
  limits and the PHP module.

## `access_log`

A path to the access log file. The format is Apache/nginx **combined**:

```
127.0.0.1 - - [13/Jul/2026:00:08:32 +0000] "GET /index.php HTTP/1.1" 200 512 "" "curl/8.0"
```

```yaml
access_log: /var/log/buran/access.log
```

- Timestamps are UTC.
- `/dev/stdout` works naturally — useful in containers.
- **Deviation from nginx**, on purpose: the bytes field counts the **whole
  wire response** (headers + body), not body bytes only.
- Writes go through a dedicated task and are flushed per line, so request
  handling never blocks on disk and no lines are lost on container kill.

## `error_log`

Destination for the server's **diagnostic log** — its own `info`/`warn`/`error`
events *and* every worker's `stdout`/`stderr` (PHP warnings, `error_log` output
that PHP sends to stderr). Omit it to write to **stderr** (the container-native
default); set a path to write to a file instead:

```yaml
error_log: /var/log/buran/error.log      # omit -> stderr
```

- The **level** is controlled by the `RUST_LOG` environment variable (default
  `info`), e.g. `RUST_LOG=warn` or `RUST_LOG=buran_router=debug`.
- Writing is **non-blocking**: a background writer drains a bounded queue, so a
  log burst (e.g. an app spewing warnings) never blocks request handling. Under
  sustained backpressure lines are dropped rather than stalling the server.
- `--check-config` and `--modules` always log to stderr and never open this
  file, so validation stays independent of the log directory.
- **A fatal startup error is always printed to stderr**, even when `error_log`
  is a file: errors before the logger is up (bad CLI, unreadable config, an
  `error_log` file that cannot be opened) and the final error that stops the
  process go to stderr so they are never swallowed. The running diagnostic trail
  goes to the file; if the server fails to start, look in stderr for the cause.
- **Log rotation:** Buran holds the file open, so a rename-based `logrotate`
  would leave it writing to the old inode. Use `copytruncate` (or ship the file
  with a log agent instead). Because the writer buffers, a hard crash
  (`panic = "abort"`) may lose the last few unflushed lines — for
  crash-forensics keep the default stderr and let the platform capture it.
- If a PHP app sets its own `error_log` ini to a file, PHP writes there
  directly, bypassing this setting.

## Environment variable substitution

Any string scalar may contain `${NAME}` tokens, expanded from the process
environment **after** YAML parsing (so YAML quoting never interferes):

```yaml
applications:
  app:
    module: ${BURAN_PHP_MODULE}
    environment:
      DATABASE_URL: ${DATABASE_URL}
```

Rules:

- Only the full `${NAME}` form is a token. Bare `$NAME` is passed through
  literally.
- A missing variable is a hard error at load time, reporting the config path
  where it occurred (fail fast, no silent empty strings).
- An unterminated `${` (no closing `}`) is kept literally.

This is how one image can serve every cell of a build matrix — see
`tests/smoke/php.yaml`, which selects the module via `${BURAN_SMOKE_MODULE}`.
