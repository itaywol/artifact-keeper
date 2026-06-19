//! Redis-backed curation verdict cache.
//!
//! Verdicts are cached per `(ecosystem, package, version)` with a TTL so the
//! hot path skips re-evaluating min-age + webhook on every fetch. Redis is the
//! only verdict store (no durable catalog) — a cache miss triggers a fresh
//! synchronous evaluation.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use crate::services::curation_eval::Verdict;

/// A verdict as stored in Redis.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedVerdict {
    pub verdict: Verdict,
    pub reason: String,
}

/// Build the Redis key for a verdict. Pure.
pub fn verdict_key(ecosystem: &str, package: &str, version: &str) -> String {
    format!("curation_verdict:{ecosystem}:{package}:{version}")
}

/// Cloneable handle to the verdict cache. `ConnectionManager` multiplexes and
/// reconnects, so clones are cheap and share one connection.
#[derive(Clone)]
pub struct VerdictCache {
    conn: ConnectionManager,
}

impl VerdictCache {
    /// Connect to Redis (e.g. `redis://localhost:6379`).
    pub async fn connect(url: &str) -> redis::RedisResult<Self> {
        let client = redis::Client::open(url)?;
        let conn = ConnectionManager::new(client).await?;
        Ok(Self { conn })
    }

    /// Look up a cached verdict. Returns `None` on miss, parse failure, or any
    /// Redis error (treated as a miss — the caller re-evaluates).
    pub async fn get(
        &self,
        ecosystem: &str,
        package: &str,
        version: &str,
    ) -> Option<CachedVerdict> {
        let key = verdict_key(ecosystem, package, version);
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(&key).await.ok().flatten();
        raw.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Store a verdict with a TTL (seconds, floored at 1). Errors are swallowed
    /// — a failed cache write must not break the fetch path.
    pub async fn set(
        &self,
        ecosystem: &str,
        package: &str,
        version: &str,
        verdict: &CachedVerdict,
        ttl_secs: i64,
    ) {
        let key = verdict_key(ecosystem, package, version);
        let payload = match serde_json::to_string(verdict) {
            Ok(p) => p,
            Err(_) => return,
        };
        let ttl = ttl_secs.max(1) as u64;
        let mut conn = self.conn.clone();
        let _: redis::RedisResult<()> = conn.set_ex(&key, payload, ttl).await;
    }

    /// Raw string GET (used for cached upstream metadata). `None` on miss/error.
    pub async fn get_raw(&self, key: &str) -> Option<String> {
        let mut conn = self.conn.clone();
        conn.get(key).await.ok().flatten()
    }

    /// Raw string SET with TTL (seconds, floored at 1). Errors swallowed.
    pub async fn set_raw(&self, key: &str, value: &str, ttl_secs: i64) {
        let ttl = ttl_secs.max(1) as u64;
        let mut conn = self.conn.clone();
        let _: redis::RedisResult<()> = conn.set_ex(key, value, ttl).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_format() {
        assert_eq!(
            verdict_key("pypi", "requests", "2.31.0"),
            "curation_verdict:pypi:requests:2.31.0"
        );
    }

    fn redis_url() -> String {
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:30379".to_string())
    }

    #[tokio::test]
    async fn roundtrip_get_set() {
        let cache = match VerdictCache::connect(&redis_url()).await {
            Ok(c) => c,
            Err(e) => panic!("redis required for this test (REDIS_URL): {e}"),
        };
        let v = CachedVerdict {
            verdict: Verdict::Block,
            reason: "min-age: version too new".to_string(),
        };
        cache.set("pypi", "rt-pkg", "1.0.0", &v, 60).await;
        let got = cache.get("pypi", "rt-pkg", "1.0.0").await;
        assert_eq!(got, Some(v));

        // Unknown key is a miss.
        assert_eq!(cache.get("pypi", "rt-pkg", "9.9.9").await, None);
    }

    #[tokio::test]
    async fn ttl_expires() {
        let cache = match VerdictCache::connect(&redis_url()).await {
            Ok(c) => c,
            Err(e) => panic!("redis required for this test (REDIS_URL): {e}"),
        };
        let v = CachedVerdict {
            verdict: Verdict::Allow,
            reason: "all gates passed".to_string(),
        };
        cache.set("pypi", "ttl-pkg", "1.0.0", &v, 1).await;
        assert!(cache.get("pypi", "ttl-pkg", "1.0.0").await.is_some());
        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        assert_eq!(cache.get("pypi", "ttl-pkg", "1.0.0").await, None);
    }
}
