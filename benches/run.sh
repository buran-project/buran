#!/usr/bin/env sh
# Buran benchmark suite: buran vs freeunit vs nginx+php-fpm.
# Linux only (host networking). Usage: ./run.sh [duration] [connections]
set -eu

cd "$(dirname "$0")"

DURATION="${1:-10s}"
CONNECTIONS="${2:-10000}"
# Cooldown between stacks: laptops throttle, and without it the last stack
# benches on the hottest silicon. Override: COOLDOWN=0 ./run.sh
COOLDOWN="${COOLDOWN:-30}"

# oha: local binary if present, container otherwise.
if command -v oha >/dev/null 2>&1; then
    OHA="oha"
else
    echo "oha not found locally, using ghcr.io/hatoo/oha via docker"
    OHA="docker run --rm --network host ghcr.io/hatoo/oha:latest"
fi

echo ">> starting stacks (first run builds the buran image, takes a while)"
docker compose up -d --build

wait_for() {
    i=0
    until curl -sf -o /dev/null "$1"; do
        i=$((i + 1))
        if [ "$i" -gt 120 ]; then
            echo "!! $2 did not come up ($1)"
            docker compose logs "$2" | tail -20
            exit 1
        fi
        sleep 0.5
    done
}

echo ">> waiting for readiness"
wait_for http://127.0.0.1:8081/index.php buran
wait_for http://127.0.0.1:8082/index.php freeunit
wait_for http://127.0.0.1:8083/index.php nginx

echo ">> sanity: identical responses"
for port in 8081 8082 8083; do
    printf "  :%s  %s\n" "$port" "$(curl -s "http://127.0.0.1:$port/index.php")"
done

bench() {
    name="$1"
    url="$2"
    # Warmup, then measure.
    $OHA -z 3s -c "$CONNECTIONS" --no-tui "$url" >/dev/null 2>&1
    result="$($OHA -z "$DURATION" -c "$CONNECTIONS" --no-tui "$url" 2>/dev/null)"
    rps="$(echo "$result" | grep 'Requests/sec' | awk '{print $2}')"
    p50="$(echo "$result" | grep '50.00% in' | awk '{print $3, $4}')"
    p99="$(echo "$result" | grep '99.00% in' | awk '{print $3, $4}')"
    ok="$(echo "$result" | grep 'Success rate' | awk '{print $3}')"
    printf "%-12s %12s rps   p50 %-12s p99 %-12s success %s\n" "$name" "$rps" "$p50" "$p99" "$ok"
}

echo ">> benchmarking: PHP hello world, ${DURATION}, ${CONNECTIONS} connections"
echo ">> cooldown between stacks: ${COOLDOWN}s"
echo "--------------------------------------------------------------------------"
sleep "$COOLDOWN"
bench "buran"     "http://127.0.0.1:8081/index.php"
sleep "$COOLDOWN"
bench "freeunit"  "http://127.0.0.1:8082/index.php"
sleep "$COOLDOWN"
bench "nginx+fpm" "http://127.0.0.1:8083/index.php"
echo "--------------------------------------------------------------------------"

echo ">> done; stacks keep running (docker compose down to stop)"
