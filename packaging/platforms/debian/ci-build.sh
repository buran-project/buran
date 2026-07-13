#!/bin/sh
# Builds the deb packages in a clean Debian container (run as root):
#   docker run --rm -v "$PWD:/src:ro" -v "$PWD/dist:/out" -e VERSION=0.1.0 \
#     debian:trixie sh /src/packaging/platforms/debian/ci-build.sh
#
# ci-image: debian:trixie   builder image, read by release.yml to build the matrix
set -eux

export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y --no-install-recommends \
    build-essential debhelper ca-certificates curl \
    cargo rustc php-dev libphp8.4-embed

# Debian trixie ships Rust 1.85, but the codebase uses let-chains (stable since
# 1.88), so the distro rustc cannot build it. Install the current stable
# toolchain — the same one the container images use — and put it ahead of the
# distro rustc on PATH. The apt cargo/rustc above stay only to satisfy the
# packaging Build-Depends check (dpkg-checkbuilddeps).
export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain stable
export PATH=/opt/cargo/bin:$PATH

mkdir -p /build/buran /out
tar -C /src --exclude .git --exclude target -cf - . | tar -C /build/buran -xf -

cd /build/buran
cp -a packaging/platforms/debian debian
sed -i "1s/([^)]*)/($VERSION)/" debian/changelog
dpkg-buildpackage -us -uc -b

cp /build/*.deb /out/
