#!/usr/bin/env sh
# Symfony-on-Buran e2e: attribute-routed controller, dev welcome page, the
# router 404 and native PHP-runtime probes (version pin, request-body
# round-trip) — Symfony's Request is the pickiest $_SERVER consumer.
#
# Usage:
#   ./run.sh          full cycle: up, checks, teardown
#   KEEP=1 ./run.sh   leave the stack running for debugging
#
# Base image must exist locally first: docker buildx bake php-8-5-debian
set -eu
cd "$(dirname "$0")"
. ../lib.sh

URL="http://127.0.0.1:8187"
BASE_IMAGE="${BURAN_IMAGE:-buran:php-8.5}"
BAKE_TARGET="php-8-5-debian"
WAIT_PATH="/hello"

buran_up

echo ">> checks"
fails=0
check "routed controller"  "$URL/hello" 200 '"hello":"buran"'
# dev welcome page: 404 status by design when / has no route
check "dev welcome page"   "$URL/"      404 "Symfony"
check "router 404"         "$URL/nope"  404 "Not Found"

# PHP runtime, natively: version pin proves the php85 module actually runs.
check "runtime version"    "$URL/probe" 200 '"php":"8.5'

# Request body over the SAPI: parsed POST field + raw php://input hash.
payload="a=buran&raw=hellobody"
want_md5="$(printf '%s' "$payload" | md5sum | cut -d' ' -f1)"
resp="$(curl -s --data "$payload" "$URL/echo")"
if echo "$resp" | grep -q '"a":"buran"' && echo "$resp" | grep -q "\"md5\":\"$want_md5\""; then
    echo "OK    request body round-trip"
else
    echo "FAIL  request body round-trip: $resp"; fails=1
fi

buran_report
