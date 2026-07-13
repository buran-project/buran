# Shared capture/normalize logic for regenerate.sh and run.sh. POSIX sh + jq.

PORT_DYN=18081
PORT_FRONT=18082

wait_ready() {
    i=0
    until curl -sf -m 2 -o /dev/null "http://127.0.0.1:$PORT_DYN/index.php"; do
        i=$((i + 1))
        [ "$i" -gt 30 ] && { echo "!! stack did not come up"; return 1; }
        sleep 1
    done
}

# Keys that legitimately differ between stacks/runs and carry no framework
# semantics. Everything NOT listed here is contract, compared byte to byte.
# argc/argv presence follows register_argc_argv of the shipped php.ini
# (docker-official has no ini, distros ship one, 8.5 changed the default) —
# a property of the PHP build, not of the server under test.
NORMALIZE_JQ='del(
    .REQUEST_TIME, .REQUEST_TIME_FLOAT,
    .REMOTE_ADDR, .REMOTE_PORT, .SERVER_ADDR, .SERVER_NAME,
    .SERVER_SOFTWARE, .FCGI_ROLE,
    .HTTP_USER_AGENT,
    .PATH, .HOSTNAME, .HOME, .USER, .PWD, .SHLVL,
    .argc, .argv
)'

# capture <mode> <method> <uri> — normalized observation JSON on stdout.
capture() {
    mode="$1"; method="$2"; uri="$3"
    port=$PORT_DYN
    [ "$mode" = "front" ] && port=$PORT_FRONT

    hdr="$(mktemp)"; body="$(mktemp)"
    if [ "$method" = "HEAD" ]; then
        curl -sS -g --path-as-is -m 10 -I -o "$hdr" "http://127.0.0.1:$port$uri"
    else
        curl -sS -g --path-as-is -m 10 -X "$method" -D "$hdr" -o "$body" \
            "http://127.0.0.1:$port$uri"
    fi

    status="$(head -1 "$hdr" | awk '{print $2}')"
    ctype="$(grep -i '^content-type:' "$hdr" | head -1 | cut -d' ' -f2- | tr -d '\r')"
    location="$(grep -i '^location:' "$hdr" | head -1 | cut -d' ' -f2- | tr -d '\r' \
        | sed 's|http://[^/]*|<origin>|')"

    server=null
    case "$ctype" in application/json*)
        [ "$method" != "HEAD" ] && server="$(jq "$NORMALIZE_JQ" "$body")"
    ;; esac

    jq -n \
        --arg status "$status" \
        --arg ctype "$ctype" \
        --arg location "$location" \
        --argjson server "$server" \
        '{
            status: ($status | tonumber),
            content_type: (if $ctype == "" then null else $ctype end),
            location: (if $location == "" then null else $location end),
            server: $server
        }'
    rm -f "$hdr" "$body"
}

# each_case <callback> — callback <name> <mode> <method> <uri> per case.
each_case() {
    while IFS='|' read -r name mode method uri; do
        case "$name" in ''|'#'*) continue ;; esac
        "$1" "$name" "$mode" "$method" "$uri"
    done < cases.txt
}
