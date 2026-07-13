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
    body_read_timeout: 30           # seconds to read the request body
    send_timeout: 30                # seconds to write the response
    idle_timeout: 180               # seconds a keep-alive connection may idle
    max_body_size: 8388608          # largest request body (bytes), 8 MiB
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
| `body_read_timeout` | `30` | seconds | Time allowed to read the request body. |
| `send_timeout` | `30` | seconds | Time allowed to write the response. |
| `idle_timeout` | `180` | seconds | How long a keep-alive connection may sit idle between requests. |
| `max_body_size` | `8388608` | bytes | Hard cap on request body size; larger requests get `413`. |
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
  frame stream rather than a file.
- **`static.mime_types`** extends, never replaces, the built-in MIME table
  (see [Static files](static-files.md)).
- **WebSocket** connections live outside the regular HTTP budgets:
  `limits.timeout` and `http.idle_timeout` do **not** apply to an upgraded
  tunnel; only `websocket.idle_timeout` does.

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

- Host `*` binds all interfaces. IPv6 literals work too (`"[::1]:8080"` style
  via `host:port`, e.g. `"::1"` as host).
- The referenced `route` must exist. `route` + `status: true` together is an
  error; so is a listener with neither.

### The status endpoint

A listener with `status: true` answers two things and never enters routing —
keep it on an internal address:

| Path | Response |
|------|----------|
| `/health` | `{"status":"ok"}` — a cheap liveness probe. |
| anything else | Pool metrics for every application. |

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
- `idle` — workers currently idle.
- `queued` — requests parked waiting for a free worker.

Use `/health` for container liveness/readiness probes and the root path for
scraping pool saturation.

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
