# syntax=docker/dockerfile:1

# Buran Application Server — minimal image, Debian flavor.
# Static files and routing only, no language modules.
#
# The rust/core stages are kept textually identical to php-debian.Dockerfile
# so BuildKit shares one core build per distro release across all images.

ARG BASE=trixie

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

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, minimal (static files and routing only)"

# Pull Debian security patches: the base image freezes its APT packages at
# publish time, so they drift behind the trixie security repo. Upgrade in place
# so image scanners see the patched versions.
RUN apt-get update \
    && apt-get upgrade -y \
    && rm -rf /var/lib/apt/lists/*

COPY --from=core /buran /usr/sbin/buran

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /usr/lib/buran/modules /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
