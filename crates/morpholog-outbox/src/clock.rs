//! Clock abstraction, so the worker's timing is testable without real sleeps.
//!
//! Production uses [`RealClock`]. Tests use [`crate::testing::MockClock`], which records each
//! `sleep_for` call and never sleeps, so timing assertions stay fast and deterministic.

use jiff::Timestamp;
use std::future::Future;
use std::time::Duration;

/// The clock [`crate::OutboxWorker`] depends on.
///
/// `now` is the current instant, used to decide how soon the next scheduled retry is due.
/// `sleep_for` resolves after the given duration, in real time or (in tests) immediately.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
    fn sleep_for(&self, duration: Duration) -> impl Future<Output = ()> + Send;
}

/// Production [`Clock`]: wall-clock `now` and real `tokio::time::sleep`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealClock;

impl Clock for RealClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
    fn sleep_for(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        tokio::time::sleep(duration)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production clock's `now` is the wall clock and `sleep_for` really elapses. Every
    /// other test injects `MockClock`, so nothing else exercises `RealClock`.
    #[tokio::test]
    async fn real_clock_tracks_wall_time_and_sleeps() {
        let clock = RealClock;
        let before = Timestamp::now();
        let now = clock.now();
        let after = Timestamp::now();
        assert!(before <= now && now <= after, "now() is the wall clock");

        let started = std::time::Instant::now();
        clock.sleep_for(Duration::from_millis(20)).await;
        assert!(
            started.elapsed() >= Duration::from_millis(20),
            "sleep_for() elapses real time"
        );
    }
}
