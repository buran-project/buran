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
    share: /var/www$uri            # one path or a list of candidates
    index: index.html             # directory index file
    types: ["!*.php"]             # extension allow/deny patterns
    follow_symlinks: false        # refuse to serve through symlinks
    serve_sources: false          # opt out of source-leak protection (danger)
```

| Field | Meaning |
|-------|---------|
| `share` | One path template, or a list of candidates tried in order. |
| `index` | File served when the resolved path is a directory. |
| `types` | Pattern set filtering which files may be served (same syntax as [route patterns](routing.md#pattern-syntax)). |
| `follow_symlinks` | When `false`, paths resolving through a symlink are refused. |
| `serve_sources` | Opt out of source-leak protection. Off by default; see below. |

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

`serve_sources: true` opts a specific share out of this protection. It exists
for rare, deliberate cases (e.g. an app that ships `.php` snippets as
downloadable examples). Leave it off unless you are certain.

The `execute` field on an application (see [Applications](applications.md))
adds extra executable extensions — those are likewise excluded from static
serving in shares that fall back to that application.

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
