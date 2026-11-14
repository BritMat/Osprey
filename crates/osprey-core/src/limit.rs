//! Speed limiter, shared by every download that uses it.
//!
//! Downloads call [`SpeedLimit::take`] with the bytes they just received.
//! When they are ahead of the allowed rate, `take` waits until they are
//! back on it. The rate can change while downloads run (0 means no
//! limit), and the budget is per second, so short bursts are smoothed.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SpeedLimit {
    bytes_per_sec: AtomicU64,
    /// The moment the bytes received so far are "paid off" at the limit.
    /// Every chunk pushes it forward, so debt is never forgotten.
    clock: Mutex<Instant>,
}

/// Unused allowance kept after an idle spell, so a short pause does not
/// turn into a big burst.
const BURST: Duration = Duration::from_millis(250);

impl Default for SpeedLimit {
    fn default() -> Self {
        Self::new(0)
    }
}

impl SpeedLimit {
    pub fn new(bytes_per_sec: u64) -> Self {
        Self {
            bytes_per_sec: AtomicU64::new(bytes_per_sec),
            clock: Mutex::new(Instant::now()),
        }
    }

    pub fn set(&self, bytes_per_sec: u64) {
        self.bytes_per_sec.store(bytes_per_sec, Ordering::Relaxed);
        *self.clock.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
    }

    pub fn rate(&self) -> u64 {
        self.bytes_per_sec.load(Ordering::Relaxed)
    }

    /// How long to wait after receiving `n` more bytes.
    fn delay_for(&self, n: u64) -> Duration {
        let rate = self.rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        let now = Instant::now();
        let mut clock = self.clock.lock().unwrap_or_else(|e| e.into_inner());
        let floor = now.checked_sub(BURST).unwrap_or(now);
        if *clock < floor {
            *clock = floor;
        }
        *clock += Duration::from_secs_f64(n as f64 / rate as f64);
        clock.saturating_duration_since(now)
    }

    pub async fn take(&self, n: u64) {
        let d = self.delay_for(n);
        if !d.is_zero() {
            // Capped so a rate lowered mid-download never stalls for long.
            tokio::time::sleep(d.min(Duration::from_secs(10))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limit_never_waits() {
        let l = SpeedLimit::new(0);
        assert_eq!(l.delay_for(1 << 30), Duration::ZERO);
    }

    #[test]
    fn waits_when_ahead_of_the_rate() {
        let l = SpeedLimit::new(1_000_000);
        // 2 MB at 1 MB/s should take about 2 seconds from the start.
        let d = l.delay_for(2_000_000);
        // A fresh limiter allows a quarter second of burst.
        assert!(
            d > Duration::from_millis(1650) && d <= Duration::from_secs(2),
            "{d:?}"
        );
        l.set(0);
        assert_eq!(l.delay_for(10_000_000), Duration::ZERO);
    }

    #[test]
    fn debt_is_not_forgotten() {
        // Owing 8 s of data must still be owed later, unlike a window
        // that resets and lets the next burst through.
        let l = SpeedLimit::new(100_000);
        let first = l.delay_for(800_000);
        let second = l.delay_for(100_000);
        assert!(second > first, "{first:?} then {second:?}");
    }

    #[tokio::test]
    async fn take_slows_a_burst_down() {
        let l = SpeedLimit::new(200_000);
        let start = Instant::now();
        for _ in 0..4 {
            l.take(25_000).await;
        }
        // 100 KB at 200 KB/s is half a second, minus the burst allowance.
        assert!(
            start.elapsed() >= Duration::from_millis(90),
            "{:?}",
            start.elapsed()
        );
    }
}
