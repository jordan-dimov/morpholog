//! Jitter abstraction so the polling worker's sleep math is
//! testable without depending on a random RNG.
//!
//! Production uses [`RandJitter`] which draws from
//! `rand::thread_rng`. Tests use [`crate::testing::FixedJitter`]
//! which returns a configured constant, making sleep-duration
//! assertions deterministic.

/// One-method trait the worker uses to pick a multiplicative
/// jitter factor for its base poll interval. The factor is
/// expected to lie in some configured range (typically `[0.75,
/// 1.25]`, i.e. ±25% around the base).
pub trait JitterRng: Send + Sync + 'static {
    fn jitter_factor(&self, low: f64, high: f64) -> f64;
}

/// Production [`JitterRng`]: uniform sample from `rand::rng()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandJitter;

impl JitterRng for RandJitter {
    fn jitter_factor(&self, low: f64, high: f64) -> f64 {
        use rand::Rng;
        rand::rng().random_range(low..high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production jitter draws inside the configured range. A
    /// handful of draws, not a distribution test - the contract is the
    /// bounds. Pinned for the same reason as `RealClock`: every other
    /// test injects `FixedJitter`.
    #[test]
    fn rand_jitter_stays_inside_the_bounds() {
        let jitter = RandJitter;
        for _ in 0..100 {
            let factor = jitter.jitter_factor(0.75, 1.25);
            assert!(
                (0.75..1.25).contains(&factor),
                "factor out of range: {factor}"
            );
        }
    }
}
