# syntax=docker/dockerfile:1

# Buran Application Server — minimal image, Debian flavor.
# Static files and routing only, no language modules.
#
# The rust/core stages are kept textually identical to php-debian.Dockerfile
# so BuildKit shares one core build per distro release across all images.

ARG BASE=trixie

# Allocator injected via LD_PRELOAD (see final stage). Empty by default — glibc's
# per-thread-arena malloc has no musl-style global-lock contention, so nothing
# is overridden. Opt in with e.g.:
#   --build-arg ALLOCATOR_PKG=libjemalloc2 \
#   --build-arg ALLOCATOR_LIB=/usr/lib/x86_64-linux-gnu/libjemalloc.so.2
ARG ALLOCATOR_PKG=""
ARG ALLOCATOR_LIB=""

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

FROM rust AS core
ARG BASE

RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-core-debian-${BASE},sharing=locked \
    cargo build --release -p buran \
    && install -m 755 -s target/release/buran /buran

# --- Final image ---------------------------------------------------------------
FROM debian:${BASE}-slim
ARG ALLOCATOR_PKG
ARG ALLOCATOR_LIB

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, minimal (static files and routing only)"

# Pull Debian security patches: the base image freezes its APT packages at
# publish time, so they drift behind the trixie security repo. Upgrade in place
# so image scanners see the patched versions.
# ALLOCATOR_PKG (empty by default) installs an optional allocator in the same
# pass, before the apt lists are dropped.
RUN apt-get update \
    && apt-get upgrade -y \
    && { [ -z "${ALLOCATOR_PKG}" ] || apt-get install --no-install-recommends -y ${ALLOCATOR_PKG}; } \
    && rm -rf /var/lib/apt/lists/*

# Default allocator for buran and every child process. Empty is a no-op (glibc
# default); override at runtime with -e LD_PRELOAD=... regardless.
ENV LD_PRELOAD=${ALLOCATOR_LIB}

COPY --from=core /buran /usr/sbin/buran

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /usr/lib/buran/modules /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
