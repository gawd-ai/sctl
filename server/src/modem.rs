//! Shared AT command helper for Quectel modem communication.
//!
//! Both GPS and LTE modules use AT commands over the same serial device.
//! This module provides the common `at_command()` function that works on
//! BusyBox ash (no `stty`, `timeout`, or fractional `sleep`).
//!
//! A per-device mutex prevents concurrent AT commands from interleaving
//! on the serial port, which causes garbled responses.

use std::collections::HashMap;
use std::sync::LazyLock;

use tokio::sync::Mutex;

/// Global per-device lock. Keyed by device path (e.g. `/dev/ttyUSB2`).
static DEVICE_LOCKS: LazyLock<Mutex<HashMap<String, &'static Mutex<()>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Get or create the lock for a given device path.
async fn device_lock(device: &str) -> &'static Mutex<()> {
    let mut map = DEVICE_LOCKS.lock().await;
    if let Some(lock) = map.get(device) {
        return *lock;
    }
    let lock = Box::leak(Box::new(Mutex::new(())));
    map.insert(device.to_string(), lock);
    lock
}

/// Send an AT command to the modem and read the response.
///
/// Opens the serial device read/write via fd 3, writes the command, then
/// reads the response using a background `cat` with `kill` timeout.
/// This approach works on BusyBox ash (no `stty`, `timeout`, or fractional `sleep`).
///
/// Serialized per device path — concurrent callers targeting the same device
/// will queue behind the lock to prevent interleaved AT responses.
pub async fn at_command(shell: &str, device: &str, command: &str) -> Result<String, String> {
    let lock = device_lock(device).await;
    let _guard = lock.lock().await;

    let cmd = format!(
        "exec 3<>{device}; \
         printf '{command}\\r' >&3; \
         sleep 1; \
         cat <&3 & pid=$!; \
         sleep 1; \
         kill $pid 2>/dev/null; \
         wait $pid 2>/dev/null; \
         exec 3>&-"
    );
    match crate::shell::process::exec_command(shell, "/", &cmd, 5000, None).await {
        Ok(result) => Ok(result.stdout),
        Err(e) => Err(format!("AT command exec error: {e}")),
    }
}
