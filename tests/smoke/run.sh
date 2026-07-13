#!/usr/bin/env sh
# Smoke test of a built image: static file + (for PHP images) script
# execution with a version assertion derived from the module name.
#
#   ./run.sh buran:php-7.3-alpine php73
#   ./run.sh buran:minimal            # static only
#
# PORT overrides the host port (default 18090) for parallel runs.
set -eu
cd "$(dirname "$0")"

IMAGE="$1"
MODULE="${2:-}"
PORT="${PORT:-18090}"
NAME="buran-smoke-$$"

cleanup() {
    status=$?
    [ "$status" = "0" ] || docker logs "$NAME" 2>&1 | tail -10
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    exit $status
}
trap cleanup EXIT

if [ -n "$MODULE" ]; then cfg=php.yaml; else cfg=minimal.yaml; fi

docker run -d --name "$NAME" -p "$PORT:8080" \
    -v "$PWD/www:/www:ro" -v "$PWD:/smoke:ro" \
    -e BURAN_SMOKE_MODULE="$MODULE" \
    "$IMAGE" buran --config "/smoke/$cfg" >/dev/null

i=0
until curl -sf -m 2 -o /dev/null "http://127.0.0.1:$PORT/static.txt"; do
    i=$((i + 1))
    [ "$i" -gt 15 ] && { echo "FAIL $IMAGE: did not come up"; exit 1; }
    sleep 1
done

curl -s -m 5 "http://127.0.0.1:$PORT/static.txt" | grep -q buran-smoke-static \
    || { echo "FAIL $IMAGE: static"; exit 1; }

if [ -n "$MODULE" ]; then
    body="$(curl -s -m 5 "http://127.0.0.1:$PORT/")"
    # php73 -> the script must report PHP 7.3.*
    want="$(echo "$MODULE" | sed 's/^php\(.\)/\1./')"
    echo "$body" | grep -q "^buran-smoke $want\." \
        || { echo "FAIL $IMAGE: want PHP $want.*, got: $body"; exit 1; }
fi

echo "OK $IMAGE${MODULE:+ ($MODULE)}"
