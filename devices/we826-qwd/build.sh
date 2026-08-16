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
# shellcheck source=../targets/openwrt-19.07-ath79-generic-mips_24kc.sh
source "$REPO_ROOT/devices/targets/openwrt-19.07-ath79-generic-mips_24kc.sh"

ARTIFACT_DIR="$REPO_ROOT/.artifacts/we826-qwd"
SERVER_SRC="$REPO_ROOT/target/$RUST_TARGET/release/sctl"
PLUGIN_SRC="$REPO_ROOT/target/$RUST_TARGET/release/libsctl_comms_quectel.so"
MUSL_LIBC_SRC="$REPO_ROOT/.toolchains/$SDK_DIR/$TOOLCHAIN_REL/lib/libc.so"
LIBGCC_SRC="$REPO_ROOT/.toolchains/$SDK_DIR/$TOOLCHAIN_REL/lib/libgcc_s.so.1"
SERVER_OUT="$ARTIFACT_DIR/sctl-server-mips_24kc"
PLUGIN_OUT="$ARTIFACT_DIR/sctl-comms-quectel-mips_24kc.so"
MUSL_LIBC_OUT="$ARTIFACT_DIR/libc-mips_24kc.so"
LIBGCC_OUT="$ARTIFACT_DIR/libgcc_s-mips_24kc.so.1"
SERVER_GZ="$SERVER_OUT.gz"
PLUGIN_GZ="$PLUGIN_OUT.gz"
MUSL_LIBC_GZ="$MUSL_LIBC_OUT.gz"
LIBGCC_GZ="$LIBGCC_OUT.gz"

need_cmd gzip
ensure_rust_build_std
ensure_openwrt_sdk
setup_openwrt_target_env

mkdir -p "$ARTIFACT_DIR"

build_cargo_release_build_std "$REPO_ROOT/server/Cargo.toml"
build_cargo_release_build_std "$REPO_ROOT/drivers/sctl-comms-quectel/Cargo.toml"

copy_and_gzip "$SERVER_SRC" "$SERVER_OUT" "$SERVER_GZ"
copy_and_gzip "$PLUGIN_SRC" "$PLUGIN_OUT" "$PLUGIN_GZ"
copy_and_gzip "$MUSL_LIBC_SRC" "$MUSL_LIBC_OUT" "$MUSL_LIBC_GZ"
copy_and_gzip "$LIBGCC_SRC" "$LIBGCC_OUT" "$LIBGCC_GZ"

echo
echo "WE826-Q-WD RAM payload artifacts:"
print_artifact_sizes "$SERVER_OUT" "$SERVER_GZ" \
    "$PLUGIN_OUT" "$PLUGIN_GZ" \
    "$MUSL_LIBC_OUT" "$MUSL_LIBC_GZ" \
    "$LIBGCC_OUT" "$LIBGCC_GZ"
echo
echo "Payload hashes:"
sha256sum "$SERVER_GZ" "$PLUGIN_GZ" "$MUSL_LIBC_GZ" "$LIBGCC_GZ"
