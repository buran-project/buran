#!/usr/bin/env sh
# Regenerates the golden files from the reference nginx+php-fpm stack.
# Run when adding cases or consciously changing the contract; commit the
# resulting golden/*.json. The reference PHP version is pinned in
# nginx-fpm/docker-compose.yml — goldens capture OUR variable population,
# which must not depend on the PHP branch (per-version overrides can be
# added if a branch ever genuinely differs).
set -eu
cd "$(dirname "$0")"
. ../lib.sh

compose() { docker compose -f nginx-fpm/docker-compose.yml "$@"; }

cleanup() { compose down -v --remove-orphans >/dev/null 2>&1 || true; }
trap cleanup EXIT

echo ">> starting reference nginx+fpm stack"
compose up -d
wait_ready

mkdir -p golden
regen_one() {
    capture "$2" "$3" "$4" > "golden/$1.json"
    echo "captured $1"
}
each_case regen_one

echo ">> done: $(ls golden | wc -l) golden files"
