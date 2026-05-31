#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat >&2 <<'EOF'
Usage:
  devices/build.sh <device>

Examples:
  devices/build.sh rut241
EOF
}

if [[ $# -ne 1 ]]; then
    usage
    exit 2
fi

DEVICE=$1
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
BUILD_SCRIPT="$SCRIPT_DIR/$DEVICE/build.sh"

if [[ ! -x "$BUILD_SCRIPT" ]]; then
    echo "unknown or non-buildable device target: $DEVICE" >&2
    echo "expected executable: $BUILD_SCRIPT" >&2
    exit 1
fi

exec "$BUILD_SCRIPT"
