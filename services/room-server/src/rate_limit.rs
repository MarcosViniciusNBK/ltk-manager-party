//! In-memory rate limiting to protect join endpoints from brute-force password guessing.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct RateLimiter {
    attempts: Arc<Mutex<HashMap<(IpAddr, String), (u32, Instant)>>>,
    max_attempts: u32,
    window: Duration,
}

impl RateLimiter {
    pub fn new(max_attempts: u32, window_secs: u64) -> Self {
        Self {
            attempts: Arc::new(Mutex::new(HashMap::new())),
            max_attempts,
            window: Duration::from_secs(window_secs),
        }
    }

    /// Check whether a client is allowed to attempt authentication for a room.
    pub async fn check(&self, ip: IpAddr, room_id: &str) -> bool {
        let mut attempts = self.attempts.lock().await;
        let key = (ip, room_id.to_string());
        let now = Instant::now();

        if let Some((count, first_attempt)) = attempts.get_mut(&key) {
            if now.duration_since(*first_attempt) > self.window {
                *count = 0;
                *first_attempt = now;
                true
            } else {
                *count < self.max_attempts
            }
        } else {
            true
        }
    }

    /// Record a failed password attempt, incrementing counter.
    pub async fn record_failure(&self, ip: IpAddr, room_id: &str) {
        let mut attempts = self.attempts.lock().await;
        let key = (ip, room_id.to_string());
        let now = Instant::now();

        let entry = attempts.entry(key).or_insert((0, now));
        if now.duration_since(entry.1) > self.window {
            entry.0 = 1;
            entry.1 = now;
        } else {
            entry.0 += 1;
        }
    }

    /// Reset failure count upon successful authentication.
    pub async fn reset(&self, ip: IpAddr, room_id: &str) {
        let mut attempts = self.attempts.lock().await;
        attempts.remove(&(ip, room_id.to_string()));
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(5, 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn rate_limiting_blocks_after_max_attempts() {
        let limiter = RateLimiter::new(3, 10);
        let ip = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
        let room = "test_room";

        assert!(limiter.check(ip, room).await);
        limiter.record_failure(ip, room).await;
        assert!(limiter.check(ip, room).await);
        limiter.record_failure(ip, room).await;
        assert!(limiter.check(ip, room).await);
        limiter.record_failure(ip, room).await;

        // 3rd failure reached max_attempts (3)
        assert!(!limiter.check(ip, room).await);

        // Reset works
        limiter.reset(ip, room).await;
        assert!(limiter.check(ip, room).await);
    }
}
