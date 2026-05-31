//! Atomic compatibility helpers for embedded targets without 64-bit atomics.

#[cfg(target_has_atomic = "64")]
pub use std::sync::atomic::AtomicU64;

#[cfg(not(target_has_atomic = "64"))]
mod fallback {
    use std::sync::atomic::Ordering;
    use std::sync::{Mutex, MutexGuard};

    /// Mutex-backed `u64` with the small API surface sctl needs from `AtomicU64`.
    pub struct AtomicU64(Mutex<u64>);

    impl AtomicU64 {
        #[must_use]
        pub const fn new(value: u64) -> Self {
            Self(Mutex::new(value))
        }

        #[must_use]
        pub fn load(&self, _ordering: Ordering) -> u64 {
            *self.guard()
        }

        pub fn store(&self, value: u64, _ordering: Ordering) {
            *self.guard() = value;
        }

        pub fn fetch_add(&self, value: u64, _ordering: Ordering) -> u64 {
            let mut guard = self.guard();
            let previous = *guard;
            *guard = previous.wrapping_add(value);
            previous
        }

        fn guard(&self) -> MutexGuard<'_, u64> {
            self.0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }
    }
}

#[cfg(not(target_has_atomic = "64"))]
pub use fallback::AtomicU64;
