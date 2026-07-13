#!/bin/sh
# Builds the rpm packages in a clean Fedora container (run as root):
#   docker run --rm -v "$PWD:/src:ro" -v "$PWD/dist:/out" -e VERSION=0.1.0 \
#     fedora:42 sh /src/packaging/platforms/rpm/ci-build.sh
#
# ci-image: fedora:42   builder image, read by release.yml to build the matrix
set -eux

dnf install -y rpm-build cargo rust php-devel php-embedded \
    systemd-rpm-macros gcc gawk tar gzip

mkdir -p /build/SOURCES /out
tar -C /src --exclude .git --exclude target -czf "/build/SOURCES/buran-$VERSION.tar.gz" \
    --transform "s,^\.,buran-$VERSION," .
sed "s/^Version:.*/Version:        $VERSION/" /src/packaging/platforms/rpm/buran.spec \
    > /build/buran.spec

rpmbuild --define "_topdir /build" -bb /build/buran.spec

cp /build/RPMS/*/*.rpm /out/
