#!/command/with-contenv sh

check() {
    case "$2" in
        ''|*[!0-9]*|0)
            printf '%s health probe port is invalid\n' "$1" >&2
            return 1
            ;;
    esac
    status=$(curl -q --noproxy '*' --silent --show-error --max-time 2 \
        --output /dev/null --write-out '%{http_code}' \
        "http://127.0.0.1:$2$3") || {
        printf '%s health probe failed\n' "$1" >&2
        return 1
    }
    if [ "$status" != 200 ]; then
        printf '%s health probe returned HTTP %s\n' "$1" "$status" >&2
        return 1
    fi
}

check HBBS "$HBBS_HEALTH_PORT" /readyz &
hbbs=$!
check HBBR "$HBBR_HEALTH_PORT" /readyz &
hbbr=$!
console=
if [ "${CONSOLE_ENABLED:-1}" != 0 ]; then
    console_port=${CONSOLE_HEALTH_PORT:-${RUSTDESK_API_GIN_API_ADDR##*:}}
    check Console "${console_port:-21114}" /health &
    console=$!
fi

failed=0
wait "$hbbs" || failed=1
wait "$hbbr" || failed=1
if [ -n "$console" ]; then
    wait "$console" || failed=1
fi
exit "$failed"
