# Routing

A **route** is a named, ordered list of steps. Each step has an optional
`match` and exactly one terminal `action`. Buran evaluates steps top to bottom
and takes the action of the **first** step whose `match` succeeds. A step with
no `match` always matches — put it last as a catch-all.

```yaml
routes:
  main:
    - match:
        uri: "/assets/*"
      action:
        share: /var/www$uri

    - match:
        uri: "/old"
      action:
        return: 301
        location: /

    - action:                 # catch-all: no match block
        application: app
```

A listener points at a route by name; `route` actions let routes chain.

## The `match` block

All present conditions must hold (logical AND). Every field is optional; an
empty `match` matches everything.

```yaml
match:
  method: GET                        # or a list: [GET, HEAD]
  host: "example.com"                # Host header
  uri: ["/api/*", "!/api/private*"]  # request path (no query string)
  query: "debug=1"                   # raw query string
  arguments:                         # parsed query params (decoded)
    page: "*"
  headers:                           # request headers (case-insensitive names)
    x-debug-token: "sesame"
  source: ["127.0.0.0/8", "::1"]     # client IP / CIDR
```

| Field | Matches against | Notes |
|-------|-----------------|-------|
| `method` | HTTP method | Case-sensitive; methods are uppercase. |
| `host` | `Host` header | |
| `uri` | decoded request path | Query string excluded. |
| `query` | raw query string | The part after `?`, unparsed. |
| `arguments` | individual query parameters | Map of `name → pattern` on decoded values. |
| `headers` | request headers | Map of `name → pattern`; header names are case-insensitive. |
| `source` | client IP address | Exact IPs and CIDR blocks (see below). |

Single values and lists are interchangeable everywhere: `method: GET` and
`method: [GET, HEAD]` are both valid.

## Pattern syntax

`method`, `host`, `uri`, `query`, and the values of `headers`/`arguments` all
use the same pattern language. A single pattern can be:

| Form | Meaning | Example |
|------|---------|---------|
| exact | literal equality | `GET`, `/login` |
| `*` | matches anything | `*` |
| prefix `abc*` | starts with `abc` | `/api/*` |
| suffix `*abc` | ends with `abc` | `*.php` |
| circumfix `a*z` | starts with `a`, ends with `z` | `img_*.png` |
| multi-wildcard | several `*` | `/a/*/b/*` (compiled to a regex) |
| regex `~...` | full regular expression | `~^/user/[0-9]+$` |
| negation `!...` | inverts any of the above | `!*.php` |

Array semantics (OR-set): a value matches the array if it hits **at least one
positive** pattern **and no negative** one. An array of only-negative patterns
matches everything except what those patterns exclude:

```yaml
uri: ["/api/*", "!/api/private*"]   # under /api, but not /api/private…
uri: ["!*.php"]                     # anything that is not a .php path
```

### `source` — IP matching

`source` takes exact IPs and CIDR blocks, IPv4 or IPv6, with `!` negation:

```yaml
source: ["10.0.0.0/8", "!10.0.0.1", "::1"]
```

- A plain address is an exact host (`/32` for IPv4, `/128` for IPv6).
- Same OR/negation rules as patterns: an empty set (or only-negatives) allows
  all except the negated entries.
- Family mismatches never match (an IPv4 rule never matches an IPv6 client).

## Actions

An action has **exactly one terminal**:

| Terminal | Effect |
|----------|--------|
| `share` | Serve static files from disk. See [Static files](static-files.md). |
| `application` | Dispatch to an application (worker pool). |
| `route` | Jump to another named route (chaining). |
| `return` | Return an HTTP status code immediately. |

Plus optional **modifiers**: `rewrite`, `response_headers`, `location`, and
`fallback`. Supplying zero or more than one terminal is a validation error.

### `return`

Return a status code directly, optionally with a redirect target:

```yaml
- match:
    uri: "/old"
  action:
    return: 301
    location: /new          # only valid with a 3xx code
```

- The code must be in `100..=599`.
- `location` requires a `3xx` `return` code, and is meaningless without
  `return`.

### `application`

Hand the request to an application. The value is either a **name** (a key in
`applications`) or an **inline** anonymous application definition:

```yaml
# by name
- action:
    application: app

# inline (compose/k8s style, no YAML anchors needed)
- action:
    application:
      module: php85
      root: /var/www
      index: index.php
      processes: 2
```

Inline applications are extracted into the global application set under a
generated name derived from the route position.

### `route`

Delegate to another route — useful for grouping and de-duplication:

```yaml
routes:
  main:
    - match: { host: "api.example.com" }
      action:
        route: api
  api:
    - action:
        application: api_app
```

### `rewrite` and template variables

`rewrite` changes the path (and optionally query) used by the terminal action,
without changing what the client sees. `REQUEST_URI` handed to the application
keeps the **original** target (CGI semantics).

```yaml
- match:
    uri: "/page/*"
  action:
    rewrite: /index.php?page=$uri
    application: site
    response_headers:
      x-rewritten: "1"
      x-powered-by: null      # strip whatever the app set
```

Templates (`rewrite`, `location`) support these variables, in `$var` or
`${var}` form:

| Variable | Value |
|----------|-------|
| `$uri` | current decoded path |
| `$args` / `$query` | current query string |
| `$host` | `Host` header |
| `$method` | request method |
| `$remote_addr` | client IP address |

Use the braced form to butt a variable against following word characters:
`${uri}_suffix`.

### `response_headers`

Add or remove response headers on the way out:

```yaml
response_headers:
  x-frame-options: DENY     # set (or overwrite)
  x-powered-by: null        # remove a header the app produced
```

A `null` value strips the header; a string sets it.

### `fallback`

Only valid together with `share`. When the static serve fails with a `40x`
(typically a missing file), the `fallback` action runs instead. A fallback is
a **full action** — it can dispatch to an application or even re-enter routing
via `route`:

```yaml
- action:
    share: /var/www$uri     # try a real file
    fallback:
      application: app       # otherwise, front controller
```

This is the standard "try static files, everything else to PHP" pattern.

## A worked example

From `examples/buran.yaml`:

```yaml
routes:
  main:
    # static assets straight from disk
    - match:
        uri: ["/assets/*", "*.ico"]
      action:
        share: ./examples/public$uri

    # permanent redirect
    - match:
        uri: "/old"
      action:
        return: 301
        location: /

    # pretty URLs → front controller
    - match:
        uri: "/page/*"
      action:
        rewrite: /index.php?page=$uri
        application: site

    # guarded debug endpoint: GET, localhost, secret header
    - match:
        uri: "/debug"
        method: GET
        source: ["127.0.0.0/8", "::1"]
        headers:
          x-debug-token: "sesame"
      action:
        return: 204

    # try static, fall back to PHP
    - action:
        share: ./examples/public$uri
        fallback:
          application: site
```
