# Packaging

Reference recipes for distribution packages (spec section 2.15). Two
audiences:

- **Distro maintainers**: take the recipe for your ecosystem, set a real
  maintainer and submit — every recipe is built by CI in a clean container
  of the target distro on each release, so it is known to work.
- **End users**: prebuilt packages are attached to every GitHub Release —
  install them into your own containers/hosts with the native package
  manager.

## Layout

```
platforms/alpine/APKBUILD     subpackages buran-php83/84/85 (parallel PHP branches)
platforms/alpine/buran.initd  OpenRC service
platforms/debian/             debhelper packaging; module for the release's PHP
platforms/rpm/buran.spec      Fedora-style spec; module for the release's PHP
common/systemd/               buran.service, sysusers.d — single source of
                              truth, staged by the debian/rpm recipes at build
common/buran/buran.yaml       default config shipped by all packages
```

## Conventions

- The PHP module binary always carries the version in its name
  (`buran-php84`) — it is what `module:` in buran.yaml resolves to, and it
  lets several PHP branches coexist (Alpine ships them as subpackages).
- The main process runs as root (nginx/fpm master model); workers drop to
  the `buran` user created via sysusers.
- `/etc/buran/buran.yaml` is a conffile: package upgrades never overwrite
  local changes.

## CI notes

- The release workflow rewrites versions (APKBUILD `pkgver`, debian
  changelog, spec `Version`) from the git tag and regenerates APKBUILD
  checksums before building.
- CI builds fetch crates from crates.io. Official distro builds forbid
  network access — vendor the crates (`cargo vendor`; the release also
  publishes a vendor tarball) when submitting.
