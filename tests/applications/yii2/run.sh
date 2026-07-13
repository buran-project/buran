#!/usr/bin/env sh
# Yii2-on-Buran e2e on PHP 7.4: default page, query routing (?r=), static
# assets, framework 404, plus native PHP-runtime probes — the version pin and
# the fastcgi_finish_request flush-then-log pattern the legacy lane relies on.
#
# Usage:
#   ./run.sh          full cycle: up, checks, teardown
#   KEEP=1 ./run.sh   leave the stack running for debugging
#
# Base image must exist locally first: docker buildx bake php-7-4-debian
set -eu
cd "$(dirname "$0")"
. ../lib.sh

URL="http://127.0.0.1:8186"
BASE_IMAGE="${BURAN_IMAGE:-buran:php-7.4-debian}"
BAKE_TARGET="php-7-4-debian"
WAIT_PATH="/"

buran_up

echo ">> checks"
fails=0
check "front page"     "$URL/"                          200 "Congratulations"
check "query routing"  "$URL/index.php?r=site%2Fabout"  200 "About"
check "static asset"   "$URL/css/site.css"              200 "container"
check "framework 404"  "$URL/index.php?r=nope"          404 "Not Found"

# Runtime version, natively: proves the php74 module really runs 7.4.
check "runtime version" "$URL/index.php?r=probe%2Finfo"  200 "php=7.4"

# fastcgi_finish_request: the reply must return well before the 2s of
# post-response "logging", and the marker that work writes must then appear.
echo ">> deferred work (fastcgi_finish_request)"
t="$(curl -s -o /dev/null -w '%{time_total}' "$URL/index.php?r=probe%2Fdefer")"
if awk "BEGIN { exit !($t < 1.0) }"; then
    echo "OK    reply flushed before background work (${t}s)"
else
    echo "FAIL  reply blocked on background work (${t}s)"; fails=1
fi

i=0
until curl -s "$URL/index.php?r=probe%2Fstatus" | grep -q done; do
    i=$((i + 1))
    [ "$i" -gt 10 ] && { echo "FAIL  deferred work never completed"; fails=1; break; }
    sleep 1
done
[ "$i" -le 10 ] && echo "OK    background work completed after response"

buran_report
