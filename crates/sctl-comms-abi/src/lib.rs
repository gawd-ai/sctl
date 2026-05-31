//! Stable C ABI shared by `sctl` and dynamically loaded comms plugins.
//!
//! This crate intentionally has no dependencies. The ABI boundary is C-shaped:
//! fixed-layout structs, function pointers, caller-owned buffers, and integer
//! result codes. Rust traits and heap-owned Rust types stay on either side.

#![deny(unsafe_op_in_unsafe_fn)]

use core::ffi::{c_char, c_void};

pub const SCTL_COMMS_ABI_VERSION: u32 = 1;
pub const SCTL_COMMS_MAX_STR: usize = 128;
pub const SCTL_COMMS_MAX_BANDS: usize = 128;

pub const SCTL_COMMS_OK: i32 = 0;
pub const SCTL_COMMS_ERR: i32 = -1;
pub const SCTL_COMMS_ERR_UNSUPPORTED: i32 = -2;
pub const SCTL_COMMS_ERR_INVALID: i32 = -3;
pub const SCTL_COMMS_ERR_BUFFER_TOO_SMALL: i32 = -4;
pub const SCTL_COMMS_ERR_MODEM_UNAVAILABLE: i32 = -5;
pub const SCTL_COMMS_ERR_AT_FAILED: i32 = -6;
pub const SCTL_COMMS_ERR_TUNNEL_CONNECTED: i32 = -7;
pub const SCTL_COMMS_ERR_SCAN_RUNNING: i32 = -8;

pub const SCTL_COMMS_LOG_ERROR: i32 = 1;
pub const SCTL_COMMS_LOG_WARN: i32 = 2;
pub const SCTL_COMMS_LOG_INFO: i32 = 3;
pub const SCTL_COMMS_LOG_DEBUG: i32 = 4;

pub const SCTL_COMMS_CAP_LOCATION_GNSS: u64 = 1 << 0;
pub const SCTL_COMMS_CAP_LINK_CELLULAR: u64 = 1 << 1;
pub const SCTL_COMMS_CAP_CELLULAR_BAND_CONTROL: u64 = 1 << 2;
pub const SCTL_COMMS_CAP_CELLULAR_SCAN: u64 = 1 << 3;
pub const SCTL_COMMS_CAP_RECOVERY_USB_CYCLE: u64 = 1 << 4;
pub const SCTL_COMMS_CAP_RECOVERY_TUNNEL_WATCHDOG: u64 = 1 << 5;

pub const SCTL_COMMS_SPEED_DOWNLOAD: i32 = 1;
pub const SCTL_COMMS_SPEED_UPLOAD: i32 = 2;

pub mod capabilities {
    pub const LOCATION_GNSS: &str = "location.gnss";
    pub const LINK_CELLULAR: &str = "link.cellular";
    pub const CELLULAR_BAND_CONTROL: &str = "cellular.band_control";
    pub const CELLULAR_SCAN: &str = "cellular.scan";
    pub const RECOVERY_USB_CYCLE: &str = "recovery.usb_cycle";
    pub const RECOVERY_TUNNEL_WATCHDOG: &str = "recovery.tunnel_watchdog";
}

pub mod methods {
    pub const HELLO: &str = "hello";
    pub const DETECT: &str = "detect";
    pub const OPEN: &str = "open";
    pub const STATUS: &str = "status";
    pub const CAPABILITIES: &str = "capabilities";
    pub const LOCATION_POLL: &str = "location.poll";
    pub const LOCATION_DISABLE: &str = "location.disable";
    pub const LINK_POLL: &str = "link.poll";
    pub const LINK_SPEED_TEST: &str = "link.speed_test";
    pub const CELLULAR_SET_BANDS: &str = "cellular.set_bands";
    pub const CELLULAR_SCAN: &str = "cellular.scan";
    pub const RECOVERY_USB_CYCLE: &str = "recovery.usb_cycle";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsSlice {
    pub ptr: *const u8,
    pub len: usize,
}

impl SctlCommsSlice {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            ptr: core::ptr::null(),
            len: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsMutSlice {
    pub ptr: *mut u8,
    pub cap: usize,
    pub len: *mut usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsHostV1 {
    pub abi_version: u32,
    pub ctx: *mut c_void,
    pub log: Option<extern "C" fn(*mut c_void, i32, SctlCommsSlice)>,
    pub now_ms: Option<extern "C" fn(*mut c_void) -> u64>,
    pub at_command:
        Option<extern "C" fn(*mut c_void, SctlCommsSlice, u64, SctlCommsMutSlice) -> i32>,
    pub interface_has_ipv4: Option<extern "C" fn(*mut c_void, SctlCommsSlice) -> bool>,
    pub speed_test:
        Option<extern "C" fn(*mut c_void, i32, SctlCommsSlice, SctlCommsSlice, *mut u64) -> i32>,
    pub usb_cycle: Option<extern "C" fn(*mut c_void, SctlCommsMutSlice) -> i32>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsProbeResult {
    pub capabilities: u64,
    pub detected_path: [c_char; SCTL_COMMS_MAX_STR],
}

impl Default for SctlCommsProbeResult {
    fn default() -> Self {
        Self {
            capabilities: 0,
            detected_path: [0; SCTL_COMMS_MAX_STR],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsOpenConfig {
    pub provider: SctlCommsSlice,
    pub device: SctlCommsSlice,
    pub data_dir: SctlCommsSlice,
    pub gps_enabled: bool,
    pub gps_auto_enable: bool,
    pub gps_history_size: usize,
    pub lte_enabled: bool,
    pub lte_watchdog: bool,
    pub lte_interface: SctlCommsSlice,
    pub speed_test_url: SctlCommsSlice,
    pub speed_test_upload_url: SctlCommsSlice,
    pub tunnel_url: SctlCommsSlice,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsStatusParams {
    pub tunnel_connected: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsLinkPollParams {
    pub refresh: bool,
    pub tunnel_connected: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsSetBandsParams {
    pub mode: i32,
    pub bands: [u16; SCTL_COMMS_MAX_BANDS],
    pub bands_len: usize,
    pub priority_band: u16,
    pub has_priority_band: bool,
    pub force: bool,
    pub tunnel_connected: bool,
}

pub const SCTL_COMMS_BAND_MODE_AUTO: i32 = 1;
pub const SCTL_COMMS_BAND_MODE_LOCKED: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsScanParams {
    pub bands: [u16; SCTL_COMMS_MAX_BANDS],
    pub bands_len: usize,
    pub include_speed_test: bool,
    pub force: bool,
    pub tunnel_connected: bool,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct SctlCommsPluginV1 {
    pub abi_version: u32,
    pub plugin_ctx: *mut c_void,
    pub destroy: Option<extern "C" fn(*mut c_void)>,
    pub probe: Option<extern "C" fn(*mut c_void, *mut SctlCommsProbeResult) -> i32>,
    pub open:
        Option<extern "C" fn(*mut c_void, *const SctlCommsOpenConfig, SctlCommsMutSlice) -> i32>,
    pub close: Option<extern "C" fn(*mut c_void) -> i32>,
    pub status:
        Option<extern "C" fn(*mut c_void, *const SctlCommsStatusParams, SctlCommsMutSlice) -> i32>,
    pub poll_location: Option<extern "C" fn(*mut c_void, SctlCommsMutSlice) -> i32>,
    pub disable_location: Option<extern "C" fn(*mut c_void, SctlCommsMutSlice) -> i32>,
    pub poll_link: Option<
        extern "C" fn(*mut c_void, *const SctlCommsLinkPollParams, SctlCommsMutSlice) -> i32,
    >,
    pub set_bands: Option<
        extern "C" fn(*mut c_void, *const SctlCommsSetBandsParams, SctlCommsMutSlice) -> i32,
    >,
    pub scan:
        Option<extern "C" fn(*mut c_void, *const SctlCommsScanParams, SctlCommsMutSlice) -> i32>,
    pub usb_cycle: Option<extern "C" fn(*mut c_void, SctlCommsMutSlice) -> i32>,
}

impl Default for SctlCommsPluginV1 {
    fn default() -> Self {
        Self {
            abi_version: 0,
            plugin_ctx: core::ptr::null_mut(),
            destroy: None,
            probe: None,
            open: None,
            close: None,
            status: None,
            poll_location: None,
            disable_location: None,
            poll_link: None,
            set_bands: None,
            scan: None,
            usb_cycle: None,
        }
    }
}

pub type SctlCommsPluginInitV1 =
    unsafe extern "C" fn(*const SctlCommsHostV1, *mut SctlCommsPluginV1) -> i32;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_layout_is_c_shaped() {
        assert_eq!(SCTL_COMMS_ABI_VERSION, 1);
        assert!(core::mem::size_of::<SctlCommsPluginV1>() <= 160);
        assert!(core::mem::size_of::<SctlCommsHostV1>() <= 96);
        assert_eq!(
            core::mem::align_of::<SctlCommsOpenConfig>(),
            core::mem::align_of::<usize>()
        );
    }
}
