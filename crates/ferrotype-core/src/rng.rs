//! A small random number generator for key timings, so typing doesn't run at a machine-perfect
//! rhythm. It's xorshift64*: fast and plenty random for that, and NOT for anything
//! security-related.

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::time::Duration;

pub struct Rng(u64);

impl Rng {
    /// Seeded from the OS (through std's per-process random hashing keys).
    pub fn seeded() -> Rng {
        Rng::new(RandomState::new().hash_one(0x5EED_u64))
    }

    pub fn new(seed: u64) -> Rng {
        // xorshift never leaves the all-zero state, so that one seed is nudged.
        Rng(if seed == 0 { 0x9E37_79B9_7F4A_7C15 } else { seed })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform between `low` and `high` (either order).
    pub fn between(&mut self, low: Duration, high: Duration) -> Duration {
        let (low, high) = if low <= high { (low, high) } else { (high, low) };
        low + (high - low).mul_f64(self.unit())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_in_range_and_varies() {
        let mut rng = Rng::new(42);
        let (low, high) = (Duration::from_millis(10), Duration::from_millis(30));
        let samples: Vec<Duration> = (0..1000).map(|_| rng.between(low, high)).collect();
        assert!(samples.iter().all(|d| *d >= low && *d < high));
        assert!(samples.iter().any(|d| *d < Duration::from_millis(15)));
        assert!(samples.iter().any(|d| *d > Duration::from_millis(25)));
        assert!(rng.between(high, low) >= low);
        let mut zero = Rng::new(0);
        assert_ne!(zero.next_u64(), 0);
    }
}
