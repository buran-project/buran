# Getting started

There are two common ways to run Buran: a **Docker image** (fastest, PHP
included) or a **build from source** (for development on Buran itself).

> The examples below use PHP because it is the first runtime Buran ships. The
> server itself is runtime-agnostic — support for more languages is planned,
> and the config concepts here carry over to any future runtime module.

## Option A — Docker (recommended)

The official PHP images bundle the Buran binary, a matching PHP runtime module,
and opcache. You supply a config and your application code.

### 1. Write a config

Create `buran.yaml` next to your app. This example serves static files from
`/www` and falls back to a PHP front controller:

```yaml
settings:
  modules: /usr/lib/buran/modules   # where the images install modules

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
    module: php85                  # resolves to /usr/lib/buran/modules/buran-php85
    root: /www
    index: index.php
    processes: 2
```

### 2. Run it

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/buran.yaml:/etc/buran/buran.yaml:ro" \
  -v "$PWD/public:/www:ro" \
  ghcr.io/buran-project/buran:php
```

The default command is `buran --config /etc/buran/buran.yaml`, and the image
exposes port `8080`. Point your browser at <http://localhost:8080>.

> **Config is static.** The images treat configuration as immutable: to change
> routing or applications, restart the container (or start a new one). See
> [Deployment](deployment.md) for image tags and how to add PHP extensions.

### Which module name?

The module suffix must match a binary installed in the modules directory. List
what an image ships:

```bash
docker run --rm ghcr.io/buran-project/buran:php buran --modules
```

Use the printed name (e.g. `php85`) as `module:` in your config. There is no
fuzzy version resolution — the match is exact, which is what lets several PHP
branches coexist in one image family.

## Option B — Build from source

Requirements:

- Rust 1.85 or newer (the workspace `rust-version`).
- For the PHP module only: a `libphp` built with `--enable-embed`, plus
  `php-config` and a C toolchain. The core server has **no** PHP dependency.

### Build and run the core server

```bash
git clone https://github.com/buran-project/buran
cd buran

# Build just the server (no PHP toolchain needed):
cargo build -p buran

# Run against the bundled example config:
cargo run -p buran -- --config examples/buran.yaml
```

The example config listens on `127.0.0.1:8180` (routing) and
`127.0.0.1:9190` (status). It expects the PHP module in `./target/debug`,
so for the PHP routes to work you also need to build the module:

```bash
# libphp with --enable-embed must be discoverable; see docker/*.Dockerfile
# for how the official images derive the link name.
cargo build -p buran-php
```

Now `target/debug/buran-php` exists and the `site` application in
`examples/buran.yaml` can serve `examples/public/`.

### Validate a config without starting

```bash
cargo run -p buran -- --check-config --config examples/buran.yaml
```

This loads and validates the config **and** probes every referenced module
binary (via `--describe`) for BWP compatibility, then exits. Use it in CI and
before a deploy. See the [CLI reference](cli.md).

## Next steps

- Learn the full config surface → [Configuration reference](configuration.md)
- Shape request handling → [Routing](routing.md)
- Tune worker pools and PHP options → [Applications](applications.md)
