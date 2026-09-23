//! Jitter abstraction, so the worker's sleep math is testable.
//!
//! Production uses [`RandJitter`]. Tests use [`crate::testing::FixedJitter`], which returns a
//! constant.

/// Picks the factor the worker multiplies its base poll interval by, from `[low, high)`.
pub trait JitterRng: Send + Sync + 'static {
    fn jitter_factor(&self, low: f64, high: f64) -> f64;
}

/// Production [`JitterRng`]: uniform sample from `rand::rng()`.
#[derive(Debug, Default, Clone, Copy)]
pub struct RandJitter;

impl JitterRng for RandJitter {
    fn jitter_factor(&self, low: f64, high: f64) -> f64 {
        use rand::RngExt;
        rand::rng().random_range(low..high)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production jitter stays inside the bounds. Every other test injects `FixedJitter`,
    /// so nothing else exercises `RandJitter`.
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
