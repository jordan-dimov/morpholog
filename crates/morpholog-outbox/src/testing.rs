//! Test-support implementations of the [`crate::clock::Clock`] and
//! [`crate::jitter::JitterRng`] traits.
//!
//! These types are always compiled (no feature flag) so integration
//! tests in `tests/*.rs` can use them without configuring a feature.
//! Production code should not import them.

#![allow(clippy::expect_used)]

use chrono::{DateTime, Utc};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::Clock;
use crate::jitter::JitterRng;

/// [`Clock`] that never sleeps and records every `sleep_for` call.
/// `now()` returns the supplied fixed instant unless explicitly
/// advanced.
///
/// Internally Arc-shared so a test can clone the handle, pass one
/// copy to the worker (which takes the clock by value), and keep
/// another copy for inspection. All clones observe the same
/// recorded sleeps and the same `now`.
#[derive(Clone)]
pub struct MockClock {
    state: Arc<MockClockState>,
}

struct MockClockState {
    fixed_now: Mutex<DateTime<Utc>>,
    sleeps: Mutex<Vec<Duration>>,
}

impl MockClock {
    pub fn new(starting_now: DateTime<Utc>) -> Self {
        Self {
            state: Arc::new(MockClockState {
                fixed_now: Mutex::new(starting_now),
                sleeps: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Snapshot of every `sleep_for` call observed so far, in
    /// order. Cloned out so callers do not hold the lock.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state
            .sleeps
            .lock()
            .expect("MockClock sleeps poisoned")
            .clone()
    }

    /// Move the mock's `now` forward by `delta`. Sleep futures
    /// produced by `sleep_for` are independent of `now` (they
    /// always resolve immediately); this method exists for tests
    /// that want to assert the worker's smart-sleep decisions
    /// against a moving clock.
    pub fn advance(&self, delta: Duration) {
        let mut now = self.state.fixed_now.lock().expect("MockClock now poisoned");
        *now += chrono::Duration::from_std(delta).expect("delta overflow");
    }
}

impl Clock for MockClock {
    fn now(&self) -> DateTime<Utc> {
        *self.state.fixed_now.lock().expect("MockClock now poisoned")
    }
    fn sleep_for(&self, duration: Duration) -> impl Future<Output = ()> + Send {
        self.state
            .sleeps
            .lock()
            .expect("MockClock sleeps poisoned")
            .push(duration);
        async {}
    }
}

/// [`JitterRng`] that always returns the same configured factor,
/// regardless of the requested range. Tests use this so sleep
/// durations are deterministic.
#[derive(Debug, Clone, Copy)]
pub struct FixedJitter {
    pub factor: f64,
}

impl FixedJitter {
    pub fn new(factor: f64) -> Self {
        Self { factor }
    }
}

impl JitterRng for FixedJitter {
    fn jitter_factor(&self, _low: f64, _high: f64) -> f64 {
        self.factor
    }
}
