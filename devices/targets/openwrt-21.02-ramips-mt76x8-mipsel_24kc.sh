#!/usr/bin/env bash

TARGET_NAME=openwrt-21.02-ramips-mt76x8-mipsel_24kc
RUST_TARGET=mipsel-unknown-linux-musl
OPENWRT_TRIPLE=mipsel-openwrt-linux-musl
SDK_ARCHIVE=openwrt-sdk-21.02.0-ramips-mt76x8_gcc-8.4.0_musl.Linux-x86_64.tar.xz
SDK_DIR=openwrt-sdk-21.02.0-ramips-mt76x8_gcc-8.4.0_musl.Linux-x86_64
SDK_URL=https://downloads.openwrt.org/releases/21.02.0/targets/ramips/mt76x8/$SDK_ARCHIVE
TOOLCHAIN_REL=staging_dir/toolchain-mipsel_24kc_gcc-8.4.0_musl
