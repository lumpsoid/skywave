//! Reconnect delays (D7): exponential backoff with jitter, never above
//! [`Backoff::MAX`].

use std::collections::hash_map::RandomState;
use std::fmt;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::Duration;

/// How long the sync engine waits before reconnect attempt `n`: the base
/// doubles per attempt up to the cap, and jitter takes up to half of it
/// away, so devices that lost the link together do not return together.
#[derive(Debug, Clone)]
pub struct Backoff {
    /// The delay before the first retry, before jitter.
    pub base: Duration,
    /// The largest delay; values above [`Backoff::MAX`] count as `MAX`.
    pub cap: Duration,
    pub jitter: Jitter,
}

impl Backoff {
    /// The spec's ceiling for any reconnect delay (§7).
    pub const MAX: Duration = Duration::from_secs(30);

    /// The delay before reconnect attempt `attempt` (1 for the first
    /// retry): `min(cap, base · 2^(attempt-1))`, scaled by a jitter factor
    /// in `[0.5, 1]`.
    pub fn delay(&self, attempt: u32) -> Duration {
        let cap = self.cap.min(Backoff::MAX);
        let doublings = attempt.saturating_sub(1).min(31);
        let full = self.base.saturating_mul(1 << doublings).min(cap);
        let unit = self.jitter.sample().clamp(0.0, 1.0);
        full.mul_f64(0.5 + 0.5 * unit)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff {
            base: Duration::from_millis(500),
            cap: Backoff::MAX,
            jitter: Jitter::random(),
        }
    }
}

/// The source of [`Backoff`]'s randomness: values in `[0, 1]`. Injectable
/// so tests see exact delays.
#[derive(Clone)]
pub struct Jitter(Arc<dyn Fn() -> f64 + Send + Sync>);

impl Jitter {
    /// Uniformly random.
    pub fn random() -> Jitter {
        Jitter::new(random_unit)
    }

    /// Always `unit`: 0 gives half the delay, 1 the full delay.
    pub fn fixed(unit: f64) -> Jitter {
        Jitter::new(move || unit)
    }

    pub fn new(sample: impl Fn() -> f64 + Send + Sync + 'static) -> Jitter {
        Jitter(Arc::new(sample))
    }

    fn sample(&self) -> f64 {
        (self.0)()
    }
}

impl fmt::Debug for Jitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Jitter")
    }
}

/// A random value in `[0, 1)` from std's per-process random hash keys;
/// plenty for spreading reconnects.
fn random_unit() -> f64 {
    let bits = RandomState::new().build_hasher().finish() >> 11;
    bits as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backoff(unit: f64) -> Backoff {
        Backoff {
            base: Duration::from_millis(500),
            cap: Backoff::MAX,
            jitter: Jitter::fixed(unit),
        }
    }

    #[test]
    fn doubles_per_attempt_up_to_the_cap() {
        let full = backoff(1.0);
        let delays: Vec<u64> = (1..=9).map(|n| full.delay(n).as_millis() as u64).collect();
        assert_eq!(
            delays,
            [500, 1000, 2000, 4000, 8000, 16000, 30000, 30000, 30000]
        );
        assert_eq!(full.delay(u32::MAX), Backoff::MAX);
        assert_eq!(full.delay(0), Duration::from_millis(500));
    }

    #[test]
    fn jitter_takes_at_most_half() {
        assert_eq!(backoff(0.0).delay(3), Duration::from_millis(1000));
        assert_eq!(backoff(0.5).delay(3), Duration::from_millis(1500));
        // Out-of-range samples are clamped.
        assert_eq!(backoff(7.0).delay(3), Duration::from_millis(2000));
        assert_eq!(backoff(-1.0).delay(3), Duration::from_millis(1000));
    }

    #[test]
    fn cap_never_exceeds_the_spec_ceiling() {
        let wide = Backoff {
            base: Duration::from_secs(10),
            cap: Duration::from_secs(600),
            jitter: Jitter::fixed(1.0),
        };
        assert_eq!(wide.delay(10), Backoff::MAX);
        let narrow = Backoff {
            cap: Duration::from_secs(3),
            ..wide
        };
        assert_eq!(narrow.delay(10), Duration::from_secs(3));
    }

    #[test]
    fn random_jitter_stays_in_range() {
        let random = Backoff {
            jitter: Jitter::random(),
            ..backoff(0.0)
        };
        for _ in 0..1000 {
            let d = random.delay(2);
            assert!(
                (Duration::from_millis(500)..=Duration::from_millis(1000)).contains(&d),
                "{d:?}"
            );
        }
    }
}
