#!/usr/bin/env sh
# Shared scaffolding for the application e2e runs. Each app's run.sh sets a few
# vars, sources this file, calls buran_up, runs its checks, then buran_report.
#
# Contract (set before calling buran_up):
#   URL          base URL the stack answers on           (required)
#   BASE_IMAGE   base image to require                   (required)
#   BAKE_TARGET  buildx bake target suggested if missing (required)
#   WAIT_PATH    path polled while waiting for readiness  (default /)
#   CURL_OPTS    extra curl flags for check(), e.g. -L    (default empty)
#
# The check counter lives in the caller: set `fails=0` before the first check;
# check() bumps it to 1 on failure, and buran_report() reads it.

buran_require_image() {
    if ! docker image inspect "$BASE_IMAGE" >/dev/null 2>&1; then
        echo "!! base image $BASE_IMAGE not found; build it first:"
        echo "   docker buildx bake ${BAKE_TARGET}"
        exit 1
    fi
}

buran_cleanup() {
    status=$?
    if [ "${KEEP:-0}" = "1" ]; then
        echo ">> KEEP=1: stack left running at $URL"
    else
        docker compose down -v --remove-orphans >/dev/null 2>&1 || true
    fi
    exit $status
}

# Require the base image, bring the stack up and block until it answers.
buran_up() {
    buran_require_image
    trap buran_cleanup EXIT

    echo ">> starting stack"
    docker compose up -d --build

    echo ">> waiting for buran to answer"
    i=0
    until curl -s -o /dev/null "$URL${WAIT_PATH:-/}"; do
        i=$((i + 1))
        [ "$i" -gt 60 ] && { echo "!! buran did not come up"; docker compose logs buran | tail -20; exit 1; }
        sleep 1
    done
}

# check DESC URL WANT_CODE EXPECT — sets fails=1 on mismatch (case-insensitive
# body match; pass an empty EXPECT to only assert the status code).
check() {
    desc="$1"; url="$2"; want_code="$3"; expect="$4"
    body="$(curl -s ${CURL_OPTS:-} -w '\n%{http_code}' "$url")"
    code="$(echo "$body" | tail -1)"
    if [ "$code" != "$want_code" ]; then
        echo "FAIL  $desc: HTTP $code (want $want_code)"
        fails=1; return 1
    fi
    if ! echo "$body" | grep -qi "$expect"; then
        echo "FAIL  $desc: marker \"$expect\" not found"
        fails=1; return 1
    fi
    echo "OK    $desc"
}

buran_report() {
    [ "${fails:-0}" = "0" ] && echo ">> ALL CHECKS PASSED" || { echo ">> FAILURES PRESENT"; exit 1; }
}
