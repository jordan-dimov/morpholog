//! Clock abstraction so the polling worker's timing behavior is
//! testable without relying on wall-clock sleeps.
//!
//! Production uses [`RealClock`] which delegates to
//! `chrono::Utc::now` and `tokio::time::sleep`. Tests can use
//! [`crate::testing::MockClock`],
//! which records each `sleep_for` call into an inspectable buffer
//! and never actually sleeps. With this split, tests can assert
//! "the worker tried to sleep for the jittered interval" with
//! zero wall-clock time elapsed, and they remain deterministic
//! under CI load.

use chrono::{DateTime, Utc};
use std::future::Future;
use std::time::Duration;

/// Minimal clock trait the [`crate::OutboxWorker`] depends on.
///
/// Two methods: `now` returns the current wall-clock instant (used
/// to compute "is `next_attempt_at` in the past?" decisions in
/// the smart-sleep path), and `sleep_for` returns a future that
/// resolves after the given duration. Implementors decide whether
/// `sleep_for` blocks real time (production) or returns ready
/// immediately (tests).
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> DateTime<Utc>;
    fn sleep_for(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Production [`Clock`]: wall-clock `now` and real `tokio::time::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn sleep_for(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production clock's `now` is the wall clock and `sleep_for`
    /// actually elapses. Pinned because every other test injects
    /// `MockClock`, so nothing else ever constructs `RealClock` - the
    /// production impl must not be a coverage blind spot.
    #[tokio::test]
    async fn real_clock_tracks_wall_time_and_sleeps() {
        let clock = RealClock;
        let before = Utc::now();
        let now = clock.now();
        let after = Utc::now();
        assert!(before <= now && now <= after, "now() is the wall clock");

        let started = std::time::Instant::now();
        clock.sleep_for(Duration::from_millis(20)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(20),
            "sleep_for() elapses real time"
        );
    }
}
