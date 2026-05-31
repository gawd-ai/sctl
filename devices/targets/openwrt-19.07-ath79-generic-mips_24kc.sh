#!/usr/bin/env bash

TARGET_NAME=openwrt-19.07-ath79-generic-mips_24kc
RUST_TARGET=mips-unknown-linux-musl
OPENWRT_TRIPLE=mips-openwrt-linux-musl
SDK_ARCHIVE=openwrt-sdk-19.07.10-ath79-generic_gcc-7.5.0_musl.Linux-x86_64.tar.xz
SDK_DIR=openwrt-sdk-19.07.10-ath79-generic_gcc-7.5.0_musl.Linux-x86_64
SDK_URL=https://downloads.openwrt.org/releases/19.07.10/targets/ath79/generic/$SDK_ARCHIVE
TOOLCHAIN_REL=staging_dir/toolchain-mips_24kc_gcc-7.5.0_musl

# The WE826 stock firmware is uClibc-based and does not have OpenWrt's musl
# dynamic loader. Build dynamic musl payloads, package the musl runtime, and
# invoke the loader from tmpfs.
OPENWRT_LINK_MODE=dynamic
