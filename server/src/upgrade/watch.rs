//! What the running agent does about the state file it did not write: at
//! start it reconciles what the previous process (or the helper) left, and
//! while the helper works it watches the file (inotify, no polling) and
//! publishes what the helper writes.

use std::path::Path;
use std::sync::Arc;

use tracing::{info, warn};

use super::apply::lock_is_free;
use super::state::{Handle, Outcome, Phase};
use super::Reason;

/// nix's Inotify offers `AsFd`; tokio's AsyncFd wants `AsRawFd`.
struct Watched(nix::sys::inotify::Inotify);

impl std::os::unix::io::AsRawFd for Watched {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsFd;
        self.0.as_fd().as_raw_fd()
    }
}

/// At start: settle a state the previous process left in flight.
///
/// - `applying` with the helper's lock free: the helper is gone without
///   writing an outcome. The running version says how it ended: the target
///   version running is `ok`, the old version running is `rolled_back`.
/// - `applying` with the lock held: the helper is at it; the caller watches.
/// - `staging`: the agent restarted mid-stage; nothing moved.
///
/// Returns whether the helper is still working.
pub fn reconcile(handle: &Handle, stage_dir: &Path, running_version: &str) -> bool {
    let state = handle.current();
    match state.phase {
        Phase::Applying => {
            if !lock_is_free(stage_dir) {
                info!("upgrade: the helper is still at it; watching its outcome");
                return true;
            }
            let running = super::version::Version::parse(running_version);
            let is = |v: &Option<String>| {
                v.as_deref().and_then(super::version::Version::parse) == running
            };
            let settled = if is(&state.to_version) {
                info!("upgrade: the helper did not report, and the new version is running: ok");
                let mut s = state.clone();
                s.detail =
                    Some("the helper did not report; the new version is running".to_string());
                s.finished(Outcome::Ok)
            } else if is(&state.from_version) {
                warn!("upgrade: the helper did not report, and the old version is running: rolled back");
                let mut s = state.clone();
                s.detail =
                    Some("the helper did not report; the previous version is running".to_string());
                s.finished(Outcome::RolledBack)
            } else {
                state.clone().not_applied(
                    Reason::HelperLost,
                    format!("the helper did not report; {running_version} is running"),
                )
            };
            handle.set(settled);
            false
        }
        Phase::Staging => {
            handle.set(state.not_applied(Reason::HelperLost, "the agent restarted while staging"));
            false
        }
        Phase::Idle | Phase::Done => false,
    }
}

/// Watch the state file's directory until the state is `done`, publishing
/// every write. Returns when the outcome is in, or when inotify is not
/// available (the state is then reported when the agent next starts).
pub async fn watch_until_done(handle: Arc<Handle>) {
    use nix::sys::inotify::{AddWatchFlags, InitFlags, Inotify};
    let Some(dir) = handle.path().parent().map(Path::to_path_buf) else {
        return;
    };
    let inotify = match Inotify::init(InitFlags::IN_NONBLOCK | InitFlags::IN_CLOEXEC) {
        Ok(i) => i,
        Err(e) => {
            warn!("upgrade: inotify unavailable ({e}); the outcome is reported at the next start");
            return;
        }
    };
    if let Err(e) = inotify.add_watch(
        &dir,
        AddWatchFlags::IN_CLOSE_WRITE | AddWatchFlags::IN_MOVED_TO,
    ) {
        warn!("upgrade: cannot watch {}: {e}", dir.display());
        return;
    }
    let fd = match tokio::io::unix::AsyncFd::new(Watched(inotify)) {
        Ok(fd) => fd,
        Err(e) => {
            warn!("upgrade: cannot watch {}: {e}", dir.display());
            return;
        }
    };
    // The helper may have finished between reconcile and the watch.
    if handle.reload().is_some_and(|s| s.phase == Phase::Done) {
        return;
    }
    loop {
        let mut guard = match fd.readable().await {
            Ok(g) => g,
            Err(e) => {
                warn!("upgrade: watch ended: {e}");
                return;
            }
        };
        match guard.try_io(|inner| {
            inner
                .get_ref()
                .0
                .read_events()
                .map_err(std::io::Error::from)
        }) {
            Ok(Ok(events)) => {
                if events
                    .iter()
                    .any(|e| e.name.as_deref().is_some_and(|n| n == "state.json"))
                {
                    if let Some(state) = handle.reload() {
                        info!(phase = ?state.phase, outcome = ?state.outcome, "upgrade: the helper wrote its state");
                        if state.phase == Phase::Done {
                            return;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                warn!("upgrade: watch read: {e}");
                return;
            }
            Err(_would_block) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upgrade::state::{self, UpgradeState};

    fn bench(tag: &str) -> (std::path::PathBuf, Arc<Handle>) {
        let dir = std::env::temp_dir().join(format!("sctl-watch-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("upgrade").join("stage")).unwrap();
        let handle = Arc::new(Handle::open(dir.to_str().unwrap()));
        (dir, handle)
    }

    #[test]
    fn a_lost_helper_is_settled_by_the_running_version() {
        let (dir, handle) = bench("lost");
        let stage = dir.join("upgrade").join("stage");
        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").with_phase(Phase::Applying));
        assert!(!reconcile(&handle, &stage, "0.6.8.2"));
        assert_eq!(handle.current().outcome, Some(Outcome::Ok));

        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").with_phase(Phase::Applying));
        assert!(!reconcile(&handle, &stage, "0.6.7.1"));
        assert_eq!(handle.current().outcome, Some(Outcome::RolledBack));

        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").with_phase(Phase::Applying));
        assert!(!reconcile(&handle, &stage, "0.5.0.0"));
        assert_eq!(handle.current().reason, Some(Reason::HelperLost));

        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2"));
        assert!(!reconcile(&handle, &stage, "0.6.7.1"));
        assert_eq!(handle.current().reason, Some(Reason::HelperLost));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_working_helper_is_left_alone() {
        let (dir, handle) = bench("alive");
        let stage = dir.join("upgrade").join("stage");
        let _lock = crate::upgrade::apply::take_lock(&stage).unwrap();
        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").with_phase(Phase::Applying));
        assert!(reconcile(&handle, &stage, "0.6.8.2"));
        assert_eq!(handle.current().phase, Phase::Applying);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_watcher_publishes_what_another_process_writes() {
        let (dir, handle) = bench("inotify");
        handle.set(UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").with_phase(Phase::Applying));
        let path = handle.path().to_path_buf();
        let watcher = tokio::spawn(watch_until_done(handle.clone()));
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let done = UpgradeState::staging(None, "0.6.7.1", "0.6.8.2").finished(Outcome::Ok);
        state::save(&path, &done).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), watcher)
            .await
            .expect("the watcher returns on done")
            .unwrap();
        assert_eq!(handle.current().outcome, Some(Outcome::Ok));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
