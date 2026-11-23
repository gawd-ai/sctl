//! Safe-mode flag endpoints.
//!
//! The supervisor writes `<data_dir>/safe_mode.flag` when it detects a
//! crash-loop. While the flag exists, the next `sctl serve` startup skips
//! every optional subsystem (modem, GPS, LTE, watchdog, infra) and keeps only
//! HTTP+tunnel+sessions live. Once an operator has investigated and cleared
//! the underlying issue, they clear the flag via:
//!
//! ```text
//! DELETE /api/safe_mode/flag    # auth required
//! ```
//!
//! The endpoint is idempotent — deleting a non-existent flag returns OK. The
//! flag content (`since_unix`, `reason`, `consecutive_crashes`) is returned
//! by `GET /api/safe_mode/flag` for inspection.

use std::path::Path;

use axum::{extract::State, http::StatusCode, Json};
use serde_json::{json, Value};

use crate::AppState;

fn flag_path(state: &AppState) -> std::path::PathBuf {
    Path::new(&state.config.server.data_dir).join("safe_mode.flag")
}

/// The most of the flag that is read: the supervisor writes a few hundred
/// bytes.
const FLAG_READ_CAP: usize = 64 * 1024;

/// What the flag in `data_dir` says, for `/api/info` and `GET
/// /api/safe_mode/flag`: `{"active": false}` when nothing is at its name,
/// else `{"active": true}` with its content as `flag` when that is JSON in
/// a regular file. The flag is whatever is at the name, as the start reads
/// it; its content is read through the file gate's handle and its type
/// judgment (ADR-011), so a FIFO or a device there is never opened. A plain
/// read ran on a runtime worker on every `/api/info`, and a FIFO at the
/// name held each one for good.
pub fn block(data_dir: &Path) -> Value {
    let path = data_dir.join("safe_mode.flag");
    if std::fs::symlink_metadata(&path).is_err() {
        return json!({ "active": false });
    }
    let flag = crate::file_access::read_own(&path, FLAG_READ_CAP)
        .ok()
        .flatten()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
    match flag {
        Some(flag) => json!({ "active": true, "flag": flag }),
        None => json!({ "active": true }),
    }
}

/// `GET /api/safe_mode/flag` — return flag contents if present.
pub async fn get_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let mut body = block(Path::new(&state.config.server.data_dir));
    // A flag that is there and cannot be read as JSON is `null`, as before.
    if body["active"] == true && body.get("flag").is_none() {
        body["flag"] = Value::Null;
    }
    Ok(Json(body))
}

/// `DELETE /api/safe_mode/flag` — clear the flag.
///
/// The next supervisor restart will then bring optional subsystems back up.
/// This does NOT restart the daemon — the operator must do that explicitly
/// (e.g. via `systemctl restart sctl`) or rely on the supervisor's normal
/// backoff to recycle the child.
pub async fn clear_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    let path = flag_path(&state);
    if path.exists() {
        std::fs::remove_file(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(Json(json!({ "cleared": true })))
    } else {
        Ok(Json(
            json!({ "cleared": false, "reason": "flag not present" }),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::OpenOptionsExt;
    use std::time::Duration;

    use super::*;

    /// Read the flag's block on a thread of its own: a plain read would
    /// wait on a FIFO for good, on whatever thread ran it. On a timeout
    /// the FIFO gets a writer, opened without waiting, before the test
    /// fails.
    fn block_at_once(data_dir: &Path, fifo: Option<&Path>) -> Value {
        let (tx, rx) = std::sync::mpsc::channel();
        let dir = data_dir.to_path_buf();
        std::thread::spawn(move || {
            let _ = tx.send(block(&dir));
        });
        let Ok(answer) = rx.recv_timeout(Duration::from_secs(5)) else {
            if let Some(fifo) = fifo {
                let _ = std::fs::OpenOptions::new()
                    .write(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(fifo);
            }
            panic!("the flag's read waited");
        };
        answer
    }

    /// `/api/info` reads the flag on every poll. A FIFO at its name held
    /// each read for good on a runtime worker. It is never opened: the
    /// flag reads as present, with no content.
    #[test]
    fn the_flag_is_read_without_waiting_whatever_is_at_its_name() {
        let dir = std::env::temp_dir().join(format!("sctl-safe-mode-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join("safe_mode.flag");
        assert_eq!(block_at_once(&dir, None), json!({"active": false}));

        nix::unistd::mkfifo(&flag, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for _ in 0..8 {
            assert_eq!(block_at_once(&dir, Some(&flag)), json!({"active": true}));
        }

        // The supervisor's own flag reads as before.
        std::fs::remove_file(&flag).unwrap();
        std::fs::write(
            &flag,
            r#"{"reason": "child_exit_nonzero", "consecutive_crashes": 3}"#,
        )
        .unwrap();
        assert_eq!(
            block_at_once(&dir, None),
            json!({"active": true, "flag": {"reason": "child_exit_nonzero", "consecutive_crashes": 3}})
        );
        std::fs::write(&flag, "not json").unwrap();
        assert_eq!(block_at_once(&dir, None), json!({"active": true}));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
