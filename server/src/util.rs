//! Small helpers shared across modules.

use std::borrow::Cow;
use std::path::Path;

/// Expand a leading `~` to `$HOME`.
///
/// - `"~"` → `"/home/user"`
/// - `"~/foo"` → `"/home/user/foo"`
/// - Anything else passes through unchanged.
pub fn expand_tilde(path: &str) -> Cow<'_, str> {
    if path == "~" || path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            if path == "~" {
                return Cow::Owned(home);
            }
            return Cow::Owned(format!("{}{}", home, &path[1..]));
        }
    }
    Cow::Borrowed(path)
}

/// Append a line to a file, rotating to `<path>.1` when the file exceeds
/// `max_bytes`. Best-effort — errors are returned to the caller so they can
/// log; we never panic. Returns `Ok(true)` if a rotation happened on this
/// call, `Ok(false)` otherwise.
///
/// The rotation suffix preserves the original extension: `foo.jsonl` rotates
/// to `foo.jsonl.1`; `bar.log` rotates to `bar.log.1`; an extensionless file
/// rotates to `file.1`. Matches the prior hand-rolled patterns in
/// `watchdog_history.jsonl` and `modem-state.log`.
///
/// Runs the rotate-then-append on the blocking pool via `spawn_blocking` so
/// the call is safe inside async polling loops without blocking a worker
/// thread on disk I/O. We use `std::fs` inside the closure rather than
/// `tokio::fs` because tokio's async file Drop schedules the close on the
/// blocking pool — readers racing the close occasionally see an empty file
/// on slower filesystems (caught by CI). `std::fs` with an explicit
/// `flush` + drop is synchronous and avoids the race.
///
/// The file is opened through the file gate's handle and its type
/// judgment (ADR-011, `file_access::append_own`): the file that was judged
/// is the file that is opened, and a new one is made exclusive. An open for
/// appending waits on a FIFO with no reader and runs a device's open; each
/// append then held one thread of the blocking pool for good. A FIFO, a
/// socket or a device at the name, or one put there after the judgment, is
/// one error to the caller at most, and no thread.
pub async fn append_rotating(path: &Path, line: &str, max_bytes: u64) -> std::io::Result<bool> {
    let path = path.to_path_buf();
    let line = line.to_string();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut rotated = false;
        if let Ok(meta) = std::fs::metadata(&path) {
            if meta.len() > max_bytes {
                let rotated_path = match path.extension().and_then(|e| e.to_str()) {
                    Some(ext) => path.with_extension(format!("{ext}.1")),
                    None => path.with_extension("1"),
                };
                // best-effort rename; if it fails we still try to append below
                if std::fs::rename(&path, &rotated_path).is_ok() {
                    rotated = true;
                }
            }
        }
        let mut f = crate::file_access::append_own(&path).map_err(|refusal| match refusal {
            crate::file_access::Refusal::Io(e) => e,
            refusal => std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("{}: {}", path.display(), refusal.message()),
            ),
        })?;
        f.write_all(line.as_bytes())?;
        f.flush()?;
        Ok(rotated)
    })
    .await
    .map_err(std::io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rotates_when_over_max_bytes() {
        let tmp = tempfile_path("append_rotating_rotate");
        let rotated_path = tmp.with_extension("log.1");
        let _ = tokio::fs::remove_file(&tmp).await;
        let _ = tokio::fs::remove_file(&rotated_path).await;
        // Seed file over the threshold
        tokio::fs::write(&tmp, vec![b'x'; 100]).await.unwrap();
        let rotated = append_rotating(&tmp, "new\n", 50).await.unwrap();
        assert!(rotated);
        assert!(rotated_path.exists());
        let after = tokio::fs::read_to_string(&tmp).await.unwrap();
        assert_eq!(after, "new\n");
    }

    #[tokio::test]
    async fn appends_when_under_max_bytes() {
        let tmp = tempfile_path("append_rotating_append");
        let _ = tokio::fs::remove_file(&tmp).await;
        tokio::fs::write(&tmp, b"existing\n").await.unwrap();
        let rotated = append_rotating(&tmp, "added\n", 1_000_000).await.unwrap();
        assert!(!rotated);
        let after = tokio::fs::read_to_string(&tmp).await.unwrap();
        assert_eq!(after, "existing\nadded\n");
    }

    #[tokio::test]
    async fn creates_when_missing() {
        let tmp = tempfile_path("append_rotating_create");
        let _ = tokio::fs::remove_file(&tmp).await;
        let rotated = append_rotating(&tmp, "first\n", 1_000_000).await.unwrap();
        assert!(!rotated);
        assert!(tmp.exists());
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("sctl_test_{name}_{}.log", std::process::id()))
    }

    /// A FIFO at the log's name held each append in its open, one thread
    /// of the blocking pool per line. It is judged first and never opened:
    /// one error, at once, and the FIFO left as it was.
    #[tokio::test]
    async fn a_fifo_at_the_name_is_one_refusal_and_no_thread() {
        use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
        let dir = std::env::temp_dir().join(format!("sctl-append-fifo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = dir.join("watchdog_history.jsonl");
        nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::S_IRWXU).unwrap();
        for _ in 0..8 {
            // An append that waited would hang the run: on a timeout the
            // FIFO gets a reader, opened without waiting, before the test
            // fails.
            let appended = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                append_rotating(&fifo, "{}\n", 1024),
            )
            .await;
            let Ok(appended) = appended else {
                let _ = std::fs::OpenOptions::new()
                    .read(true)
                    .custom_flags(libc::O_NONBLOCK)
                    .open(&fifo);
                panic!("the append waited on a FIFO");
            };
            let error = appended.expect_err("a FIFO is not appended to");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
            assert!(error.to_string().contains("a FIFO"), "{error}");
        }
        assert!(std::fs::symlink_metadata(&fifo)
            .unwrap()
            .file_type()
            .is_fifo());
        // A device is refused the same way; a file that is not there yet is
        // made, and one that is there is appended to.
        assert!(append_rotating(Path::new("/dev/null"), "x\n", 1024)
            .await
            .is_err());
        let log = dir.join("modem-state.log");
        assert!(!append_rotating(&log, "one\n", 1024).await.unwrap());
        assert!(!append_rotating(&log, "two\n", 1024).await.unwrap());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "one\ntwo\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
