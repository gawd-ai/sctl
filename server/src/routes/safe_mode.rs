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
/// /api/safe_mode/flag`:
///
/// - `{"active": false}` only when nothing is at the flag's name;
/// - `{"active": true, "flag": {...}}` when a regular file there holds
///   JSON;
/// - `{"active": true, "flag": null, "reason": "..."}` for anything else at
///   the name: a FIFO or a device, content that is not JSON, a link to
///   nothing, or a name that cannot be looked at. `reason` says which.
///
/// A unit in safe mode whose flag cannot be read never reads as a unit out
/// of safe mode. The content is read through the file gate's handle and
/// its type judgment (ADR-011), so a FIFO or a device there is never
/// opened: a plain read ran on a runtime worker on every `/api/info`, and
/// a FIFO at the name held each one for good.
pub fn block(data_dir: &Path) -> Value {
    let path = data_dir.join("safe_mode.flag");
    let unread = |reason: String| json!({ "active": true, "flag": null, "reason": reason });
    match std::fs::symlink_metadata(&path) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return json!({ "active": false }),
        Err(e) => return unread(format!("the flag cannot be looked at: {e}")),
    }
    match crate::file_access::read_own(&path, FLAG_READ_CAP) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(flag) => json!({ "active": true, "flag": flag }),
            Err(_) => unread("the flag's content is not JSON".to_string()),
        },
        // Something is at the name and leads to no file: a link to
        // nothing, unless the flag was cleared between the two looks.
        Ok(None) => match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => json!({ "active": false }),
            _ => unread("the flag is a link to nothing".to_string()),
        },
        Err(refusal) => unread(format!("the flag cannot be read: {}", refusal.message())),
    }
}

/// Whether the flag is active, as [`block`] reads it.
pub fn active(data_dir: &Path) -> bool {
    block(data_dir)["active"] == true
}

/// `GET /api/safe_mode/flag` — return flag contents if present.
pub async fn get_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    Ok(Json(block(Path::new(&state.config.server.data_dir))))
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
    /// each read for good on a runtime worker. It is never opened, and it
    /// never reads as "no flag": a unit in safe mode whose flag cannot be
    /// read must not look like a unit out of safe mode.
    #[test]
    fn a_flag_that_cannot_be_read_is_active_and_says_what_is_there() {
        let dir = std::env::temp_dir().join(format!("sctl-safe-mode-flag-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let flag = dir.join("safe_mode.flag");
        // Nothing at the name: the one case that is not active.
        assert_eq!(block_at_once(&dir, None), json!({"active": false}));
        assert!(!active(&dir));

        // A FIFO: never opened, active, and said.
        nix::unistd::mkfifo(&flag, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for _ in 0..8 {
            assert_eq!(
                block_at_once(&dir, Some(&flag)),
                json!({"active": true, "flag": null,
                       "reason": "the flag cannot be read: Not a regular file: a FIFO"})
            );
        }
        assert!(active(&dir));
        std::fs::remove_file(&flag).unwrap();

        // A directory, a link to nothing, a link to a device.
        std::fs::create_dir(&flag).unwrap();
        let answer = block_at_once(&dir, None);
        assert_eq!(answer["active"], true);
        assert_eq!(answer["flag"], Value::Null);
        assert_eq!(
            answer["reason"],
            "the flag cannot be read: Path is a directory"
        );
        std::fs::remove_dir(&flag).unwrap();
        std::os::unix::fs::symlink(dir.join("nowhere"), &flag).unwrap();
        assert_eq!(
            block_at_once(&dir, None),
            json!({"active": true, "flag": null, "reason": "the flag is a link to nothing"})
        );
        std::fs::remove_file(&flag).unwrap();
        std::os::unix::fs::symlink("/dev/zero", &flag).unwrap();
        let answer = block_at_once(&dir, None);
        assert_eq!(answer["active"], true);
        assert!(answer["reason"]
            .as_str()
            .unwrap()
            .contains("a character device"));
        std::fs::remove_file(&flag).unwrap();

        // The supervisor's own flag reads as before, with no reason.
        std::fs::write(
            &flag,
            r#"{"reason": "child_exit_nonzero", "consecutive_crashes": 3}"#,
        )
        .unwrap();
        assert_eq!(
            block_at_once(&dir, None),
            json!({"active": true, "flag": {"reason": "child_exit_nonzero", "consecutive_crashes": 3}})
        );
        // Content that is not JSON: active, and said.
        std::fs::write(&flag, "not json").unwrap();
        assert_eq!(
            block_at_once(&dir, None),
            json!({"active": true, "flag": null, "reason": "the flag's content is not JSON"})
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A name that cannot even be looked at is not "no flag".
    #[test]
    fn a_flag_that_cannot_be_looked_at_is_active() {
        // A data directory that is a file: looking at a name under it fails
        // with something other than "not found".
        let dir = std::env::temp_dir().join(format!("sctl-safe-mode-nodir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&dir);
        std::fs::write(&dir, b"a file where the data directory should be").unwrap();
        let answer = block(&dir);
        assert_eq!(answer["active"], true, "{answer}");
        assert_eq!(answer["flag"], Value::Null);
        assert!(
            answer["reason"]
                .as_str()
                .unwrap()
                .starts_with("the flag cannot be looked at:"),
            "{answer}"
        );
        let _ = std::fs::remove_file(&dir);
    }
}
