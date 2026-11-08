//! Token-bucket speed limiter shared by every connection of a download
//! (per-file limit) or by every download (global limit).

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

pub struct RateLimiter {
    /// Bytes per second; 0 = unlimited. Can change while downloads run.
    rate: AtomicU64,
    /// (available tokens, last refill). Tokens may go negative ("debt"):
    /// the caller that overdraws sleeps until the debt is repaid.
    bucket: Mutex<(f64, Instant)>,
}

impl RateLimiter {
    pub fn new(rate: u64) -> Self {
        Self {
            rate: AtomicU64::new(rate),
            bucket: Mutex::new((0.0, Instant::now())),
        }
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
        // Start the new rate from an empty bucket, no stale debt or burst.
        *self.bucket.lock().unwrap() = (0.0, Instant::now());
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Account for `n` bytes, sleeping as long as needed to stay under the rate.
    pub async fn acquire(&self, n: usize) {
        let rate = self.rate();
        if rate == 0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock().unwrap();
            let now = Instant::now();
            let rate = rate as f64;
            // Allow at most ~1/8 s of burst so the limit feels smooth.
            let burst = (rate / 8.0).max(16.0 * 1024.0);
            b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * rate).min(burst);
            b.1 = now;
            b.0 -= n as f64;
            (b.0 < 0.0).then(|| Duration::from_secs_f64(-b.0 / rate))
        };
        if let Some(wait) = wait {
            tokio::time::sleep(wait).await;
        }
    }
}
