//! Auth module: password hashing for protected shares, plus in-memory
//! request rate limiting. Nothing in this module ever logs or stores a
//! plaintext password.

use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use rand::rngs::OsRng;
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub fn hash_password(plain: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let hash = argon2
        .hash_password(plain.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("password hashing failed: {e}"))?;
    Ok(hash.to_string())
}

pub fn verify_password(plain: &str, hash: &str) -> bool {
    let parsed = match PasswordHash::new(hash) {
        Ok(p) => p,
        Err(_) => return false,
    };
    Argon2::default()
        .verify_password(plain.as_bytes(), &parsed)
        .is_ok()
}

/// Simple sliding-window, in-memory rate limiter keyed by IP address.
/// Good enough for a single-process local server; if this ever needs to
/// scale across multiple processes, swap the backing store for Redis.
pub struct RateLimiter {
    window: Duration,
    max_requests: u32,
    hits: Mutex<HashMap<IpAddr, Vec<Instant>>>,
}

impl RateLimiter {
    pub fn new(max_requests: u32, window: Duration) -> Self {
        Self {
            window,
            max_requests,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the request should be allowed.
    pub fn check(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        let entry = hits.entry(ip).or_default();
        entry.retain(|t| now.duration_since(*t) < self.window);
        if entry.len() as u32 >= self.max_requests {
            false
        } else {
            entry.push(now);
            true
        }
    }

    /// Periodic cleanup so the map doesn't grow unbounded from one-off IPs.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut hits = self.hits.lock().unwrap();
        hits.retain(|_, times| {
            times.retain(|t| now.duration_since(*t) < self.window);
            !times.is_empty()
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let h = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &h));
        assert!(!verify_password("wrong password", &h));
    }

    #[test]
    fn rate_limiter_blocks_after_threshold() {
        let rl = RateLimiter::new(3, Duration::from_secs(60));
        let ip: IpAddr = "127.0.0.1".parse().unwrap();
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(rl.check(ip));
        assert!(!rl.check(ip));
    }
}
