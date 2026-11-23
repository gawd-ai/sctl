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

use crate::error::{codes, ApiError};
use crate::AppState;

/// The most of the flag that is read: the supervisor writes a few hundred
/// bytes.
const FLAG_READ_CAP: usize = 64 * 1024;

/// What is at the flag's name, looked at without following a link.
enum AtName {
    Nothing,
    Something(std::fs::Metadata),
    /// The name cannot be looked at.
    Unseen(std::io::Error),
}

fn at_name(data_dir: &Path) -> AtName {
    match std::fs::symlink_metadata(data_dir.join("safe_mode.flag")) {
        Ok(meta) => AtName::Something(meta),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AtName::Nothing,
        Err(e) => AtName::Unseen(e),
    }
}

/// Whether the safe-mode flag is set. The one judgment, for boot, for
/// `/api/info` and the flag routes, and for the restart's answer: anything
/// at the flag's name is the flag, looked at without following a link. A
/// regular file, a link (to a file, to a device, to nothing), a directory,
/// a FIFO, and a name that cannot be looked at are all the flag; only
/// nothing at the name is no flag.
///
/// Three readers disagreed. For a link to nothing the route said active,
/// `DELETE` said "not present", and boot, which followed the link, ran
/// normal.
pub fn active(data_dir: &Path) -> bool {
    !matches!(at_name(data_dir), AtName::Nothing)
}

/// What the flag in `data_dir` says, for `/api/info` and `GET
/// /api/safe_mode/flag`. `active` is [`active`]'s answer:
///
/// - `{"active": false}` only when nothing is at the flag's name;
/// - `{"active": true, "flag": {...}}` when a regular file there holds
///   JSON;
/// - `{"active": true, "flag": null, "reason": "..."}` for anything else at
///   the name: a FIFO or a device, a directory, content that is not JSON, a
///   link to nothing, or a name that cannot be looked at. `reason` says
///   which.
///
/// A unit in safe mode whose flag cannot be read never reads as a unit out
/// of safe mode. The content is read through the file gate's handle and
/// its type judgment (ADR-011), so a FIFO or a device there is never
/// opened: a plain read ran on a runtime worker on every `/api/info`, and
/// a FIFO at the name held each one for good.
pub fn block(data_dir: &Path) -> Value {
    let path = data_dir.join("safe_mode.flag");
    let unread = |reason: String| json!({ "active": true, "flag": null, "reason": reason });
    match at_name(data_dir) {
        AtName::Something(_) => {}
        AtName::Nothing => return json!({ "active": false }),
        AtName::Unseen(e) => return unread(format!("the flag cannot be looked at: {e}")),
    }
    match crate::file_access::read_own(&path, FLAG_READ_CAP) {
        Ok(Some(bytes)) => match serde_json::from_slice::<Value>(&bytes) {
            Ok(flag) => json!({ "active": true, "flag": flag }),
            Err(_) => unread("the flag's content is not JSON".to_string()),
        },
        // Something is at the name and leads to no file: a link to
        // nothing, unless the flag was cleared between the two looks.
        Ok(None) if active(data_dir) => unread("the flag is a link to nothing".to_string()),
        Ok(None) => json!({ "active": false }),
        Err(refusal) => unread(format!("the flag cannot be read: {}", refusal.message())),
    }
}

/// `GET /api/safe_mode/flag` — return flag contents if present.
pub async fn get_flag(State(state): State<AppState>) -> Result<Json<Value>, StatusCode> {
    Ok(Json(block(Path::new(&state.config.server.data_dir))))
}

/// Why the flag was not cleared.
#[derive(Debug)]
pub enum NotCleared {
    /// A directory at the name that holds something: it is not removed.
    DirectoryNotEmpty(std::path::PathBuf),
    /// The name could not be looked at, or what is there not removed.
    Io(std::path::PathBuf, std::io::Error),
}

/// Remove what is at the flag's name, whatever [`active`] saw there:
/// `true` when something was removed, `false` when nothing was there.
///
/// A link is removed itself, never what it names. A directory is removed
/// when it is empty; one that holds something is refused, since nothing
/// under it is the agent's to delete.
pub fn clear(data_dir: &Path) -> Result<bool, NotCleared> {
    let path = data_dir.join("safe_mode.flag");
    let meta = match at_name(data_dir) {
        AtName::Nothing => return Ok(false),
        AtName::Unseen(e) => return Err(NotCleared::Io(path, e)),
        AtName::Something(meta) => meta,
    };
    // `symlink_metadata`: a link to a directory is a link, and is unlinked.
    let removed = if meta.is_dir() {
        std::fs::remove_dir(&path)
    } else {
        std::fs::remove_file(&path)
    };
    match removed {
        Ok(()) => Ok(true),
        // Cleared by someone else between the look and the removal.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e)
            if meta.is_dir()
                && matches!(e.raw_os_error(), Some(libc::ENOTEMPTY | libc::EEXIST)) =>
        {
            Err(NotCleared::DirectoryNotEmpty(path))
        }
        Err(e) => Err(NotCleared::Io(path, e)),
    }
}

/// `DELETE /api/safe_mode/flag` — clear the flag.
///
/// Removes what is at the flag's name, which is what boot and `GET` read
/// as the flag: a file, a FIFO, a link (the link itself, never its
/// target), an empty directory. A directory that holds something is
/// refused, `409 IS_DIRECTORY`, with a message naming it.
///
/// The next supervisor restart will then bring optional subsystems back up.
/// This does NOT restart the daemon — the operator must do that explicitly
/// (e.g. via `systemctl restart sctl`) or rely on the supervisor's normal
/// backoff to recycle the child.
pub async fn clear_flag(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<ApiError>)> {
    match clear(Path::new(&state.config.server.data_dir)) {
        Ok(true) => Ok(Json(json!({ "cleared": true }))),
        Ok(false) => Ok(Json(
            json!({ "cleared": false, "reason": "flag not present" }),
        )),
        Err(NotCleared::DirectoryNotEmpty(path)) => Err(ApiError::new(
            codes::IS_DIRECTORY,
            format!(
                "The safe-mode flag at {} is a directory that is not empty: it is not removed. \
                 The agent stays in safe mode at its next start until the directory is emptied \
                 or removed on the unit.",
                path.display()
            ),
        )
        .into_response_with(StatusCode::CONFLICT)),
        Err(NotCleared::Io(path, e)) => Err(ApiError::new(
            codes::IO_ERROR,
            format!(
                "The safe-mode flag at {} could not be removed: {e}",
                path.display()
            ),
        )
        .into_response_with(StatusCode::INTERNAL_SERVER_ERROR)),
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

    /// A scratch data directory and a state over it.
    fn bench(name: &str) -> (std::path::PathBuf, AppState) {
        let dir =
            std::env::temp_dir().join(format!("sctl-safe-mode-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("data")).unwrap();
        let mut config: crate::config::Config = toml::from_str("").unwrap();
        config.server.data_dir = dir.join("data").display().to_string();
        let gate = std::sync::Arc::new(crate::file_access::Gate::for_agent(&config, None, None));
        (dir, AppState::for_tests(config, gate))
    }

    /// What `DELETE` answered: its body when it cleared or found nothing,
    /// `None` when it refused.
    async fn delete(state: &AppState) -> Option<Value> {
        clear_flag(State(state.clone())).await.ok().map(|Json(v)| v)
    }

    /// Boot, the routes and the restart's answer read one function, and
    /// `DELETE` removes what that function saw. They disagreed: for a link
    /// to nothing the route said active, `DELETE` said "not present" and
    /// boot ran normal; a directory was active for ever, since `DELETE`
    /// could not remove it.
    #[tokio::test]
    async fn whatever_is_at_the_name_is_the_flag_and_delete_clears_it() {
        type Place = Box<dyn Fn(&Path)>;
        let (dir, state) = bench("clear");
        let data = dir.join("data");
        let flag = data.join("safe_mode.flag");
        let target = dir.join("target");
        std::fs::write(&target, "kept").unwrap();
        let nothing = json!({ "cleared": false, "reason": "flag not present" });

        assert!(!active(&data));
        assert_eq!(delete(&state).await, Some(nothing.clone()));

        let nowhere = dir.join("nowhere");
        let kept = target.clone();
        let cases: Vec<(&str, Place)> = vec![
            (
                "a link to nothing",
                Box::new(move |f| std::os::unix::fs::symlink(&nowhere, f).unwrap()),
            ),
            (
                "a link to a file",
                Box::new(move |f| std::os::unix::fs::symlink(&kept, f).unwrap()),
            ),
            (
                "an empty directory",
                Box::new(|f| std::fs::create_dir(f).unwrap()),
            ),
            (
                "a FIFO",
                Box::new(|f| nix::unistd::mkfifo(f, nix::sys::stat::Mode::S_IRWXU).unwrap()),
            ),
            (
                "the supervisor's file",
                Box::new(|f| std::fs::write(f, r#"{"reason": "x"}"#).unwrap()),
            ),
        ];
        for (what, place) in cases {
            place(&flag);
            assert!(active(&data), "{what} is the flag");
            assert_eq!(block(&data)["active"], true, "{what}: the route agrees");
            assert_eq!(
                delete(&state).await,
                Some(json!({ "cleared": true })),
                "{what} is cleared"
            );
            assert!(
                std::fs::symlink_metadata(&flag).is_err(),
                "{what}: nothing is left at the name"
            );
            assert!(!active(&data), "{what}: cleared");
            assert_eq!(block(&data), json!({ "active": false }));
            assert_eq!(delete(&state).await, Some(nothing.clone()));
        }
        // The link was removed, never what it named.
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A directory that holds something is not removed: the refusal names
    /// it, and the flag stays set.
    #[tokio::test]
    async fn a_directory_that_is_not_empty_is_refused_and_stays_the_flag() {
        let (dir, state) = bench("full");
        let data = dir.join("data");
        let flag = data.join("safe_mode.flag");
        std::fs::create_dir(&flag).unwrap();
        std::fs::write(flag.join("inside"), "kept").unwrap();
        assert!(active(&data));
        assert_eq!(delete(&state).await, None, "refused");
        let (status, Json(refusal)) = clear_flag(State(state.clone())).await.unwrap_err();
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(refusal.code, codes::IS_DIRECTORY);
        assert!(
            refusal.message.contains(&format!(
                "{} is a directory that is not empty",
                flag.display()
            )),
            "{}",
            refusal.message
        );
        assert!(active(&data), "still the flag");
        assert_eq!(
            std::fs::read_to_string(flag.join("inside")).unwrap(),
            "kept"
        );
        // Emptied, it is cleared.
        std::fs::remove_file(flag.join("inside")).unwrap();
        assert_eq!(delete(&state).await, Some(json!({ "cleared": true })));
        assert!(!active(&data));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
