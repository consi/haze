//! Pluggable wall-clock source.
//!
//! Production code reads "now" through a `Clock` so tests can pin time and
//! step the storage lifecycle through years of simulated activity without
//! waiting. The real implementation just delegates to `chrono::Utc::now()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

/// Source of the current wall-clock second count.
pub trait Clock: Send + Sync {
    fn now_secs(&self) -> i64;
}

/// System wall clock backed by `chrono::Utc::now()`. Use in production.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// Test clock that returns a value set explicitly via `set_now`. Threadsafe -
/// the lifecycle test advances it from a background task while readers run on
/// the main thread.
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(initial_secs: i64) -> Self {
        Self(AtomicI64::new(initial_secs))
    }

    pub fn set_now(&self, secs: i64) {
        self.0.store(secs, Ordering::SeqCst);
    }

    pub fn advance(&self, delta_secs: i64) {
        self.0.fetch_add(delta_secs, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_secs(&self) -> i64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// Convenient default for production wiring.
pub fn system_clock() -> Arc<dyn Clock> {
    Arc::new(SystemClock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_round_trip() {
        let c = ManualClock::new(0);
        assert_eq!(c.now_secs(), 0);
        c.set_now(1_000);
        assert_eq!(c.now_secs(), 1_000);
        c.advance(50);
        assert_eq!(c.now_secs(), 1_050);
    }

    #[test]
    fn system_clock_returns_recent_value() {
        let c = SystemClock;
        let now = c.now_secs();
        // 2026-01-01T00:00:00Z = 1_767_225_600. Anything past that means the
        // call is wired up to the real clock, not e.g. stuck at zero.
        assert!(now > 1_767_225_600);
    }
}
