#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage:
  API_KEY=... SERVER_URL=... SERVER_SHA256=... \
  PLUGIN_URL=... PLUGIN_SHA256=... devices/we826-qwd/install.sh root@ROUTER_IP

Environment:
  API_KEY               Required. Auth token written to /etc/sctl/sctl.toml.
  SERVER_URL            Required. URL for compressed or raw sctl server payload.
  SERVER_SHA256         Required unless ALLOW_UNSIGNED=1. Hash of downloaded server payload.
  SERVER_GZIP           Default: 1
  PLUGIN_URL            Required for LTE/comms config. URL for comms plugin payload.
  PLUGIN_SHA256         Required unless ALLOW_UNSIGNED=1. Hash of downloaded plugin payload.
  PLUGIN_GZIP           Default: 1
  MUSL_LIBC_URL         Required. URL for OpenWrt musl libc/loader payload.
  MUSL_LIBC_SHA256      Required unless ALLOW_UNSIGNED=1. Hash of downloaded libc payload.
  MUSL_LIBC_GZIP        Default: 1
  LIBGCC_URL            Required. URL for libgcc_s.so.1 payload.
  LIBGCC_SHA256         Required unless ALLOW_UNSIGNED=1. Hash of downloaded libgcc payload.
  LIBGCC_GZIP           Default: 1
  SERIAL                Default: WE826-Q-WD
  LTE_INTERFACE         Default: usb0  (AT port is auto-detected, never hardcoded)
  TUNNEL_URL            Optional. Relay tunnel endpoint, e.g.
                        wss://relay-001.example/api/tunnel/register
  TUNNEL_KEY            Optional. Relay tunnel registration key. Required with TUNNEL_URL.
  TUNNEL_BIND_ADDRESS   Default: usb0. Interface the tunnel is pinned to. Use the
                        INTERFACE NAME, not an IP — sctl only applies SO_BINDTODEVICE
                        when the value fails to parse as an IP address.
                        NOTE: if TUNNEL_URL/TUNNEL_KEY are omitted and the target
                        already has a [tunnel] block, it is PRESERVED verbatim.
  MIN_TMP_KB            Default: 24576
  MIN_MARGIN_KB         Default: 256
  ALLOW_UNSIGNED        Default: 0
  INSTALL_DISABLED      Default: 0. Set 1 to install files but keep service disabled.
  SSH                   Default: ssh
  SCP                   Default: scp
  SSH_OPTS              Extra ssh/scp options, for example legacy KEX flags.
EOF
}

if [[ $# -ne 1 ]]; then
    usage
    exit 2
fi

if [[ -z "${API_KEY:-}" ]]; then
    echo "API_KEY is required" >&2
    usage
    exit 2
fi

if [[ -z "${SERVER_URL:-}" ]]; then
    echo "SERVER_URL is required" >&2
    usage
    exit 2
fi

ALLOW_UNSIGNED=${ALLOW_UNSIGNED:-0}
if [[ "$ALLOW_UNSIGNED" != "1" && -z "${SERVER_SHA256:-}" ]]; then
    echo "SERVER_SHA256 is required unless ALLOW_UNSIGNED=1" >&2
    exit 2
fi

if [[ -z "${PLUGIN_URL:-}" ]]; then
    echo "PLUGIN_URL is required because the WE826-Q-WD template enables Quectel LTE/comms" >&2
    exit 2
fi

if [[ "$ALLOW_UNSIGNED" != "1" && -z "${PLUGIN_SHA256:-}" ]]; then
    echo "PLUGIN_SHA256 is required unless ALLOW_UNSIGNED=1" >&2
    exit 2
fi

if [[ -z "${MUSL_LIBC_URL:-}" ]]; then
    echo "MUSL_LIBC_URL is required for the WE826-Q-WD musl runtime" >&2
    exit 2
fi

if [[ "$ALLOW_UNSIGNED" != "1" && -z "${MUSL_LIBC_SHA256:-}" ]]; then
    echo "MUSL_LIBC_SHA256 is required unless ALLOW_UNSIGNED=1" >&2
    exit 2
fi

if [[ -z "${LIBGCC_URL:-}" ]]; then
    echo "LIBGCC_URL is required for the WE826-Q-WD musl runtime" >&2
    exit 2
fi

if [[ "$ALLOW_UNSIGNED" != "1" && -z "${LIBGCC_SHA256:-}" ]]; then
    echo "LIBGCC_SHA256 is required unless ALLOW_UNSIGNED=1" >&2
    exit 2
fi

HOST=$1
ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
DEVICE_DIR="$ROOT_DIR/devices/we826-qwd"
COMMON_DIR="$ROOT_DIR/devices/common"

SERIAL=${SERIAL:-WE826-Q-WD}
LTE_INTERFACE=${LTE_INTERFACE:-usb0}
TUNNEL_URL=${TUNNEL_URL:-}
TUNNEL_KEY=${TUNNEL_KEY:-}
TUNNEL_BIND_ADDRESS=${TUNNEL_BIND_ADDRESS:-usb0}
SERVER_GZIP=${SERVER_GZIP:-1}
PLUGIN_GZIP=${PLUGIN_GZIP:-1}
MUSL_LIBC_GZIP=${MUSL_LIBC_GZIP:-1}
LIBGCC_GZIP=${LIBGCC_GZIP:-1}
MIN_TMP_KB=${MIN_TMP_KB:-24576}
MIN_MARGIN_KB=${MIN_MARGIN_KB:-256}
INSTALL_DISABLED=${INSTALL_DISABLED:-0}
# Wired-WAN preference agent. Installed ALWAYS, but ENABLED=0 by default: shipping
# it dormant means a unit can be armed later with a single config flip from
# netage-server, with no re-install and no sctl restart. See docs/adr/ADR-0010 in
# netage-fleet for why this is the one place we push routing state to a device.
WANPREF=${WANPREF:-0}
WANPREF_WAN_IF=${WANPREF_WAN_IF:-eth0}
WANPREF_LTE_IF=${WANPREF_LTE_IF:-usb0}
WANPREF_TARGETS=${WANPREF_TARGETS:-"174.138.114.209:8443 1.1.1.1:80"}
SSH=${SSH:-ssh}
SCP=${SCP:-scp}
SSH_OPTS=${SSH_OPTS:-}

for file in \
    "$COMMON_DIR/sctl-ramboot.init" \
    "$COMMON_DIR/sctl-ramboot.sh" \
    "$DEVICE_DIR/sctl.toml.template" \
    "$DEVICE_DIR/netage-wanpref.init" \
    "$DEVICE_DIR/wanpref.conf.template"; do
    if [[ ! -r "$file" ]]; then
        echo "missing required file: $file" >&2
        exit 1
    fi
done

toml_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

sed_repl_escape() {
    printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
}

shell_quote() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"
}

write_kv() {
    local key=$1
    local value=$2
    printf '%s=' "$key"
    shell_quote "$value"
    printf '\n'
}

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

sed \
    -e "s|{{API_KEY}}|$(sed_repl_escape "$(toml_escape "$API_KEY")")|g" \
    -e "s|{{SERIAL}}|$(sed_repl_escape "$(toml_escape "$SERIAL")")|g" \
    -e "s|{{LTE_INTERFACE}}|$(sed_repl_escape "$(toml_escape "$LTE_INTERFACE")")|g" \
    "$DEVICE_DIR/sctl.toml.template" > "$tmpdir/sctl.toml"

# --- [tunnel] block: emit from env, else PRESERVE whatever the device already has.
#
# WHY THIS EXISTS. This script overwrites /etc/sctl/sctl.toml wholesale. The relay
# tunnel is per-deployment (endpoint + key are environment-specific) so it was never
# templated — which meant re-running install.sh against an onboarded unit SILENTLY
# DELETED its [tunnel] block, including `bind_address`. For a device reachable only
# over that tunnel (a vehicle, a remote site) that is an unrecoverable mistake: the
# unit comes back up with no way to phone home.
#
# So: if TUNNEL_URL/TUNNEL_KEY are supplied we write a fresh block; otherwise we read
# the existing block off the device and carry it forward verbatim. Re-running this
# script is therefore non-destructive by default.
if [[ -n "$TUNNEL_URL" || -n "$TUNNEL_KEY" ]]; then
    if [[ -z "$TUNNEL_URL" || -z "$TUNNEL_KEY" ]]; then
        echo "TUNNEL_URL and TUNNEL_KEY must be set together" >&2
        exit 1
    fi
    {
        printf '\n[tunnel]\n'
        printf 'tunnel_key = "%s"\n' "$(toml_escape "$TUNNEL_KEY")"
        printf 'url = "%s"\n' "$(toml_escape "$TUNNEL_URL")"
        printf '# Pinned to an interface NAME so sctl applies SO_BINDTODEVICE — an IP\n'
        printf '# literal here would only set the source address, leaving tunnel egress\n'
        printf '# at the mercy of the default route.\n'
        printf 'bind_address = "%s"\n' "$(toml_escape "$TUNNEL_BIND_ADDRESS")"
        printf 'reconnect_delay_secs = 2\n'
        printf 'reconnect_max_delay_secs = 30\n'
        printf 'heartbeat_interval_secs = 10\n'
        printf 'heartbeat_timeout_secs = 45\n'
    } >> "$tmpdir/sctl.toml"
    printf 'tunnel:             configured from env (bind_address=%s)\n' "$TUNNEL_BIND_ADDRESS"
else
    # shellcheck disable=SC2086
    existing_tunnel=$($SSH $SSH_OPTS "$HOST" \
        "awk '/^\\[tunnel\\]/{f=1} f&&/^\\[/&&!/^\\[tunnel\\]/{f=0} f' /etc/sctl/sctl.toml 2>/dev/null" \
        || true)
    if [[ -n "${existing_tunnel//[[:space:]]/}" ]]; then
        printf '\n%s\n' "$existing_tunnel" >> "$tmpdir/sctl.toml"
        printf 'tunnel:             PRESERVED from device (%s lines)\n' \
            "$(printf '%s\n' "$existing_tunnel" | wc -l | tr -d ' ')"
    else
        printf 'tunnel:             none (device has no [tunnel] block)\n'
    fi
fi

{
    write_kv RUN_DIR "/tmp/sctl"
    write_kv CACHE_DIR "/tmp/sctl/cache"
    write_kv BIN "/tmp/sctl/sctl-server"
    write_kv PLUGIN "/tmp/sctl/lib/libsctl_comms_quectel.so"
    write_kv MUSL_LIBC "/tmp/sctl/lib/libc.so"
    write_kv LIBGCC "/tmp/sctl/lib/libgcc_s.so.1"
    write_kv SCTL_CONFIG "/etc/sctl/sctl.toml"
    write_kv SERVER_URL "$SERVER_URL"
    write_kv SERVER_SHA256 "${SERVER_SHA256:-}"
    printf 'SERVER_GZIP=%s\n' "$SERVER_GZIP"
    write_kv PLUGIN_URL "$PLUGIN_URL"
    write_kv PLUGIN_SHA256 "${PLUGIN_SHA256:-}"
    printf 'PLUGIN_GZIP=%s\n' "$PLUGIN_GZIP"
    write_kv MUSL_LIBC_URL "$MUSL_LIBC_URL"
    write_kv MUSL_LIBC_SHA256 "${MUSL_LIBC_SHA256:-}"
    printf 'MUSL_LIBC_GZIP=%s\n' "$MUSL_LIBC_GZIP"
    write_kv LIBGCC_URL "$LIBGCC_URL"
    write_kv LIBGCC_SHA256 "${LIBGCC_SHA256:-}"
    printf 'LIBGCC_GZIP=%s\n' "$LIBGCC_GZIP"
    printf 'MIN_TMP_KB=%s\n' "$MIN_TMP_KB"
    printf 'FETCH_TIMEOUT_SECS=%s\n' "${FETCH_TIMEOUT_SECS:-90}"
    printf 'CONNECT_TIMEOUT_SECS=%s\n' "${CONNECT_TIMEOUT_SECS:-10}"
    printf 'FETCH_ATTEMPTS=%s\n' "${FETCH_ATTEMPTS:-20}"
    printf 'FETCH_RETRY_SECS=%s\n' "${FETCH_RETRY_SECS:-15}"
    printf 'ALLOW_UNSIGNED=%s\n' "$ALLOW_UNSIGNED"
} > "$tmpdir/ramboot.conf"

sed \
    -e "s|{{WANPREF}}|$(sed_repl_escape "$WANPREF")|g" \
    -e "s|{{WANPREF_WAN_IF}}|$(sed_repl_escape "$WANPREF_WAN_IF")|g" \
    -e "s|{{WANPREF_LTE_IF}}|$(sed_repl_escape "$WANPREF_LTE_IF")|g" \
    -e "s|{{WANPREF_TARGETS}}|$(sed_repl_escape "$WANPREF_TARGETS")|g" \
    "$DEVICE_DIR/wanpref.conf.template" > "$tmpdir/wanpref.conf"

# The overlay budget MUST account for the wanpref files, or the guard silently
# under-reports and we can fill a 832 KB overlay.
payload_kb=$(du -k "$COMMON_DIR/sctl-ramboot.init" "$COMMON_DIR/sctl-ramboot.sh" "$tmpdir/sctl.toml" "$tmpdir/ramboot.conf" "$DEVICE_DIR/netage-wanpref.init" "$tmpdir/wanpref.conf" | awk '{s += $1} END {print s + 16}')
# shellcheck disable=SC2086
avail_kb=$($SSH $SSH_OPTS "$HOST" "df -k /overlay | awk 'NR==2 {print \$4}'")
# shellcheck disable=SC2086
existing_kb=$($SSH $SSH_OPTS "$HOST" "du -k /etc/init.d/sctl /etc/sctl/ramboot.sh /etc/sctl/ramboot.conf /etc/sctl/sctl.toml /etc/init.d/netage-wanpref /etc/sctl/wanpref.conf 2>/dev/null | awk '{s += \$1} END {print s + 0}'")
effective_avail_kb=$((avail_kb + existing_kb))
required_kb=$((payload_kb + MIN_MARGIN_KB))

printf 'overlay available: %s KB\n' "$avail_kb"
printf 'replaceable sctl:   %s KB\n' "$existing_kb"
printf 'effective space:    %s KB\n' "$effective_avail_kb"
printf 'bootstrap estimate: %s KB\n' "$payload_kb"
printf 'required w/margin:  %s KB\n' "$required_kb"

if (( effective_avail_kb < required_kb )); then
    echo "not enough overlay space for safe ramboot install" >&2
    exit 1
fi

cp "$COMMON_DIR/sctl-ramboot.init" "$tmpdir/sctl.init"
cp "$COMMON_DIR/sctl-ramboot.sh" "$tmpdir/ramboot.sh"

# shellcheck disable=SC2086
cp "$DEVICE_DIR/netage-wanpref.init" "$tmpdir/wanpref.init"

# shellcheck disable=SC2086
$SCP -O $SSH_OPTS "$tmpdir/sctl.init" "$tmpdir/ramboot.sh" "$tmpdir/ramboot.conf" "$tmpdir/sctl.toml" "$tmpdir/wanpref.init" "$tmpdir/wanpref.conf" "$HOST:/tmp/"

# shellcheck disable=SC2086
$SSH $SSH_OPTS "$HOST" INSTALL_DISABLED="$INSTALL_DISABLED" 'sh -s' <<'EOF'
set -eu

stamp=$(date +%Y%m%d%H%M%S)
mkdir -p /etc/sctl/playbooks

[ ! -e /etc/init.d/sctl ] || cp /etc/init.d/sctl /tmp/sctl.init.backup.$stamp
[ ! -e /etc/sctl/ramboot.sh ] || cp /etc/sctl/ramboot.sh /tmp/sctl.ramboot.backup.$stamp
[ ! -e /etc/sctl/ramboot.conf ] || cp /etc/sctl/ramboot.conf /tmp/sctl.ramboot.conf.backup.$stamp
[ ! -e /etc/sctl/sctl.toml ] || cp /etc/sctl/sctl.toml /tmp/sctl.toml.backup.$stamp

/etc/init.d/sctl stop >/dev/null 2>&1 || true

cp /tmp/sctl.init /etc/init.d/sctl
cp /tmp/ramboot.sh /etc/sctl/ramboot.sh
cp /tmp/ramboot.conf /etc/sctl/ramboot.conf
cp /tmp/sctl.toml /etc/sctl/sctl.toml
cp /tmp/wanpref.init /etc/init.d/netage-wanpref
cp /tmp/wanpref.conf /etc/sctl/wanpref.conf

chmod 0755 /etc/init.d/sctl /etc/sctl/ramboot.sh /etc/init.d/netage-wanpref
chmod 0600 /etc/sctl/ramboot.conf /etc/sctl/sctl.toml
chmod 0644 /etc/sctl/wanpref.conf

# Installed always; armed only when its own conf says ENABLED=1. Starting it while
# disabled is deliberate — the loop idles, writes state, and can be armed later by
# a config flip alone, with no re-install and no sctl restart.
/etc/init.d/netage-wanpref enable >/dev/null 2>&1 || true
/etc/init.d/netage-wanpref stop  >/dev/null 2>&1 || true
/etc/init.d/netage-wanpref start >/dev/null 2>&1 || true

if [ "$INSTALL_DISABLED" = "1" ]; then
    touch /etc/sctl/disabled
else
    rm -f /etc/sctl/disabled
fi

/etc/init.d/sctl enable
if [ "$INSTALL_DISABLED" != "1" ]; then
    /etc/init.d/sctl restart
fi

rm -f /tmp/sctl.init /tmp/ramboot.sh /tmp/ramboot.conf /tmp/sctl.toml /tmp/wanpref.init /tmp/wanpref.conf
df -h /overlay /tmp
EOF
