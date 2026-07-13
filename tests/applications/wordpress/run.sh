#!/usr/bin/env sh
# WordPress-on-Buran e2e test (spec 2.13, MVP exit criterion): brings the
# stack up, installs WP via wp-cli, drives the front controller, REST API,
# static assets, the admin login flow and an APCu/opcache probe (mu-plugin)
# through Buran.
#
# Usage:
#   ./run.sh          full cycle: up, install, checks, teardown
#   KEEP=1 ./run.sh   leave the stack running for debugging
#
# The buran service extends the stock image (see Dockerfile); the base must
# exist locally first: docker buildx bake php-8-4-debian
set -eu
cd "$(dirname "$0")"
. ../lib.sh

URL="http://127.0.0.1:8184"
BASE_IMAGE="${BURAN_IMAGE:-buran:php-8.4}"
BAKE_TARGET="php-8-4-debian"
WAIT_PATH="/wp-login.php"
CURL_OPTS="-L"  # WP redirects front-controller URLs; follow them in check()

buran_up

# wp-cli lives inside the buran container (see Dockerfile); run it as
# www-data so file ownership in the shared volume stays consistent.
wp() {
    docker compose exec -T -u 33:33 -e HOME=/tmp buran wp --path=/www "$@"
}

if ! wp core is-installed 2>/dev/null; then
    echo ">> installing WordPress via wp-cli"
    wp core install \
        --url="$URL" \
        --title="Buran WP" \
        --admin_user=admin \
        --admin_password=buran-demo \
        --admin_email=admin@example.com \
        --skip-email
fi

# Pretty permalinks: the real front-controller test (/wp-json/, /hello-world/).
wp option get permalink_structure | grep -q postname \
    || wp rewrite structure '/%postname%/'

echo ">> checks"
fails=0
check "front page"       "$URL/"                          200 "Buran WP"
check "login page"       "$URL/wp-login.php"              200 "loginform"
check "REST API"         "$URL/wp-json/"                  200 "\"name\":\"Buran WP\""
check "admin css asset"  "$URL/wp-includes/css/buttons.css" 200 "button"
check "post via query"   "$URL/?p=1"                      200 "Hello world"
check "pretty permalink" "$URL/hello-world/"              200 "Hello world"

# APCu + opcache through the mu-plugin: what a real WP deploy actually uses.
# Two hits must report apcu/opcache on and a strictly increasing counter,
# proving the shared segment survives across requests and workers.
echo ">> apcu + opcache (mu-plugin probe)"
p1="$(curl -s "$URL/?buran_probe=1")"
p2="$(curl -s "$URL/?buran_probe=1")"
h1="$(echo "$p1" | sed -n 's/.*hits=\([0-9]*\).*/\1/p')"
h2="$(echo "$p2" | sed -n 's/.*hits=\([0-9]*\).*/\1/p')"
if echo "$p2" | grep -q "apcu=on" && echo "$p2" | grep -q "opcache=on" \
    && [ -n "$h1" ] && [ -n "$h2" ] && [ "$h2" -gt "$h1" ]; then
    echo "OK    apcu shared counter ($h1 -> $h2) + opcache on"
else
    echo "FAIL  apcu/opcache probe: [$p1] [$p2]"; fails=1
fi

# Login flow: cookies + POST through the whole stack.
echo ">> admin login (cookies + POST)"
jar="$(mktemp)"
curl -s -c "$jar" -o /dev/null "$URL/wp-login.php"
code="$(curl -s -b "$jar" -c "$jar" -o /dev/null -w '%{http_code}' \
    -d 'log=admin&pwd=buran-demo&wp-submit=Log+In&redirect_to='"$URL"'/wp-admin/&testcookie=1' \
    "$URL/wp-login.php")"
if [ "$code" = "302" ]; then
    dash="$(curl -s -b "$jar" -o /dev/null -w '%{http_code}' "$URL/wp-admin/index.php")"
    if [ "$dash" = "200" ]; then
        echo "OK    admin login + dashboard"
    else
        echo "FAIL  dashboard after login: HTTP $dash"; fails=1
    fi
else
    echo "FAIL  login POST: HTTP $code (expected 302)"; fails=1
fi
rm -f "$jar"

buran_report
