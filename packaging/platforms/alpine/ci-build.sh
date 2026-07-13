#!/bin/sh
# Builds the apk packages in a clean Alpine container (run as root):
#   docker run --rm -v "$PWD:/src:ro" -v "$PWD/dist:/out" -e VERSION=0.1.0 \
#     alpine:3.24 sh /src/packaging/platforms/alpine/ci-build.sh
#
# ci-image: alpine:3.24   builder image, read by release.yml to build the matrix
set -eux

# populate the index cache: abuild-apk resolves makedepends through it
apk update
apk add alpine-sdk tar

adduser -D builder
addgroup builder abuild

mkdir -p /build /out
tar -C /src --exclude .git --exclude target -czf "/build/buran-$VERSION.tar.gz" \
    --transform "s,^\.,buran-$VERSION," .
cp /src/packaging/platforms/alpine/APKBUILD /src/packaging/platforms/alpine/buran.initd /build/
cp /src/packaging/common/buran/buran.yaml /build/
sed -i "s/^pkgver=.*/pkgver=$VERSION/" /build/APKBUILD
chown -R builder /build

# abuild-sudo (setuid, group abuild) lets the builder install makedepends.
# The throwaway public key must land in /etc/apk/keys (root) or the final
# package index fails signature verification.
su builder -c 'abuild-keygen -an'
cp /home/builder/.abuild/*.rsa.pub /etc/apk/keys/
su builder -c 'cd /build && abuild checksum && abuild -r'

find /home/builder/packages -name '*.apk' -exec cp {} /out/ \;
ls /out/
