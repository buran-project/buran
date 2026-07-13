# syntax=docker/dockerfile:1

# Buran Application Server — PHP image, Debian flavor.
#
# Based on the official php:X.Y-cli-<distro> image: its libphp.so is built
# with --enable-embed on every branch since 7.3, which is exactly what
# buran-php links against. Extensions follow the docker-official workflow
# (docker-php-ext-install / pecl).
#
# Built via docker-bake.hcl (repo root) — PHP_VERSION and BASE come from the
# matrix there. Manual build:
#   docker build -f docker/php-debian.Dockerfile \
#     --build-arg PHP_VERSION=8.5 --build-arg BASE=trixie -t buran:php-8.5-debian .

ARG PHP_VERSION=8.5
ARG BASE=trixie

# Extension name to build opcache from; empty when statically built-in (8.5+).
ARG OPCACHE_EXT=""

# --- Rust toolchain on the bare distro release --------------------------------
# PHP-free on purpose: this stage and `core` are identical across every PHP
# version on the same BASE (and shared with minimal-debian.Dockerfile), so
# BuildKit builds them once per distro release.
FROM debian:${BASE}-slim AS rust

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH

RUN set -ex \
    && apt-get update \
    && apt-get install --no-install-recommends --no-install-suggests -y \
         ca-certificates curl build-essential pkg-config \
    && rm -rf /var/lib/apt/lists/* \
    && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
       | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable \
    && rustc --version

WORKDIR /usr/src/buran
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# --- Core server binary (no PHP dependency) -----------------------------------
FROM rust AS core
ARG BASE

RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-core-debian-${BASE},sharing=locked \
    cargo build --release -p buran \
    && install -m 755 -s target/release/buran /buran

# --- PHP module, linked against libphp of this exact version ------------------
FROM php:${PHP_VERSION}-cli-${BASE} AS module
ARG PHP_VERSION
ARG BASE

ENV RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo PATH=/opt/cargo/bin:$PATH
COPY --from=rust /opt/rustup /opt/rustup
COPY --from=rust /opt/cargo /opt/cargo

WORKDIR /usr/src/buran
COPY Cargo.toml Cargo.lock ./
COPY crates/ crates/

# php-config and the compiler toolchain ($PHPIZE_DEPS) ship with the image.
# The embed library kept the major in its name until 8.0 (libphp7.so vs
# libphp.so) — derive the link name from what the image actually has.
# The installed name is versioned (buran-php85) — it is what `module:` in
# the config resolves 1:1 (spec: exact match, no fuzzy), so several PHP
# versions can live side by side in one modules dir.
RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-php-${PHP_VERSION}-${BASE},sharing=locked \
    so="$(find "$(php-config --prefix)/lib" -maxdepth 1 -name 'libphp*.so' | head -n1)" \
    && libname="$(basename "$so" .so)" && libname="${libname#lib}" \
    && export BURAN_PHP_LIB="$libname" \
    && cargo build --release -p buran-php \
    && install -D -m 755 -s target/release/buran-php \
         "/out/buran-php$(echo "${PHP_VERSION}" | tr -d .)"

# --- Final image ---------------------------------------------------------------
FROM php:${PHP_VERSION}-cli-${BASE}
ARG OPCACHE_EXT

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, PHP (Debian flavor)"

# Pull Debian security patches. The php base image freezes its APT packages at
# publish time, so shipped packages (curl, linux-libc-dev, ...) drift behind
# the trixie security repo and light up image scanners. Upgrade in place so the
# final image carries the patched versions.
RUN apt-get update \
    && apt-get upgrade -y \
    && rm -rf /var/lib/apt/lists/*

# Nothing beyond opcache on purpose: extend the image the same way as the
# official php ones — docker-php-ext-install / pecl; the ini land in the scan
# dir, which the buran SAPI picks up like any other PHP. See
# tests/applications/wordpress/Dockerfile for the pattern.
RUN [ -z "${OPCACHE_EXT}" ] || docker-php-ext-install -j"$(nproc)" ${OPCACHE_EXT}

COPY --from=core /buran /usr/sbin/buran
COPY --from=module /out/ /usr/lib/buran/modules/

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
