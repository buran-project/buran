# Buran Application Server — image build matrix.
#
# Single source of truth for every published image: PHP version × libc flavor
# plus the runtime-less `minimal` pair. CI and local builds run the same file.
#
# Usage:
#   docker buildx bake                        # everything, local platform only
#   docker buildx bake php-debian            # all Debian PHP images
#   docker buildx bake php-8-5-alpine        # a single cell
#   REPO=ghcr.io/buran-project/buran VERSION=0.1.0 \
#     docker buildx bake --push --set '*.platform=linux/amd64,linux/arm64'
#
# Base image policy — each PHP branch is pinned to the newest base that
# natively carries it; bumping a base is a one-line diff here:
#   - Debian flavor: official docker-library php images (php:X.Y-cli-<distro>).
#     Their libphp ships --enable-embed on every branch since 7.3.
#     (Alpine cli images do NOT — embed is ZTS-only there, hence:)
#   - Alpine flavor: plain alpine:3.x + PHP from the distro repos
#     (phpXX, phpXX-embed, phpXX-dev). Every branch 7.3..8.5 is covered by
#     some aports release, security updates come from Alpine maintainers.
#     Layout is apk-native (/etc/phpXX, apk-packaged extensions), which
#     intentionally differs from the docker-official layout of the Debian
#     flavor.

# Comma-separated list of repositories to tag for; every tag is fanned out
# to each of them: REPO=ghcr.io/buran-project/buran,docker.io/xxx/buran.
variable "REPO" {
  default = "buran"
}

# Suffix appended to every tag. CI builds each platform natively on its own
# runner as "<tag>-amd64" / "<tag>-arm64", then stitches the multi-arch
# manifests under the clean tag with `docker buildx imagetools create`.
variable "TAG_SUFFIX" {
  default = ""
}

# Buran release version, e.g. "0.1.0". Empty = no version-prefixed tags
# (local/dev builds).
variable "VERSION" {
  default = ""
}

# The PHP branch that gets the bare `php` / `php-alpine` alias tags.
variable "PHP_LATEST" {
  default = "8.5"
}

# Version prefixes for every tag: none (floating), exact, and — for clean
# X.Y.Z releases only (not rc/dev) — the "X.Y-" / "X-" semver anchors.
# VERSION=0.1.0 => ["", "0.1.0-", "0.1-", "0-"].
function "prefixes" {
  params = [v]
  result = concat(
    [""],
    v == "" ? [] : ["${v}-"],
    can(regex("^\\d+\\.\\d+\\.\\d+$", v)) ? [
      "${join(".", slice(split(".", v), 0, 2))}-",
      "${split(".", v)[0]}-",
    ] : [],
  )
}

function "tags" {
  params = [names]
  result = flatten([
    for r in split(",", REPO) : [
      for t in names : [for p in prefixes(VERSION) : "${r}:${p}${t}${TAG_SUFFIX}"]
    ]
  ])
}

group "default" {
  targets = ["php", "minimal"]
}

group "php" {
  targets = ["php-debian", "php-alpine"]
}

group "minimal" {
  targets = ["minimal-debian", "minimal-alpine"]
}

# Cells on supported PHP branches and live bases — the security-scan matrix
# (.github/workflows/scan.yml). Legacy cells (7.3-8.1, EOL bases, built once
# and never rebuilt) are excluded by design: their findings are permanent.
# Update when a PHP branch reaches EOL or a new one lands.
group "supported" {
  targets = [
    "php-8-2-debian", "php-8-3-debian", "php-8-4-debian", "php-8-5-debian",
    "php-8-2-alpine", "php-8-3-alpine", "php-8-4-alpine", "php-8-5-alpine",
    "minimal-debian", "minimal-alpine",
  ]
}

target "_common" {
  context   = "."
  platforms = ["linux/amd64", "linux/arm64"]
  labels = {
    "org.opencontainers.image.title"   = "Buran Application Server"
    "org.opencontainers.image.source"  = "https://github.com/buran-project/buran"
    "org.opencontainers.image.version" = VERSION
  }
}

# --- PHP × Debian -----------------------------------------------------------
# Tags: php-8.5-debian (canonical), php-8.5 (debian is the default flavor),
# php (latest branch only) — each also with the "${VERSION}-" prefix.

target "php-debian" {
  inherits = ["_common"]
  name     = "php-${replace(item.php, ".", "-")}-debian"
  matrix = {
    item = [
      # alloc/alloc_lib: same allocator knob as the Alpine flavor, empty here —
      # glibc's per-thread-arena malloc has no musl-style global-lock problem,
      # so the default ships no override. To opt one cell in, set e.g.
      # alloc = "libjemalloc2", alloc_lib = "/usr/lib/x86_64-linux-gnu/libjemalloc.so.2".
      { php = "7.3", base = "bullseye", opc = "opcache", alloc = "", alloc_lib = "" }, # legacy, EOL base: built once, never rebuilt
      { php = "7.4", base = "bullseye", opc = "opcache", alloc = "", alloc_lib = "" }, # legacy
      { php = "8.0", base = "bullseye", opc = "opcache", alloc = "", alloc_lib = "" }, # legacy
      { php = "8.1", base = "trixie", opc = "opcache", alloc = "", alloc_lib = "" },   # legacy
      { php = "8.2", base = "trixie", opc = "opcache", alloc = "", alloc_lib = "" },
      { php = "8.3", base = "trixie", opc = "opcache", alloc = "", alloc_lib = "" },
      { php = "8.4", base = "trixie", opc = "opcache", alloc = "", alloc_lib = "" },
      { php = "8.5", base = "trixie", opc = "", alloc = "", alloc_lib = "" },
    ]
  }
  dockerfile = "docker/php-debian.Dockerfile"
  args = {
    PHP_VERSION = item.php
    BASE        = item.base # FROM php:${PHP_VERSION}-cli-${BASE}
    OPCACHE_EXT = item.opc
    # Allocator baked in and preloaded, straight from the matrix cell.
    ALLOCATOR_PKG = item.alloc
    ALLOCATOR_LIB = item.alloc_lib
  }
  # Module identity for CI (read from `bake --print`) and image self-description
  # — keeps the test workflow language-agnostic, no PHP knowledge baked in there.
  labels = {
    "dev.buran.lang"   = "php"
    "dev.buran.module" = "php${replace(item.php, ".", "")}"
  }
  tags = tags(concat(
    ["php-${item.php}-debian", "php-${item.php}"],
    item.php == PHP_LATEST ? ["php"] : [],
  ))
}

# --- PHP × Alpine ------------------------------------------------------------
# Tags: php-8.5-alpine (canonical), php-alpine (latest branch only) — each
# also with the "${VERSION}-" prefix.

target "php-alpine" {
  inherits = ["_common"]
  name     = "php-${replace(item.php, ".", "-")}-alpine"
  matrix = {
    item = [
      # extra: json was a separate apk package until it moved into core in
      # 8.0 (the debian flavor has it compiled in — parity demands it here).
      # alloc/alloc_lib: allocator package baked in per cell and its soname to
      # preload via LD_PRELOAD. jemalloc where Alpine carries it (3.17+); empty
      # on legacy EOL bases that ship no jemalloc, leaving the musl allocator.
      { php = "7.3", base = "3.12", pkg = "php7", opc = "php7-opcache", extra = "php7-json", alloc = "", alloc_lib = "" }, # legacy, EOL base: built once, never rebuilt
      { php = "7.4", base = "3.15", pkg = "php7", opc = "php7-opcache", extra = "php7-json", alloc = "", alloc_lib = "" }, # legacy
      { php = "8.0", base = "3.16", pkg = "php8", opc = "php8-opcache", extra = "", alloc = "", alloc_lib = "" },          # legacy
      { php = "8.1", base = "3.19", pkg = "php81", opc = "php81-opcache", extra = "", alloc = "jemalloc", alloc_lib = "/usr/lib/libjemalloc.so.2" }, # legacy
      { php = "8.2", base = "3.22", pkg = "php82", opc = "php82-opcache", extra = "", alloc = "jemalloc", alloc_lib = "/usr/lib/libjemalloc.so.2" },
      { php = "8.3", base = "3.24", pkg = "php83", opc = "php83-opcache", extra = "", alloc = "jemalloc", alloc_lib = "/usr/lib/libjemalloc.so.2" },
      { php = "8.4", base = "3.24", pkg = "php84", opc = "php84-opcache", extra = "", alloc = "jemalloc", alloc_lib = "/usr/lib/libjemalloc.so.2" },
      { php = "8.5", base = "3.24", pkg = "php85", opc = "", extra = "", alloc = "jemalloc", alloc_lib = "/usr/lib/libjemalloc.so.2" },
    ]
  }
  dockerfile = "docker/php-alpine.Dockerfile"
  args = {
    PHP_VERSION  = item.php
    BASE         = item.base
    PHP_PKG      = item.pkg
    OPCACHE_PKG  = item.opc
    EXTRA_PKGS   = item.extra
    # Allocator baked in and preloaded, straight from the matrix cell.
    ALLOCATOR_PKG = item.alloc
    ALLOCATOR_LIB = item.alloc_lib
  }
  # See php-debian: module identity for CI + image self-description.
  labels = {
    "dev.buran.lang"   = "php"
    "dev.buran.module" = "php${replace(item.php, ".", "")}"
  }
  tags = tags(concat(
    ["php-${item.php}-alpine"],
    item.php == PHP_LATEST ? ["php-alpine"] : [],
  ))
}

# --- Minimal (static files / routing only, no language modules) --------------

target "minimal-debian" {
  inherits   = ["_common"]
  dockerfile = "docker/minimal-debian.Dockerfile"
  args       = { BASE = "trixie" }
  tags       = tags(["minimal-debian", "minimal"])
}

target "minimal-alpine" {
  inherits   = ["_common"]
  dockerfile = "docker/minimal-alpine.Dockerfile"
  args       = { BASE = "3.24" }
  tags       = tags(["minimal-alpine"])
}
