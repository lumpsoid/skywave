//! A tiny seedable generator (SplitMix64): enough randomness for link
//! profiles, reproducible from one `u64`.

use std::ops::RangeInclusive;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Environment variable that fixes the seed [`seed`] returns.
pub const SEED_ENV: &str = "SKYWAVE_LINK_SEED";

/// The seed for this run: `SKYWAVE_LINK_SEED` if set, else a fresh one. It
/// is printed to stderr, which `cargo test` shows when the test fails.
pub fn seed() -> u64 {
    let seed = match std::env::var(SEED_ENV) {
        Ok(value) => value
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("{SEED_ENV} must be a u64, got {value:?}")),
        Err(_) => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            Rng::new(nanos ^ u64::from(std::process::id()).rotate_left(32)).next_u64()
        }
    };
    eprintln!("link seed {seed} (repeat with {SEED_ENV}={seed})");
    seed
}

/// SplitMix64. Not for anything but tests.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed)
    }

    /// An independent generator for sub-stream `n` (e.g. one per
    /// connection), so the order in which streams draw does not matter.
    pub fn fork(&self, n: u64) -> Rng {
        let mut base = Rng(self.0 ^ n.wrapping_mul(0xA24B_AED4_963E_E407));
        Rng(base.next_u64())
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n`; 0 when `n` is 0.
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }

    /// Uniform in the range.
    pub fn in_range(&mut self, range: RangeInclusive<u64>) -> u64 {
        let (lo, hi) = (*range.start(), *range.end());
        if hi <= lo {
            return lo;
        }
        lo + self.below(hi - lo + 1)
    }

    /// Uniform in `0..=max`, microsecond resolution.
    pub fn duration(&mut self, max: Duration) -> Duration {
        Duration::from_micros(self.in_range(0..=max.as_micros() as u64))
    }

    /// Uniform in the range, microsecond resolution.
    pub fn duration_in(&mut self, range: RangeInclusive<Duration>) -> Duration {
        let (lo, hi) = (*range.start(), *range.end());
        lo + self.duration(hi.saturating_sub(lo))
    }

    /// True with probability `p`.
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64 <= p
    }

    /// A uniformly chosen element.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.below(items.len() as u64) as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_stream() {
        let (mut a, mut b) = (Rng::new(7), Rng::new(7));
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        assert_ne!(
            Rng::new(7).fork(1).next_u64(),
            Rng::new(7).fork(2).next_u64()
        );
    }

    #[test]
    fn ranges_hold() {
        let mut rng = Rng::new(1);
        for _ in 0..1000 {
            assert!((5..=9).contains(&rng.in_range(5..=9)));
            let d = rng.duration_in(Duration::from_millis(10)..=Duration::from_millis(20));
            assert!(d >= Duration::from_millis(10) && d <= Duration::from_millis(20));
        }
        assert_eq!(rng.in_range(3..=3), 3);
        assert_eq!(rng.below(0), 0);
    }
}
