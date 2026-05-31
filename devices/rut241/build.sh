#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(repo_root_fallback() {
    if git -C "$SCRIPT_DIR" rev-parse --show-toplevel 2>/dev/null; then
        return
    fi
    cd "$SCRIPT_DIR/../.." && pwd
}; repo_root_fallback)

# shellcheck source=../common/build-lib.sh
source "$REPO_ROOT/devices/common/build-lib.sh"
# shellcheck source=../targets/openwrt-21.02-ramips-mt76x8-mipsel_24kc.sh
source "$REPO_ROOT/devices/targets/openwrt-21.02-ramips-mt76x8-mipsel_24kc.sh"

ARTIFACT_DIR="$REPO_ROOT/.artifacts/rut241"
SERVER_SRC="$REPO_ROOT/server/target/$RUST_TARGET/release/sctl"
PLUGIN_SRC="$REPO_ROOT/drivers/sctl-comms-quectel/target/$RUST_TARGET/release/libsctl_comms_quectel.so"
SERVER_OUT="$ARTIFACT_DIR/sctl-server-mipsel_24kc"
PLUGIN_OUT="$ARTIFACT_DIR/sctl-comms-quectel-mipsel_24kc.so"
SERVER_GZ="$SERVER_OUT.gz"
PLUGIN_GZ="$PLUGIN_OUT.gz"

need_cmd gzip
ensure_rust_build_std
ensure_openwrt_sdk
setup_openwrt_target_env

mkdir -p "$ARTIFACT_DIR"

build_cargo_release_build_std "$REPO_ROOT/server/Cargo.toml"
build_cargo_release_build_std "$REPO_ROOT/drivers/sctl-comms-quectel/Cargo.toml"

copy_and_gzip "$SERVER_SRC" "$SERVER_OUT" "$SERVER_GZ"
copy_and_gzip "$PLUGIN_SRC" "$PLUGIN_OUT" "$PLUGIN_GZ"

echo
echo "RUT241 artifacts:"
print_artifact_sizes "$SERVER_OUT" "$SERVER_GZ" "$PLUGIN_OUT" "$PLUGIN_GZ"
