# syntax=docker/dockerfile:1

# Buran Application Server — minimal image, Alpine flavor.
# Static files and routing only, no language modules.
#
# The rust/core stages are kept textually identical to php-alpine.Dockerfile
# so BuildKit shares one core build per distro release across all images.

ARG BASE=3.24

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

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, minimal (static files and routing only)"

# apk upgrade pulls any Alpine security patches the pinned base tag has drifted
# behind so image scanners see the fixed package versions.
# jemalloc replaces the musl allocator, whose global-lock malloc serialises
# under multi-thread contention (the core runs a multi-thread tokio router).
# The core binary is dynamically linked, so the allocator is injected via
# LD_PRELOAD (set below) rather than compiled in — an operator can point it at
# another allocator or clear it (-e LD_PRELOAD=) to fall back to musl.
RUN apk upgrade --no-cache \
    && apk add --no-cache libgcc jemalloc

# Default allocator for buran and every child process. Override at runtime.
ENV LD_PRELOAD=/usr/lib/libjemalloc.so.2

COPY --from=core /buran /usr/sbin/buran

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /usr/lib/buran/modules /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
