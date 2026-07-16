# syntax=docker/dockerfile:1

# Buran Application Server — minimal image, Alpine flavor.
# Static files and routing only, no language modules.
#
# The rust/core stages are kept textually identical to php-alpine.Dockerfile
# so BuildKit shares one core build per distro release across all images.

ARG BASE=3.24

# Allocator injected via LD_PRELOAD (see final stage). Defaults to jemalloc,
# whose arena-per-thread design avoids musl's global-lock malloc contention.
# Override to ship another allocator (both may be space-separated package
# lists), or set both empty to keep the musl allocator:
#   --build-arg ALLOCATOR_PKG=mimalloc \
#   --build-arg ALLOCATOR_LIB=/usr/lib/libmimalloc.so.2
ARG ALLOCATOR_PKG=jemalloc
ARG ALLOCATOR_LIB=/usr/lib/libjemalloc.so.2

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

# Dynamically linked against the base image's musl (crt-static off) so an
# operator can swap the malloc implementation at runtime via LD_PRELOAD
# (jemalloc ships in the final image; see below). Runtime deps are the base
# musl plus libgcc_s — Rust std pulls the latter on musl even with
# panic=abort; both are installed in the final image.
FROM rust AS core
ARG BASE

RUN --mount=type=cache,target=/opt/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-core-alpine-${BASE},sharing=locked \
    RUSTFLAGS="-C target-feature=-crt-static" \
    cargo build --release -p buran \
    && install -m 755 -s target/release/buran /buran

# --- Final image ---------------------------------------------------------------
FROM alpine:${BASE}
ARG ALLOCATOR_PKG
ARG ALLOCATOR_LIB

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, minimal (static files and routing only)"

# apk upgrade pulls any Alpine security patches the pinned base tag has drifted
# behind so image scanners see the fixed package versions.
# The core binary is dynamically linked, so its allocator is injected via
# LD_PRELOAD (set below) rather than compiled in. The default (jemalloc)
# replaces musl's global-lock malloc, which serialises under the multi-thread
# tokio router. ALLOCATOR_PKG/ALLOCATOR_LIB (see top) pick a different one.
RUN apk upgrade --no-cache \
    && apk add --no-cache libgcc ${ALLOCATOR_PKG}

# Default allocator for buran and every child process. Override at runtime with
# -e LD_PRELOAD=... (or -e LD_PRELOAD= to fall back to musl). Empty when the
# allocator args are cleared at build time.
ENV LD_PRELOAD=${ALLOCATOR_LIB}

COPY --from=core /buran /usr/sbin/buran

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /usr/lib/buran/modules /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
