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
# The GL-XE300 runs OpenWrt 19.07.8 on ath79/NAND, but the ath79 generic/nand split
# is a kernel + flash-layout distinction, not a toolchain one: both subtargets ship
# gcc 7.5.0 + musl and unpack the same toolchain-mips_24kc_gcc-7.5.0_musl. The
# userland binaries are interchangeable, so the generic target script is reused
# verbatim rather than forked. The 19.07.10 SDK against a 19.07.8 device is likewise
# a non-issue inside the series (musl 1.1.24 throughout).
# shellcheck source=../targets/openwrt-19.07-ath79-generic-mips_24kc.sh
source "$REPO_ROOT/devices/targets/openwrt-19.07-ath79-generic-mips_24kc.sh"

ARTIFACT_DIR="$REPO_ROOT/.artifacts/xe300"
SERVER_SRC="$REPO_ROOT/target/$RUST_TARGET/release/sctl"
PLUGIN_SRC="$REPO_ROOT/target/$RUST_TARGET/release/libsctl_comms_quectel.so"
SERVER_OUT="$ARTIFACT_DIR/sctl-server-mips_24kc"
PLUGIN_OUT="$ARTIFACT_DIR/sctl-comms-quectel-mips_24kc.so"
SERVER_GZ="$SERVER_OUT.gz"
PLUGIN_GZ="$PLUGIN_OUT.gz"

need_cmd gzip
ensure_rust_build_std
ensure_openwrt_sdk
setup_openwrt_target_env

mkdir -p "$ARTIFACT_DIR"

build_cargo_release_build_std "$REPO_ROOT/server/Cargo.toml"
build_cargo_release_build_std "$REPO_ROOT/drivers/sctl-comms-quectel/Cargo.toml"

# Only two payloads. The WE826 additionally ships musl + libgcc because its stock
# firmware is uClibc with no OpenWrt loader; the XE300 IS OpenWrt 19.07 with musl
# 1.1.24 and runs these binaries under its own /lib/libc.so (verified on hardware).
copy_and_gzip "$SERVER_SRC" "$SERVER_OUT" "$SERVER_GZ"
copy_and_gzip "$PLUGIN_SRC" "$PLUGIN_OUT" "$PLUGIN_GZ"

echo
echo "GL-XE300 artifacts:"
print_artifact_sizes "$SERVER_OUT" "$SERVER_GZ" "$PLUGIN_OUT" "$PLUGIN_GZ"
echo
# Hashes are printed for remote (file-API / STP) pushes to fielded units, where the
# installer's scp path is not available and the payload must be verified on-device.
echo "Payload hashes:"
sha256sum "$SERVER_GZ" "$PLUGIN_GZ"
