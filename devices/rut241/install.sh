#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage:
  API_KEY=... devices/rut241/install.sh root@ROUTER_IP

Environment:
  API_KEY        Required. Auth token written to /etc/sctl/sctl.toml.
  SERIAL         Default: RUT241
  LTE_INTERFACE  Default: qmimux0  (QMI data bearer; drives NOCONN->CONNECT)
  MIN_MARGIN_KB  Default: 768

Note: the AT port is auto-detected (USB interface :1.2), never hardcoded, so a
ttyUSB re-enumeration cannot take the modem down. A [tunnel] (OOB relay) block is
NOT written by this installer and must be re-added after install if used.
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
DEVICE_DIR="$ROOT_DIR/devices/rut241"
ARTIFACT_DIR="$ROOT_DIR/.artifacts/rut241"
SERVER_GZ="$ARTIFACT_DIR/sctl-server-mipsel_24kc.gz"
PLUGIN_GZ="$ARTIFACT_DIR/sctl-comms-quectel-mipsel_24kc.so.gz"
SERIAL=${SERIAL:-RUT241}
LTE_INTERFACE=${LTE_INTERFACE:-qmimux0}
MIN_MARGIN_KB=${MIN_MARGIN_KB:-768}

for file in "$SERVER_GZ" "$PLUGIN_GZ" "$DEVICE_DIR/sctl.init" "$DEVICE_DIR/sctl.toml.template"; do
    if [[ ! -r "$file" ]]; then
        echo "missing required file: $file" >&2
        exit 1
    fi
done

tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

# Escape a value for the replacement side of `sed s|...|VALUE|`: backslash, the
# `|` delimiter, and `&`. Keeps a key containing those characters from
# corrupting the generated sctl.toml.
sed_repl_escape() { printf '%s' "$1" | sed -e 's/[\\&|]/\\&/g'; }

sed \
    -e "s|{{API_KEY}}|$(sed_repl_escape "$API_KEY")|g" \
    -e "s|{{SERIAL}}|$(sed_repl_escape "$SERIAL")|g" \
    -e "s|{{LTE_INTERFACE}}|$(sed_repl_escape "$LTE_INTERFACE")|g" \
    "$DEVICE_DIR/sctl.toml.template" > "$tmpdir/sctl.toml"

payload_kb=$(du -k "$SERVER_GZ" "$PLUGIN_GZ" | awk '{s += $1} END {print s + 32}')
avail_kb=$(ssh "$HOST" "df -k /overlay | awk 'NR==2 {print \$4}'")
existing_kb=$(ssh "$HOST" "du -k /usr/local/lib/sctl/sctl-server-mipsel_24kc.gz /usr/local/lib/sctl/sctl-comms-quectel-mipsel_24kc.so.gz 2>/dev/null | awk '{s += \$1} END {print s + 0}'")
effective_avail_kb=$((avail_kb + existing_kb))
required_kb=$((payload_kb + MIN_MARGIN_KB))

printf 'overlay available: %s KB\n' "$avail_kb"
printf 'replaceable sctl:   %s KB\n' "$existing_kb"
printf 'effective space:    %s KB\n' "$effective_avail_kb"
printf 'payload estimate:   %s KB\n' "$payload_kb"
printf 'required w/margin:  %s KB\n' "$required_kb"

if (( effective_avail_kb < required_kb )); then
    echo "not enough overlay space for safe install" >&2
    exit 1
fi

scp "$SERVER_GZ" "$PLUGIN_GZ" "$DEVICE_DIR/sctl.init" "$tmpdir/sctl.toml" "$HOST:/tmp/"

ssh "$HOST" <<'EOF'
set -eu

stamp=$(date +%Y%m%d%H%M%S)
[ ! -e /etc/init.d/sctl ] || cp /etc/init.d/sctl /tmp/sctl.init.backup.$stamp
[ ! -e /etc/sctl/sctl.toml ] || cp /etc/sctl/sctl.toml /tmp/sctl.toml.backup.$stamp

/etc/init.d/sctl stop >/dev/null 2>&1 || true

mkdir -p /usr/local/lib/sctl /etc/sctl/playbooks
cp /tmp/sctl-server-mipsel_24kc.gz /usr/local/lib/sctl/sctl-server-mipsel_24kc.gz
cp /tmp/sctl-comms-quectel-mipsel_24kc.so.gz /usr/local/lib/sctl/sctl-comms-quectel-mipsel_24kc.so.gz
cp /tmp/sctl.init /etc/init.d/sctl
cp /tmp/sctl.toml /etc/sctl/sctl.toml

chmod 0644 /usr/local/lib/sctl/*.gz
chmod 0755 /etc/init.d/sctl
chmod 0600 /etc/sctl/sctl.toml

/etc/init.d/sctl enable
/etc/init.d/sctl restart

rm -f /tmp/sctl-server-mipsel_24kc.gz \
      /tmp/sctl-comms-quectel-mipsel_24kc.so.gz \
      /tmp/sctl.init \
      /tmp/sctl.toml

sleep 8
/etc/init.d/sctl status
df -h /overlay /tmp
EOF
