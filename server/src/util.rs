//! Small helpers shared across modules.

use std::borrow::Cow;
use std::path::Path;

use tokio::io::AsyncWriteExt;

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
/// Uses `tokio::fs` end-to-end so the call is safe inside async polling
/// loops without blocking a worker thread on disk I/O.
pub async fn append_rotating(path: &Path, line: &str, max_bytes: u64) -> std::io::Result<bool> {
    let mut rotated = false;
    if let Ok(meta) = tokio::fs::metadata(path).await {
        if meta.len() > max_bytes {
            let rotated_path = match path.extension().and_then(|e| e.to_str()) {
                Some(ext) => path.with_extension(format!("{ext}.1")),
                None => path.with_extension("1"),
            };
            // best-effort rename; if it fails we still try to append below
            if tokio::fs::rename(path, &rotated_path).await.is_ok() {
                rotated = true;
            }
        }
    }
    let mut f = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    f.write_all(line.as_bytes()).await?;
    Ok(rotated)
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
}
