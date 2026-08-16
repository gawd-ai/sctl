#!/bin/sh
set -eu

CONFIG=${SCTL_RAMBOOT_CONFIG:-/etc/sctl/ramboot.conf}

log() {
    logger -t sctl-ramboot "$*" 2>/dev/null || echo "sctl-ramboot: $*" >&2
}

die() {
    log "$*"
    exit 1
}

[ -r "$CONFIG" ] || die "missing config $CONFIG"

# shellcheck disable=SC1090
. "$CONFIG"

RUN_DIR=${RUN_DIR:-/tmp/sctl}
CACHE_DIR=${CACHE_DIR:-$RUN_DIR/cache}
BIN=${BIN:-$RUN_DIR/sctl-server}
PLUGIN=${PLUGIN:-$RUN_DIR/lib/libsctl_comms_quectel.so}
MUSL_LIBC=${MUSL_LIBC:-$RUN_DIR/lib/libc.so}
LIBGCC=${LIBGCC:-$RUN_DIR/lib/libgcc_s.so.1}
SCTL_CONFIG=${SCTL_CONFIG:-/etc/sctl/sctl.toml}
SERVER_GZIP=${SERVER_GZIP:-1}
PLUGIN_GZIP=${PLUGIN_GZIP:-1}
MUSL_LIBC_GZIP=${MUSL_LIBC_GZIP:-1}
LIBGCC_GZIP=${LIBGCC_GZIP:-1}
MIN_TMP_KB=${MIN_TMP_KB:-24576}
FETCH_TIMEOUT_SECS=${FETCH_TIMEOUT_SECS:-90}
CONNECT_TIMEOUT_SECS=${CONNECT_TIMEOUT_SECS:-10}
FETCH_ATTEMPTS=${FETCH_ATTEMPTS:-20}
FETCH_RETRY_SECS=${FETCH_RETRY_SECS:-15}
ALLOW_UNSIGNED=${ALLOW_UNSIGNED:-0}

[ -r "$SCTL_CONFIG" ] || die "missing sctl config $SCTL_CONFIG"
[ -n "${SERVER_URL:-}" ] || die "SERVER_URL is required"

check_tmp_space() {
    local avail
    avail=$(df -k /tmp | awk 'NR==2 {print $4}')
    if [ -z "$avail" ] || [ "$avail" -lt "$MIN_TMP_KB" ]; then
        die "not enough /tmp space: ${avail:-unknown} KB available, need $MIN_TMP_KB KB"
    fi
}

download_file() {
    local url=$1
    local dst=$2
    local attempt=1

    rm -f "$dst.tmp"

    while [ "$attempt" -le "$FETCH_ATTEMPTS" ]; do
        if command -v curl >/dev/null 2>&1; then
            if curl -fLk \
                --connect-timeout "$CONNECT_TIMEOUT_SECS" \
                --max-time "$FETCH_TIMEOUT_SECS" \
                -o "$dst.tmp" "$url"; then
                mv -f "$dst.tmp" "$dst"
                return 0
            fi
        elif command -v wget >/dev/null 2>&1; then
            if wget -O "$dst.tmp" "$url"; then
                mv -f "$dst.tmp" "$dst"
                return 0
            fi
        else
            die "neither curl nor wget is available"
        fi

        rm -f "$dst.tmp"
        log "download failed for $url, attempt $attempt/$FETCH_ATTEMPTS"
        attempt=$((attempt + 1))
        [ "$attempt" -le "$FETCH_ATTEMPTS" ] && sleep "$FETCH_RETRY_SECS"
    done

    die "download failed for $url after $FETCH_ATTEMPTS attempts"
}

sha256_file() {
    local file=$1

    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v openssl >/dev/null 2>&1; then
        openssl dgst -sha256 "$file" | awk '{print $NF}'
    else
        return 127
    fi
}

check_file_sha256() {
    local expected=$1
    local file=$2
    local actual

    actual=$(sha256_file "$file") || die "no SHA-256 verifier available"
    [ "$actual" = "$expected" ]
}

verify_file() {
    local expected=$1
    local file=$2
    local label=$3
    local actual

    if [ -z "$expected" ]; then
        [ "$ALLOW_UNSIGNED" = "1" ] || die "$label SHA-256 is required unless ALLOW_UNSIGNED=1"
        log "$label SHA-256 not configured; accepting because ALLOW_UNSIGNED=1"
        return 0
    fi

    actual=$(sha256_file "$file") || die "no SHA-256 verifier available for $label"
    [ "$actual" = "$expected" ] || die "$label SHA-256 mismatch: expected $expected got $actual"
}

cache_is_valid() {
    # Mirror of fetch_payload's cache-hit test, with no side effects: true
    # only when this artifact would be served from cache without a download.
    local expected=$1
    local cache=$2

    [ -r "$cache" ] || return 1
    if [ -z "$expected" ]; then
        [ "$ALLOW_UNSIGNED" = "1" ] || return 1
        return 0
    fi
    check_file_sha256 "$expected" "$cache"
}

fetch_payload() {
    local url=$1
    local expected=$2
    local cache=$3
    local label=$4

    if [ -r "$cache" ]; then
        if [ -z "$expected" ] && [ "$ALLOW_UNSIGNED" = "1" ]; then
            log "using cached unsigned $label payload"
            return 0
        fi
        if [ -n "$expected" ] && check_file_sha256 "$expected" "$cache"; then
            log "using cached $label payload"
            return 0
        fi
        log "discarding cached $label payload with invalid SHA-256"
        rm -f "$cache"
    fi

    log "fetching $label payload"
    download_file "$url" "$cache"
    verify_file "$expected" "$cache" "$label"
}

install_payload() {
    local src=$1
    local dst=$2
    local gzip=$3
    local label=$4

    mkdir -p "$(dirname "$dst")"
    rm -f "$dst.tmp"
    if [ "$gzip" = "1" ]; then
        gzip -dc "$src" > "$dst.tmp" || {
            rm -f "$dst.tmp"
            die "failed to decompress $label payload"
        }
    else
        cp "$src" "$dst.tmp"
    fi
    mv -f "$dst.tmp" "$dst"
}

SERVER_CACHE=$CACHE_DIR/sctl-server.payload
PLUGIN_CACHE=$CACHE_DIR/sctl-comms.payload
MUSL_LIBC_CACHE=$CACHE_DIR/musl-libc.payload
LIBGCC_CACHE=$CACHE_DIR/libgcc.payload

# Only an actual download needs MIN_TMP_KB of headroom. A warm restart with
# every payload already cached-and-valid writes ~no new bytes, and dying on
# unrelated /tmp pressure (captures, vendor log spam) would keep the unit
# down for the rest of the power-on. So gate the space check on whether any
# artifact really needs fetching; if even one does, the full check runs.
NEED_FETCH=0
cache_is_valid "${SERVER_SHA256:-}" "$SERVER_CACHE" || NEED_FETCH=1
[ -z "${PLUGIN_URL:-}" ] || cache_is_valid "${PLUGIN_SHA256:-}" "$PLUGIN_CACHE" || NEED_FETCH=1
[ -z "${MUSL_LIBC_URL:-}" ] || cache_is_valid "${MUSL_LIBC_SHA256:-}" "$MUSL_LIBC_CACHE" || NEED_FETCH=1
[ -z "${LIBGCC_URL:-}" ] || cache_is_valid "${LIBGCC_SHA256:-}" "$LIBGCC_CACHE" || NEED_FETCH=1

if [ "$NEED_FETCH" = "1" ]; then
    check_tmp_space
else
    log "all payloads cached and valid; skipping /tmp space check"
fi
mkdir -p "$RUN_DIR/lib" "$RUN_DIR/data" "$CACHE_DIR"

fetch_payload "$SERVER_URL" "${SERVER_SHA256:-}" "$SERVER_CACHE" "server"
install_payload "$SERVER_CACHE" "$BIN" "$SERVER_GZIP" "server"
chmod 0755 "$BIN"

if [ -n "${PLUGIN_URL:-}" ]; then
    fetch_payload "$PLUGIN_URL" "${PLUGIN_SHA256:-}" "$PLUGIN_CACHE" "comms plugin"
    install_payload "$PLUGIN_CACHE" "$PLUGIN" "$PLUGIN_GZIP" "comms plugin"
    chmod 0644 "$PLUGIN"
fi

if [ -n "${MUSL_LIBC_URL:-}" ]; then
    fetch_payload "$MUSL_LIBC_URL" "${MUSL_LIBC_SHA256:-}" "$MUSL_LIBC_CACHE" "musl libc"
    install_payload "$MUSL_LIBC_CACHE" "$MUSL_LIBC" "$MUSL_LIBC_GZIP" "musl libc"
    chmod 0755 "$MUSL_LIBC"
    LOADER=${LOADER:-$MUSL_LIBC}
fi

if [ -n "${LIBGCC_URL:-}" ]; then
    fetch_payload "$LIBGCC_URL" "${LIBGCC_SHA256:-}" "$LIBGCC_CACHE" "libgcc"
    install_payload "$LIBGCC_CACHE" "$LIBGCC" "$LIBGCC_GZIP" "libgcc"
    chmod 0644 "$LIBGCC"
fi

# Self-heal the wired-WAN preference agent, if this device has one installed.
# It is a separate, unsupervised process (there is no procd on these units), so
# nothing else would revive it if it died. Piggy-backing on the sctl start path
# means any sctl start or restart also resurrects it, at the cost of zero extra
# files. `start` is idempotent — it no-ops when the pidfile is live.
if [ -x /etc/init.d/netage-wanpref ]; then
    /etc/init.d/netage-wanpref start >/dev/null 2>&1 || true
fi

log "starting sctl from RAM"
if [ -n "${LOADER:-}" ]; then
    exec "$LOADER" --library-path "$RUN_DIR/lib" "$BIN" serve --config "$SCTL_CONFIG"
fi
exec "$BIN" serve --config "$SCTL_CONFIG"
