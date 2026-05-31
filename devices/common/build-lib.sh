#!/usr/bin/env bash

need_cmd() {
    local cmd=$1
    if ! command -v "$cmd" >/dev/null 2>&1; then
        echo "missing required command: $cmd" >&2
        exit 1
    fi
}

repo_root_from() {
    local start=$1
    git -C "$start" rev-parse --show-toplevel 2>/dev/null
}

build_number() {
    git -C "$REPO_ROOT" rev-list --count HEAD 2>/dev/null || echo 0
}

ensure_rust_build_std() {
    need_cmd cargo
    need_cmd rustup
    if ! rustup toolchain list | grep -q '^nightly-'; then
        echo "nightly Rust toolchain is required for -Z build-std" >&2
        echo "install with: rustup toolchain install nightly" >&2
        exit 1
    fi
    if ! rustup component list --installed | grep -q '^rust-src'; then
        echo "rust-src component is required for -Z build-std" >&2
        echo "install with: rustup component add rust-src" >&2
        exit 1
    fi
}

ensure_openwrt_sdk() {
    need_cmd curl
    need_cmd tar

    mkdir -p "$REPO_ROOT/.toolchains"

    local archive="$REPO_ROOT/.toolchains/$SDK_ARCHIVE"
    local sdk_dir="$REPO_ROOT/.toolchains/$SDK_DIR"

    if [[ ! -f "$archive" ]]; then
        echo "downloading OpenWrt SDK: $SDK_URL"
        curl -fL --progress-bar -o "$archive" "$SDK_URL"
    fi

    if [[ ! -d "$sdk_dir" ]]; then
        echo "extracting OpenWrt SDK: $SDK_DIR"
        tar -C "$REPO_ROOT/.toolchains" -xf "$archive"
    fi

    local linker="$sdk_dir/$TOOLCHAIN_REL/bin/$OPENWRT_TRIPLE-gcc"
    if [[ ! -x "$linker" ]]; then
        echo "missing OpenWrt linker: $linker" >&2
        exit 1
    fi
}

setup_openwrt_target_env() {
    local sdk_dir="$REPO_ROOT/.toolchains/$SDK_DIR"
    local toolchain="$sdk_dir/$TOOLCHAIN_REL"
    local linker="$toolchain/bin/$OPENWRT_TRIPLE-gcc"

    export STAGING_DIR="$sdk_dir/staging_dir"
    export PATH="$toolchain/bin:$PATH"
    export SCTL_BUILD_NUMBER
    SCTL_BUILD_NUMBER=$(build_number)
    if [[ "${OPENWRT_LINK_MODE:-dynamic}" == "dynamic" ]]; then
        export RUSTFLAGS="${RUSTFLAGS:-} -C target-feature=-crt-static"
    else
        export RUSTFLAGS="${RUSTFLAGS:-}"
    fi

    local env_target
    env_target=$(printf '%s' "$RUST_TARGET" | tr '[:lower:]-' '[:upper:]_')
    export "CARGO_TARGET_${env_target}_LINKER=$linker"
    export "CC_${RUST_TARGET//-/_}=$linker"
    export "AR_${RUST_TARGET//-/_}=$toolchain/bin/$OPENWRT_TRIPLE-ar"
}

build_cargo_release_build_std() {
    local manifest=$1
    echo "building $(dirname "$manifest") for $RUST_TARGET"
    cargo +nightly build -Z build-std=std,panic_abort \
        --manifest-path "$manifest" \
        --release \
        --target "$RUST_TARGET"
}

copy_and_gzip() {
    local src=$1
    local dst=$2
    local gz=$3

    install -m 0755 "$src" "$dst"
    gzip -9 -c "$dst" > "$gz"
}

print_artifact_sizes() {
    local file
    for file in "$@"; do
        if [[ -f "$file" ]]; then
            printf '%10s  %s\n' "$(wc -c < "$file")" "${file#$REPO_ROOT/}"
        fi
    done
}
