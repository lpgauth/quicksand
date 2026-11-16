//! When the running eval has to stop: once its deadline passes, or for good
//! once the runtime is stopped.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const NO_DEADLINE: u64 = u64::MAX;

/// Shared by the worker, its QuickJS interrupt handler, and the NIF calls
/// that stop the runtime.
pub struct Interrupt {
    stopping: AtomicBool,
    epoch: Instant,
    // Nanoseconds after `epoch`, so the interrupt handler can read it without
    // taking a lock.
    deadline: AtomicU64,
}

impl Default for Interrupt {
    fn default() -> Self {
        Self {
            stopping: AtomicBool::new(false),
            epoch: Instant::now(),
            deadline: AtomicU64::new(NO_DEADLINE),
        }
    }
}

impl Interrupt {
    pub fn stop(&self) {
        self.stopping.store(true, Ordering::Relaxed);
    }

    #[must_use]
    pub fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Relaxed)
    }

    pub fn set_deadline(&self, deadline: Option<Instant>) {
        let nanos = deadline.map_or(NO_DEADLINE, |deadline| {
            let since_epoch = deadline.saturating_duration_since(self.epoch).as_nanos();
            u64::try_from(since_epoch).unwrap_or(NO_DEADLINE)
        });
        self.deadline.store(nanos, Ordering::Relaxed);
    }

    pub fn clear_deadline(&self) {
        self.set_deadline(None);
    }

    #[must_use]
    pub fn deadline_passed(&self) -> bool {
        let deadline = self.deadline.load(Ordering::Relaxed);
        deadline != NO_DEADLINE && self.elapsed_nanos() >= deadline
    }

    /// Whether the running eval has to stop now.
    #[must_use]
    pub fn is_due(&self) -> bool {
        self.is_stopping() || self.deadline_passed()
    }

    /// Time left until the eval has to stop.
    #[must_use]
    pub fn remaining(&self) -> Duration {
        if self.is_stopping() {
            return Duration::ZERO;
        }
        match self.deadline.load(Ordering::Relaxed) {
            NO_DEADLINE => Duration::MAX,
            deadline => Duration::from_nanos(deadline.saturating_sub(self.elapsed_nanos())),
        }
    }

    fn elapsed_nanos(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
    }
}
