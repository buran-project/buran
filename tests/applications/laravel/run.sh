#!/usr/bin/env sh
# Laravel-on-Buran e2e: welcome page, framework routing/404, static assets,
# session cookie and a large streamed response — the parts that depend on the
# $_SERVER/SAPI contract.
#
# Usage:
#   ./run.sh          full cycle: up, checks, teardown
#   KEEP=1 ./run.sh   leave the stack running for debugging
#
# Base image must exist locally first: docker buildx bake php-8-4-debian
set -eu
cd "$(dirname "$0")"
. ../lib.sh

URL="http://127.0.0.1:8185"
BASE_IMAGE="${BURAN_IMAGE:-buran:php-8.4}"
BAKE_TARGET="php-8-4-debian"
WAIT_PATH="/"

buran_up

echo ">> checks"
fails=0
check "welcome page"     "$URL/"            200 "Laravel"
check "framework 404"    "$URL/nope"        404 "Not Found"
check "static asset"     "$URL/favicon.ico" 200 ""

# Session bootstrap: the framework must set its cookies over our SAPI.
if curl -s -D - -o /dev/null "$URL/" | grep -qi "set-cookie: .*session"; then
    echo "OK    session cookie"
else
    echo "FAIL  session cookie"; fails=1
fi

# Streamed ~200 KiB response: the whole body must survive Buran's chunked
# path — assert both the end marker and the byte count.
big="$(curl -s "$URL/big")"
size="$(printf '%s' "$big" | wc -c)"
if echo "$big" | grep -q "END-MARKER" && [ "$size" -ge 204800 ]; then
    echo "OK    streamed big response ($size bytes)"
else
    echo "FAIL  streamed big response: $size bytes, marker $(echo "$big" | grep -c END-MARKER)"; fails=1
fi

buran_report
