//! The retry schedule for transient failures. One knob (`--retries`); the
//! curve itself is written down here and nowhere else.

/// Initial 500ms, doubling, capped at 8s, jitter applied by the caller so the
/// curve stays a pure function.
#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    retries: u32,
}

impl Backoff {
    pub const INITIAL_MS: u64 = 500;
    pub const MAX_MS: u64 = 8_000;

    pub fn new(retries: u32) -> Self {
        Self { retries }
    }

    /// Total tries, including the first one.
    pub fn attempts(&self) -> u32 {
        self.retries + 1
    }

    /// Delay before retry number `attempt` (0 is the delay after the first
    /// failure). Jitter is not included.
    pub fn delay_ms(&self, attempt: u32) -> u64 {
        Self::INITIAL_MS
            .saturating_mul(1u64 << attempt.min(16))
            .min(Self::MAX_MS)
    }

    /// Full jitter: anywhere in `[0, delay_ms]`. `fraction` is the caller's
    /// random number in `[0, 1)`.
    pub fn delay_with_jitter_ms(&self, attempt: u32, fraction: f64) -> u64 {
        let base = self.delay_ms(attempt) as f64;
        (base * fraction.clamp(0.0, 1.0)) as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curve_doubles_then_caps() {
        let backoff = Backoff::new(2);
        assert_eq!(backoff.attempts(), 3);
        let delays: Vec<u64> = (0..6).map(|n| backoff.delay_ms(n)).collect();
        assert_eq!(delays, vec![500, 1000, 2000, 4000, 8000, 8000]);
    }

    #[test]
    fn jitter_stays_inside_the_window() {
        let backoff = Backoff::new(2);
        assert_eq!(backoff.delay_with_jitter_ms(1, 0.0), 0);
        assert_eq!(backoff.delay_with_jitter_ms(1, 0.5), 500);
        assert_eq!(backoff.delay_with_jitter_ms(1, 1.0), 1000);
    }
}
