//! Platform-specific self-heal and bootstrap helpers.
//!
//! Each submodule is gated on a runtime detection (e.g. `is_openwrt()`), so
//! the same binary is safe to run on dev hosts, OpenWrt devices, and other
//! Linux distros — non-matching platforms become no-ops.

pub mod openwrt;
