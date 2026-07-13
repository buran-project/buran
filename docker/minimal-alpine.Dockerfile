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

# Left statically linked (musl default): zero runtime deps.
FROM rust AS core
ARG BASE

RUN --mount=type=cache,target=/opt/cargo/registry \
    --mount=type=cache,target=/usr/src/buran/target,id=buran-core-alpine-${BASE},sharing=locked \
    cargo build --release -p buran \
    && install -m 755 -s target/release/buran /buran

# --- Final image ---------------------------------------------------------------
FROM alpine:${BASE}

LABEL org.opencontainers.image.title="Buran Application Server"
LABEL org.opencontainers.image.description="Buran universal application server, minimal (static files and routing only)"

COPY --from=core /buran /usr/sbin/buran

# Static configuration by design: mount your config at /etc/buran/buran.yaml
# (or override CMD). Config change = new container.
RUN mkdir -p /etc/buran /usr/lib/buran/modules /www

EXPOSE 8080

CMD ["buran", "--config", "/etc/buran/buran.yaml"]
