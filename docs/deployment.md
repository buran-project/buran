# Deployment

Buran ships two ways: **Docker images** and **distribution packages** (Alpine,
Debian, RPM). Both put the same pieces in place — the `buran` binary, one or
more runtime modules, and a config at `/etc/buran/buran.yaml`.

PHP is the first runtime Buran ships, so the current images and packages are
built around it. As more language runtimes land they follow the same module
model (a versioned `buran-<runtime>` binary in the modules dir), and everything
below applies unchanged.

## Docker images

Images are built from `docker-bake.hcl` at the repo root. There are two
families:

| Family | Contents |
|--------|----------|
| **php** | Buran + a matching PHP runtime module + opcache. Based on the official `php:<ver>-cli` image. |
| **minimal** | Buran only — static files, redirects, routing; no runtime module. |

Each comes in **Debian** and **Alpine** flavors. The PHP family is built for a
matrix of PHP branches (7.3 through 8.5 at time of writing); the latest branch
also gets the bare `php` / `php-alpine` alias tags.

### Running

```bash
docker run --rm -p 8080:8080 \
  -v "$PWD/buran.yaml:/etc/buran/buran.yaml:ro" \
  -v "$PWD/public:/www:ro" \
  ghcr.io/buran-project/buran:php
```

Image conventions:

- Default command: `buran --config /etc/buran/buran.yaml`.
- Exposed port: `8080`.
- Modules installed in `/usr/lib/buran/modules` (so `settings.modules` should
  point there — the shipped default config already does).
- The PHP module binary is version-named (`buran-php85`), which is exactly what
  `module:` resolves to — so multiple PHP versions can coexist.

### Configuration is static

The images treat config as immutable: **a config change means a new
container.** There is no live reload and no admin socket. This is deliberate —
running state stays predictable. Mount your config read-only and roll a new
container to change it.

### Adding PHP extensions

Images carry only opcache. Extend them like any official `php` image:

```dockerfile
FROM ghcr.io/buran-project/buran:php
RUN docker-php-ext-install -j"$(nproc)" pdo_mysql gd
```

The generated INI lands in the PHP scan directory and the Buran SAPI loads it
automatically. See `tests/applications/wordpress/Dockerfile` for a real
example.

### Health checks

Add a `status: true` listener on an internal port and probe `/health`:

```yaml
listeners:
  "*:8080":
    route: main
  "127.0.0.1:9000":
    status: true
```

```dockerfile
HEALTHCHECK CMD curl -fsS http://127.0.0.1:9000/health || exit 1
```

The root path of the status listener returns per-application pool metrics — see
[Configuration › status endpoint](configuration.md#the-status-endpoint).

### Running as PID 1

Buran is a correct init: it reaps orphaned children and shuts down gracefully
on `SIGTERM`. You do **not** need `tini` or `--init`.

## Distribution packages

Reference packaging recipes live under `packaging/` for three ecosystems:

| Path | Ecosystem |
|------|-----------|
| `packaging/platforms/alpine/` | APKBUILD + OpenRC service; PHP branches as subpackages. |
| `packaging/platforms/debian/` | debhelper packaging + systemd. |
| `packaging/platforms/rpm/` | Fedora-style spec + systemd. |
| `packaging/common/` | Shared `buran.yaml`, systemd unit, sysusers, tmpfiles. |

Every recipe is built by CI in a clean container of the target distro on each
release, so the shipped recipes are known to work.

### Installing from a release

Prebuilt packages are attached as assets to every
[GitHub Release](https://github.com/buran-project/buran/releases). Download the
one for your distro and install it with the native package manager — no
repository setup required.

```bash
# Debian / Ubuntu (.deb)
sudo apt install ./buran_0.1.0_amd64.deb        # pulls in dependencies
# or, without dependency resolution:
sudo dpkg -i ./buran_0.1.0_amd64.deb

# Fedora / RHEL (.rpm)
sudo dnf install ./buran-0.1.0-1.x86_64.rpm

# Alpine (.apk)
sudo apk add --allow-untrusted ./buran-0.1.0-r0.apk
```

#### A note on package signatures

The release artifacts are standalone files, not a hosted APT/DNF/apk
repository, so there is no signed repository index to verify them against.
What that means per format when you install a downloaded file:

- **`.deb`** — unsigned, and it does not matter: Debian never signs individual
  `.deb` files (trust normally comes from the repository's signed `Release`).
  `apt install ./…` and `dpkg -i` install it without any signature check.
- **`.rpm`** — unsigned. `dnf install ./file.rpm` installs it (DNF does not
  gpg-check local files by default). On a hardened host with a strict global
  `gpgcheck`, add `--nogpgcheck`.
- **`.apk`** — **this is the one gotcha.** The package is signed inside the
  build container with a throwaway key that is discarded afterwards, so your
  system has no public key to verify it. Install with **`--allow-untrusted`**
  (shown above); a plain `apk add ./…` will fail with a verification error.

None of this blocks a local install — it is the normal flow for packages
pulled straight from a release rather than from a configured repository.

### Runtime model

- The **main process runs as root** (the nginx/fpm master model): it binds
  privileged ports and forks workers.
- **Workers drop privileges** to the `buran` user, created via
  `systemd-sysusers` from `packaging/common/systemd/buran.sysusers.conf`.
- `/etc/buran/buran.yaml` is a **conffile**: package upgrades never overwrite
  your local changes.
- The PHP module binary always carries its version in the name
  (`buran-php84`), letting several PHP branches coexist. On Alpine they are
  shipped as separate subpackages.

### Running as non-root (rootless)

Root is only needed for the privilege-drop model above. Buran runs perfectly as
an unprivileged user — the standard rootless-container case — as long as two
conditions hold:

1. **No application sets `user` or `group`.** Dropping workers to another
   identity requires root. If an application declares `user`/`group` while
   Buran is not root, the server **refuses to start** with a clear error
   (`application user/group is set but buran is not running as root`) — a
   deliberate fail-fast, checked before any traffic is accepted, rather than a
   crashing worker loop. With no `user`/`group`, workers simply inherit the same
   unprivileged identity as the main process.
2. **Listeners bind unprivileged ports (≥ 1024).** Binding a privileged port
   such as `*:80` as a non-root user fails at the OS level (`EACCES`). Use
   `*:8080` and put any `:80`/`:443` termination in front (reverse proxy,
   `CAP_NET_BIND_SERVICE`, or a published-port mapping in Docker).

Nothing else requires root: the modules directory, `body_temp_path` and the
access log are ordinary filesystem permissions, and PID-1 orphan reaping only
activates when Buran actually is PID 1. The bundled `examples/buran.yaml`
(port 8180/8080, no `user`/`group`) starts as an ordinary user as-is.

### systemd

The unit and its `sysusers.d` / `tmpfiles.d` companions are the single source
of truth in `packaging/common/systemd/` and are staged by the Debian and RPM
recipes at build time. Typical lifecycle:

```bash
systemctl enable --now buran
systemctl reload buran     # or restart, depending on the unit
journalctl -u buran
```

Edit `/etc/buran/buran.yaml`, validate with `buran --check-config`, then
restart the service.

### Building packages yourself

Each platform directory has a `ci-build.sh` used by the release workflow. Note
that official distro builds forbid network access, so vendor the crates first:

```bash
cargo vendor
```

The release also publishes a vendor tarball. See `packaging/README.md` for the
full CI notes.
