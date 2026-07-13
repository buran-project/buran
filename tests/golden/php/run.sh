#!/usr/bin/env sh
# Runs the golden contract against a built Buran image:
#
#   ./run.sh buran:php-8.4-debian php84
#
# Starts the image with buran.yaml (semantically equal to the nginx+fpm
# reference), replays cases.txt and diffs normalized observations against
# golden/*.json. Any diff = the $_SERVER/response contract moved.
set -eu
cd "$(dirname "$0")"
. ../lib.sh

IMAGE="$1"
MODULE="$2"
NAME="buran-golden-$$"

cleanup() {
    status=$?
    [ "$status" = "0" ] || docker logs "$NAME" 2>&1 | tail -10
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    exit $status
}
trap cleanup EXIT

docker run -d --name "$NAME" \
    -p "$PORT_DYN:8080" -p "$PORT_FRONT:8081" \
    -v "$PWD/www:/www:ro" -v "$PWD:/golden:ro" \
    -e BURAN_GOLDEN_MODULE="$MODULE" \
    "$IMAGE" buran --config /golden/buran.yaml >/dev/null
wait_ready

fails=0
check_one() {
    actual="$(mktemp)"
    capture "$2" "$3" "$4" > "$actual"
    if diff -u "golden/$1.json" "$actual" > /tmp/golden-diff.$$ 2>&1; then
        echo "OK    $1"
    else
        echo "FAIL  $1 ($3 $4)"
        sed 's/^/      /' /tmp/golden-diff.$$
        fails=$((fails + 1))
    fi
    rm -f "$actual" /tmp/golden-diff.$$
}
each_case check_one

if [ "$fails" = "0" ]; then
    echo ">> GOLDEN CONTRACT HOLDS ($IMAGE, $MODULE)"
else
    echo ">> $fails CASE(S) DIVERGED"
    exit 1
fi
