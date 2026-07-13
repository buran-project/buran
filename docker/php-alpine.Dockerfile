# syntax=docker/dockerfile:1

# Buran Application Server — PHP image, Alpine flavor.
#
# Based on plain alpine:3.x with PHP from the distro repositories: the
# official php:*-cli-alpine images ship no embed SAPI (ZTS variants only),
# while aports carries phpXX-embed for every branch we support. Layout is
# apk-native by design and differs from the Debian flavor: config lives in
# /etc/phpXX, extensions come as apk packages (e.g. `apk add php85-gd`).
#
# Built via docker-bake.hcl (repo root) — PHP_VERSION, BASE and PHP_PKG come
# from the matrix there. Manual build:
#   docker build -f docker/php-alpine.Dockerfile \
#     --build-arg PHP_VERSION=8.5 --build-arg BASE=3.24 --build-arg PHP_PKG=php85 \
#     -t buran:php-8.5-alpine .

ARG PHP_VERSION=8.5
ARG BASE=3.24
ARG PHP_PKG=php85

# Opcache subpackage name; empty when bundled into the main package (8.5+).
ARG OPCACHE_PKG=""

# Per-branch extras for parity with the Debian flavor (7.x: php7-json —
# json moved into core only in 8.0).
ARG EXTRA_PKGS=""

# --- Rust toolchain on the bare distro release --------------------------------
# PHP-free on purpose: this stage and `core` are identical across every PHP
# version on the same BASE (and shared with minimal-alpine.Dockerfile), so
# BuildKit builds them once per distro release.
FROM alpine:${BASE} AS rust

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH

RUN set -ex \
    && apk add --no-cache ca-certificates curl gcc musl-dev make pkgconf \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustc --version

WORKDIR /usr/src/buran
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# --- Core server binary (no PHP dependency) -----------------------------------
# Left statically linked (musl default): zero runtime deps.
FROM rust AS core
ARG BASE

RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-core-alpine-${BASE},sharing=locked \
    cargo build --release -p buran \
    && install -m 755 -s target/release/buran /buran

# --- PHP module, linked against libphp of this exact version ------------------
FROM rust AS module
ARG PHP_VERSION
ARG BASE
ARG PHP_PKG

RUN apk add --no-cache ${PHP_PKG}-dev ${PHP_PKG}-embed

# libphp placement varies per version (libphp7.so, libphp83.so,
# php85/libphp.so...), so link name, dir and rpath are derived from the
# actual package contents at build time — they cannot desync from apk.
# crt-static must go off to link a shared libphp.
# The installed name is versioned (buran-php85) — it is what `module:` in
# the config resolves 1:1 (spec: exact match, no fuzzy), so several PHP
# versions can live side by side in one modules dir.
RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-php-${PHP_VERSION}-alpine,sharing=locked \
    so="/$(apk info -qL ${PHP_PKG}-embed | grep '\.so$')" \
    && libdir="$(dirname "$so")" \
    && libname="$(basename "$so" .so)" && libname="${libname#lib}" \
    && export BURAN_PHP_CONFIG="php-config${PHP_PKG#php}" \
    && export BURAN_PHP_LIB="$libname" \
    && export BURAN_PHP_LIB_DIR="$libdir" \
    && export RUSTFLAGS="-C target-feature=-crt-static -C link-arg=-Wl,-rpath,$libdir" \
    && cargo build --release -p buran-php \
    && install -D -m 755 -s target/release/buran-php \
         "/out/buran-php$(echo "${PHP_VERSION}" | tr -d .)"

# --- Final image ---------------------------------------------------------------
FROM alpine:${BASE}
ARG PHP_PKG
ARG OPCACHE_PKG
ARG EXTRA_PKGS

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, PHP (Alpine flavor)"

# Nothing beyond opcache on purpose: extend the image with apk packages
# (e.g. `apk add php85-mysqli`); their ini land in /etc/phpXX/conf.d, which
# libphp scans as usual. See tests/applications/wordpress/Dockerfile for the
# pattern.
# libgcc backs the dynamically linked buran-php module.
# The plain `php` command ships only with the distro-default branch — the
# ecosystem (composer, wp-cli's `env php` shebang) expects it, so symlink
# the image's own version when the package did not provide it.
RUN apk add --no-cache libgcc ${PHP_PKG} ${PHP_PKG}-embed ${OPCACHE_PKG} ${EXTRA_PKGS} \
    && { command -v php >/dev/null || ln -s ${PHP_PKG} /usr/bin/php; }

COPY --from=core /buran /usr/sbin/buran
COPY --from=module /out/ /usr/lib/buran/modules/

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
