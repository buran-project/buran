#!/usr/bin/env sh
# Yii3-on-Buran e2e: default page, framework 404 and a native runtime probe
# through the front controller.
#
# Usage:
#   ./run.sh          full cycle: up, checks, teardown
#   KEEP=1 ./run.sh   leave the stack running for debugging
#
# Base image must exist locally first: docker buildx bake php-8-3-debian
set -eu
cd "$(dirname "$0")"
. ../lib.sh

URL="http://127.0.0.1:8188"
BASE_IMAGE="${BURAN_IMAGE:-buran:php-8.3}"
BAKE_TARGET="php-8-3-debian"
WAIT_PATH="/"

buran_up

echo ">> checks"
fails=0
check "front page"      "$URL/"      200 "yii"
check "framework 404"   "$URL/nope"  404 "not found"
check "runtime version" "$URL/probe" 200 "php=8.3"

buran_report
