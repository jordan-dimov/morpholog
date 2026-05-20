//! Clock abstraction so the polling worker's timing behavior is
//! testable without relying on wall-clock sleeps.
//!
//! Production uses [`RealClock`] which delegates to
//! `chrono::Utc::now` and `tokio::time::sleep`. Tests can use
//! [`MockClock`] (re-exported from the [`crate::testing`] module),
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
