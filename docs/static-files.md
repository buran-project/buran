# Static files

The `share` action serves files straight from disk. It is used both for asset
directories and, with `fallback`, as the "try a real file first" half of a
front-controller setup.

## Short form

The value is a path template (see [template variables](routing.md#rewrite-and-template-variables)).
Most setups append `$uri` to a document root:

```yaml
- action:
    share: /var/www$uri
```

A request for `/css/app.css` serves `/var/www/css/app.css`.

## Full form

Pass an object to control indexing, extension filtering and symlinks:

```yaml
- action:
    share: /var/www$uri            # a single path template
    index: index.html             # directory index file
    types: ["!*.php"]             # extension allow/deny patterns
    follow_symlinks: false        # refuse to serve through symlinks
    serve_sources: false          # opt out of source-leak protection (danger); or [".php"]
```

| Field | Meaning |
|-------|---------|
| `share` | A single path template. |
| `index` | File served when the resolved path is a directory. |
| `types` | Pattern set filtering which files may be served (same syntax as [route patterns](routing.md#pattern-syntax)). |
| `follow_symlinks` | When `false`, paths resolving through a symlink are refused. |
| `serve_sources` | Opt out of source-leak protection: `false` (default), `true` (all sources), or a list like `[".php"]` (only those). See below. |

## Source-leak protection

This is a safety feature you get for free. Every runtime module declares the
extensions it treats as **executable source** (the PHP module declares `.php`,
`.phtml`, `.phar`). Buran collects these at startup and the static handler
**refuses to serve any file with such an extension**, regardless of what a
`share` rule matches.

That means you do **not** need a defensive `!*.php` in your `types`: a
forgotten filter cannot leak PHP source. The example config relies on exactly
this:

```yaml
# No `!*.php` needed: the php module declares .php/.phtml/.phar as sources
# and the router refuses to serve them as static files by design.
- action:
    share: ./examples/public$uri
    fallback:
      application: site
```

`serve_sources` opts a specific share out of this protection. It exists for
rare, deliberate cases (e.g. an app that ships `.php` snippets as downloadable
examples). Prefer the **least-privilege list form** — `serve_sources: [".php"]`
serves only `.php` raw and keeps every other source extension protected —
over the blanket `serve_sources: true` (all sources). Leave it off unless you
are certain.

Serving source is refused in one case: a share that opts out **and** has a
`fallback` reaching an application is rejected at config load, because it would
serve that application's own source (they share the file tree). Put downloadable
sources under a separate share that has no application fallback.

The `execute` field on an application (see [Applications](applications.md))
adds extra executable extensions — those are likewise excluded from static
serving in shares that fall back to that application.

## What is *not* protected: sensitive files are your responsibility

Source-leak protection only covers what a runtime can tell Buran about —
**executable source extensions**. Buran has no way to know which *other* files
under a share root are sensitive, because that is entirely deployment-specific.
A `share: /var/www$uri` serves **everything** under `/var/www` that is not an
executable source — including `.env`, `.git/`, `*.bak`, `config.yaml`,
`composer.json`, editor swap files, and so on.

Buran deliberately does **not** ship a default dotfile denylist, matching nginx
and Apache (which serve dotfiles by default; Apache blocks only `.ht*`). A blanket
`/\.` deny is wrong here:

- `/.well-known/` is a dotfile path required for **ACME/Let's Encrypt** HTTP-01,
  `security.txt`, and more — a blanket deny breaks certificate issuance.
- A `.git/` directory is sometimes served **on purpose** (dumb HTTP git protocol),
  so even denying `.git` by default would break legitimate setups.

What is sensitive is a property of your deployment, not of Buran — so **denying
it is the administrator's job**. Keep secrets out of the document root, and add
an explicit deny step in front of the `share` for anything that must never be
served. Route patterns support `~regex`, so one step covers a family:

```yaml
routes:
  main:
    # Refuse the files this deployment considers sensitive. Tune the list.
    - match: { uri: "~/\\.(git|env|ht[a-z]*)(/|$)" }
      action: { return: 404 }
    - action:
        share: /var/www$uri
        fallback:
          application: site
```

Return `404` (not `403`) to avoid confirming a file exists. Order matters:
the deny step must come **before** the `share`.

## MIME types

Buran ships a built-in extension → MIME table. Extend or override it under
`settings.http.static.mime_types`:

```yaml
settings:
  http:
    static:
      mime_types:
        text/x-buran: [".brn"]
        application/wasm: [".wasm"]
```

Each entry maps a MIME type to the extensions that should use it.

## Security headers for served content

Like nginx and Apache, Buran does not add security response headers by default —
`content-type` for static files comes from a fixed extension table (never from
request bytes), so there is no MIME-injection vector. But if a `share` serves
**user-uploaded** content, add `X-Content-Type-Options: nosniff` so a browser
cannot sniff an `application/octet-stream` upload as HTML and run injected
script in your origin. Set it (and any other headers like `Content-Security-Policy`
or `X-Frame-Options`) per route with `response_headers`:

```yaml
- match: { uri: "/uploads/*" }
  action:
    share: /var/www$uri
    response_headers:
      x-content-type-options: nosniff
      content-disposition: attachment      # force download rather than render
```
