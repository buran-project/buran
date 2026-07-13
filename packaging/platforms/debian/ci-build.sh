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
    build-essential debhelper ca-certificates \
    cargo rustc php-dev libphp8.4-embed

mkdir -p /build/buran /out
tar -C /src --exclude .git --exclude target -cf - . | tar -C /build/buran -xf -

cd /build/buran
cp -a packaging/platforms/debian debian
sed -i "1s/([^)]*)/($VERSION)/" debian/changelog
dpkg-buildpackage -us -uc -b

cp /build/*.deb /out/
