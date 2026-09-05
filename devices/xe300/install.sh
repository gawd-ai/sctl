#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage:
  API_KEY=... devices/xe300/install.sh root@ROUTER_IP

Environment:
  API_KEY               Required. Auth token written to /etc/sctl/sctl.toml.
  SERIAL                Default: GL-XE300. Use model+MAC (e.g. XE300-9483C446D42F);
                        devices.agent_serial is UNIQUE fleet-side and a bare model
                        name collides at the second unit.
  LTE_INTERFACE         Default: wwan0  (the netdev holding the bearer IPv4)
  MIN_MARGIN_KB         Default: 4096
  TUNNEL_URL            Optional. Set together with TUNNEL_KEY to write a fresh
                        [tunnel] block. If neither is set, an existing on-device
                        [tunnel] block is PRESERVED (see below).
  TUNNEL_KEY            Optional. See TUNNEL_URL.
  TUNNEL_BIND_ADDRESS   Optional interface NAME to pin tunnel egress with
                        SO_BINDTODEVICE. Leave EMPTY on a multi-WAN site unit: a
                        pin makes the tunnel ignore routing entirely and retry a
                        dead interface forever, which is what caused the WE826's
                        availability flapping. Only pin when egress must not follow
                        the default route.
  SSH / SCP / SSH_OPTS  Transport overrides. SSH_OPTS defaults to the ssh-rsa
                        algorithm flags this device's dropbear 2019.78 requires.

The AT port is auto-detected (USB interface :1.2), never hardcoded, so a ttyUSB
re-enumeration cannot take the modem down.
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

HOST=$1
ROOT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
DEVICE_DIR="$ROOT_DIR/devices/xe300"
ARTIFACT_DIR="$ROOT_DIR/.artifacts/xe300"
SERVER_GZ="$ARTIFACT_DIR/sctl-server-mips_24kc.gz"
PLUGIN_GZ="$ARTIFACT_DIR/sctl-comms-quectel-mips_24kc.so.gz"
SERIAL=${SERIAL:-GL-XE300}
LTE_INTERFACE=${LTE_INTERFACE:-wwan0}
MIN_MARGIN_KB=${MIN_MARGIN_KB:-4096}
TUNNEL_URL=${TUNNEL_URL:-}
TUNNEL_KEY=${TUNNEL_KEY:-}
TUNNEL_BIND_ADDRESS=${TUNNEL_BIND_ADDRESS:-}

# GL.iNet firmware 3.x ships dropbear 2019.78: its host key is ssh-rsa only, and it
# predates ed25519 client-key support, so the client key must be RSA and modern
# OpenSSH needs both algorithms re-enabled. Legacy KEX is NOT required (2019.78 does
# curve25519), and plain scp works (no -O).
SSH=${SSH:-ssh}
SCP=${SCP:-scp}
SSH_OPTS=${SSH_OPTS:--o HostKeyAlgorithms=+ssh-rsa -o PubkeyAcceptedAlgorithms=+ssh-rsa}

for file in "$SERVER_GZ" "$PLUGIN_GZ" "$DEVICE_DIR/sctl.init" "$DEVICE_DIR/sctl.toml.template"; do
    if [[ ! -r "$file" ]]; then
        echo "missing required file: $file" >&2
        exit 1
    fi
done

toml_escape() {
    printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g'
}

# Escape a value for the replacement side of `sed s|...|VALUE|`: backslash, the
# `|` delimiter, and `&`. Keeps a key containing those characters from
# corrupting the generated sctl.toml.
sed_repl_escape() {
    printf '%s' "$1" | sed 's/[\\&|]/\\&/g'
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
# tunnel is per-deployment (endpoint + key are environment-specific) so it is not
# templated — which means re-running install.sh against an onboarded unit would
# SILENTLY DELETE its [tunnel] block. For a device reachable only over that tunnel
# (a remote site, a vehicle) that is an unrecoverable mistake: the unit comes back
# up with no way to phone home. So: env writes a fresh block, otherwise the existing
# block is read off the device and carried forward verbatim. Re-running this script
# is therefore non-destructive by default.
if [[ -n "$TUNNEL_URL" || -n "$TUNNEL_KEY" ]]; then
    if [[ -z "$TUNNEL_URL" || -z "$TUNNEL_KEY" ]]; then
        echo "TUNNEL_URL and TUNNEL_KEY must be set together" >&2
        exit 1
    fi
    {
        printf '\n[tunnel]\n'
        printf 'tunnel_key = "%s"\n' "$(toml_escape "$TUNNEL_KEY")"
        printf 'url = "%s"\n' "$(toml_escape "$TUNNEL_URL")"
        if [[ -n "$TUNNEL_BIND_ADDRESS" ]]; then
            printf '# Pinned to an interface NAME so sctl applies SO_BINDTODEVICE — an IP\n'
            printf '# literal here would only set the source address, leaving tunnel egress\n'
            printf '# at the mercy of the default route.\n'
            printf 'bind_address = "%s"\n' "$(toml_escape "$TUNNEL_BIND_ADDRESS")"
        fi
        printf 'reconnect_delay_secs = 2\n'
        printf 'reconnect_max_delay_secs = 30\n'
        printf 'heartbeat_interval_secs = 10\n'
        printf 'heartbeat_timeout_secs = 45\n'
    } >> "$tmpdir/sctl.toml"
    printf 'tunnel:             configured from env (bind_address=%s)\n' \
        "${TUNNEL_BIND_ADDRESS:-<unpinned, follows routing>}"
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

payload_kb=$(du -k "$SERVER_GZ" "$PLUGIN_GZ" | awk '{s += $1} END {print s + 32}')
# shellcheck disable=SC2086
avail_kb=$($SSH $SSH_OPTS "$HOST" "df -k /overlay | awk 'NR==2 {print \$4}'")
# shellcheck disable=SC2086
existing_kb=$($SSH $SSH_OPTS "$HOST" "du -k /usr/local/lib/sctl/sctl-server-mips_24kc.gz /usr/local/lib/sctl/sctl-comms-quectel-mips_24kc.so.gz 2>/dev/null | awk '{s += \$1} END {print s + 0}'")
effective_avail_kb=$((avail_kb + existing_kb))
required_kb=$((payload_kb + MIN_MARGIN_KB))

printf 'serial:             %s\n' "$SERIAL"
printf 'lte interface:      %s\n' "$LTE_INTERFACE"
printf 'overlay available:  %s KB\n' "$avail_kb"
printf 'replaceable sctl:   %s KB\n' "$existing_kb"
printf 'effective space:    %s KB\n' "$effective_avail_kb"
printf 'payload estimate:   %s KB\n' "$payload_kb"
printf 'required w/margin:  %s KB\n' "$required_kb"

if (( effective_avail_kb < required_kb )); then
    echo "not enough overlay space for safe install" >&2
    exit 1
fi

# shellcheck disable=SC2086
$SCP $SSH_OPTS "$SERVER_GZ" "$PLUGIN_GZ" "$DEVICE_DIR/sctl.init" "$tmpdir/sctl.toml" "$HOST:/tmp/"

# shellcheck disable=SC2086
$SSH $SSH_OPTS "$HOST" 'sh -s' <<'EOF'
set -eu

stamp=$(date +%Y%m%d%H%M%S)
[ ! -e /etc/init.d/sctl ] || cp /etc/init.d/sctl /tmp/sctl.init.backup.$stamp
[ ! -e /etc/sctl/sctl.toml ] || cp /etc/sctl/sctl.toml /tmp/sctl.toml.backup.$stamp

/etc/init.d/sctl stop >/dev/null 2>&1 || true

mkdir -p /usr/local/lib/sctl/data /etc/sctl/playbooks
cp /tmp/sctl-server-mips_24kc.gz /usr/local/lib/sctl/sctl-server-mips_24kc.gz
cp /tmp/sctl-comms-quectel-mips_24kc.so.gz /usr/local/lib/sctl/sctl-comms-quectel-mips_24kc.so.gz
cp /tmp/sctl.init /etc/init.d/sctl
cp /tmp/sctl.toml /etc/sctl/sctl.toml

chmod 0644 /usr/local/lib/sctl/*.gz
chmod 0755 /etc/init.d/sctl
chmod 0600 /etc/sctl/sctl.toml

/etc/init.d/sctl enable
# `start`, not `restart`: the service was explicitly stopped above (to release the
# old binary before overwriting it), and rc.common's `restart` would run its stop
# half against an already-stopped service, which procd answers with a bare
# "Command failed: Not found" on stderr. Harmless, but it reads as a failed install.
/etc/init.d/sctl start

rm -f /tmp/sctl-server-mips_24kc.gz \
      /tmp/sctl-comms-quectel-mips_24kc.so.gz \
      /tmp/sctl.init \
      /tmp/sctl.toml

sleep 8

# NOT `/etc/init.d/sctl status`: OpenWrt 19.07's rc.common exposes no `status` verb
# for procd services (RutOS does, which is why the rut241 installer can call it).
# Ask procd directly instead.
ubus call service list '{"name":"sctl"}' 2>/dev/null || ps w | grep '[s]ctl-server' || echo "sctl: NOT RUNNING"
df -h /overlay /tmp
EOF
