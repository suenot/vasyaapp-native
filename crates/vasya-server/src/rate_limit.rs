//! Anti-flood rate limiting (plan §12 decision: strict server-side limits).
//!
//! Token bucket per telegram account applied to mutating operations
//! (send/forward/create/delete). Conservative defaults: burst of 10,
//! refill 1 token per 2 seconds. Telegram's own FLOOD_WAIT is handled
//! separately (flood.rs); this limiter keeps us from getting there.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::error::ApiError;

#[derive(Clone, Copy, Debug)]
pub struct RateLimitConfig {
    /// Bucket capacity (max burst of mutations).
    pub capacity: u32,
    /// Time to regenerate one token.
    pub refill_every: Duration,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            capacity: 10,
            refill_every: Duration::from_secs(2),
        }
    }
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

pub struct RateLimiter {
    config: RateLimitConfig,
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl RateLimiter {
    pub fn new(config: RateLimitConfig) -> Self {
        Self {
            config,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// Take one token for a mutating operation on `account_id`.
    pub fn check_mutation(&self, account_id: &str) -> Result<(), ApiError> {
        let mut buckets = self.buckets.lock().unwrap();
        let now = Instant::now();
        let bucket = buckets.entry(account_id.to_string()).or_insert(Bucket {
            tokens: self.config.capacity as f64,
            last_refill: now,
        });

        // Continuous refill proportional to elapsed time.
        let elapsed = now.duration_since(bucket.last_refill);
        let refill = elapsed.as_secs_f64() / self.config.refill_every.as_secs_f64();
        bucket.tokens = (bucket.tokens + refill).min(self.config.capacity as f64);
        bucket.last_refill = now;

        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            Ok(())
        } else {
            let deficit = 1.0 - bucket.tokens;
            let wait = self.config.refill_every.as_secs_f64() * deficit;
            Err(ApiError::RateLimited {
                retry_after_secs: wait.ceil() as u64,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn burst_up_to_capacity_then_reject() {
        let limiter = RateLimiter::new(RateLimitConfig {
            capacity: 3,
            refill_every: Duration::from_secs(60),
        });
        for _ in 0..3 {
            limiter.check_mutation("acc").unwrap();
        }
        match limiter.check_mutation("acc") {
            Err(ApiError::RateLimited { retry_after_secs }) => assert!(retry_after_secs >= 1),
            other => panic!("expected RateLimited, got {other:?}"),
        }
    }

    #[test]
    fn accounts_have_independent_buckets() {
        let limiter = RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_every: Duration::from_secs(60),
        });
        limiter.check_mutation("a").unwrap();
        limiter.check_mutation("b").unwrap();
        assert!(limiter.check_mutation("a").is_err());
    }

    #[test]
    fn refills_over_time() {
        let limiter = RateLimiter::new(RateLimitConfig {
            capacity: 1,
            refill_every: Duration::from_millis(20),
        });
        limiter.check_mutation("acc").unwrap();
        assert!(limiter.check_mutation("acc").is_err());
        std::thread::sleep(Duration::from_millis(40));
        limiter.check_mutation("acc").unwrap();
    }
}
