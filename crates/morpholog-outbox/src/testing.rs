//! Test implementations of [`crate::clock::Clock`] and [`crate::jitter::JitterRng`].
//!
//! Always compiled, so integration tests can use them without a feature flag. Production code
//! should not import them.

#![allow(clippy::expect_used)]

use jiff::Timestamp;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::clock::Clock;
use crate::jitter::JitterRng;

/// [`Clock`] that never sleeps and records every `sleep_for` call.
/// `now()` stays at the starting instant until [`MockClock::advance`] moves it.
///
/// Clones share state, so a test can hand one to the worker and inspect another.
#[derive(Clone)]
pub struct MockClock {
    state: Arc<MockClockState>,
}

struct MockClockState {
    fixed_now: Mutex<Timestamp>,
    sleeps: Mutex<Vec<Duration>>,
}

impl MockClock {
    pub fn new(starting_now: Timestamp) -> Self {
        Self {
            state: Arc::new(MockClockState {
                fixed_now: Mutex::new(starting_now),
                sleeps: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Every `sleep_for` duration so far, in order.
    pub fn sleeps(&self) -> Vec<Duration> {
        self.state
            .sleeps
            .lock()
            .expect("MockClock sleeps poisoned")
            .clone()
    }

    /// Move `now` forward by `delta`. Sleeps still resolve immediately; this only changes what
    /// `now()` reports.
    pub fn advance(&self, delta: Duration) {
        let mut now = self.state.fixed_now.lock().expect("MockClock now poisoned");
        *now = now.checked_add(delta).expect("delta overflow");
    }
}

impl Clock for MockClock {
    fn now(&self) -> Timestamp {
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

/// [`JitterRng`] that always returns `factor`, ignoring the requested range.
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
