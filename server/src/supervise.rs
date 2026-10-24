//! Background loops that survive their own panics.
//!
//! A panic in a supervised loop is logged and the loop is rebuilt after a
//! delay. Under `panic=abort` such a panic took the whole agent down and the
//! external supervisor restarted everything; under unwind, the release
//! profile's choice, the task would otherwise die silently and for good.
//!
//! The binary's loops and the library's own use the same two helpers: the
//! Infra monitor, at boot and for a push, runs under [`spawn_supervised`]
//! (ADR-010 decision 6).

use std::panic::AssertUnwindSafe;
use std::time::Duration;

use futures_util::FutureExt as _;
use tokio::task::JoinHandle;

/// How long a panicked background loop waits before it is rebuilt.
pub const RESTART_DELAY: Duration = Duration::from_secs(5);

/// Spawn a background loop that survives panics: a panic is logged and the
/// loop is rebuilt by `mk` after `restart_delay`. A loop that returns
/// normally (shutdown) is not rebuilt, and aborting the returned handle
/// remains the way to stop it.
pub fn spawn_supervised<F, Fut>(
    name: &'static str,
    restart_delay: Duration,
    mut mk: F,
) -> JoinHandle<()>
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            if AssertUnwindSafe(mk()).catch_unwind().await.is_ok() {
                // Normal completion (shutdown): do not rebuild.
                break;
            }
            tracing::error!(
                task = name,
                "background task panicked; restarting in {restart_delay:?}"
            );
            tokio::time::sleep(restart_delay).await;
        }
    })
}

/// Bridge a loop that spawns itself (the comms poller) into
/// [`spawn_supervised`]: a panic in the inner task is raised again here so
/// the supervisor rebuilds it, and dropping this future (the supervisor
/// handle's `.abort()` on shutdown or config change) aborts the inner task,
/// so the handle behaves as a loop spawned directly.
pub async fn supervise_spawned(inner: JoinHandle<()>) {
    struct AbortOnDrop(JoinHandle<()>);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let mut inner = AbortOnDrop(inner);
    if let Err(e) = (&mut inner.0).await {
        if e.is_panic() {
            std::panic::resume_unwind(e.into_panic());
        }
        // Cancelled from outside: treat as shutdown.
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    #[tokio::test]
    async fn supervised_loop_restarts_after_panics_then_stops_on_completion() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let handle = super::spawn_supervised(
            "test_task",
            std::time::Duration::from_millis(10),
            move || {
                let counter = counter.clone();
                async move {
                    // Panics on the first two calls, completes on the third.
                    assert!(
                        counter.fetch_add(1, Ordering::SeqCst) >= 2,
                        "induced test panic"
                    );
                }
            },
        );
        handle.await.expect("supervisor task must not panic");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn a_panic_in_a_spawned_loop_reaches_the_supervisor() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let handle = super::spawn_supervised(
            "test_spawned",
            std::time::Duration::from_millis(10),
            move || {
                let counter = counter.clone();
                super::supervise_spawned(tokio::spawn(async move {
                    assert!(
                        counter.fetch_add(1, Ordering::SeqCst) >= 1,
                        "induced test panic"
                    );
                }))
            },
        );
        handle.await.expect("supervisor task must not panic");
        assert_eq!(calls.load(Ordering::SeqCst), 2, "rebuilt once, then done");
    }
}
