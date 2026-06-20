//! Redis-backed curation verdict cache.
//!
//! Verdicts are cached per `(remote_repo_id, ecosystem, package, version)` with
//! a TTL so the hot path skips re-evaluating min-age + webhook on every fetch.
//! Keying by repo keeps two remotes with different policies from colliding and
//! lets a policy edit invalidate just that repo's verdicts. Redis is the only
//! verdict store — a cache miss triggers a fresh synchronous evaluation.

use redis::aio::ConnectionManager;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

use crate::services::curation_eval::Verdict;

/// A verdict as stored in Redis. Carries `package`/`version` so the
/// blocked-list view can be reconstructed from values alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedVerdict {
    pub verdict: Verdict,
    pub reason: String,
    #[serde(default)]
    pub package: String,
    #[serde(default)]
    pub version: String,
}

/// Build the Redis key for a verdict. Pure.
pub fn verdict_key(repo_id: &str, ecosystem: &str, package: &str, version: &str) -> String {
    format!("curation_verdict:{repo_id}:{ecosystem}:{package}:{version}")
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
        repo_id: &str,
        ecosystem: &str,
        package: &str,
        version: &str,
    ) -> Option<CachedVerdict> {
        let key = verdict_key(repo_id, ecosystem, package, version);
        let mut conn = self.conn.clone();
        let raw: Option<String> = conn.get(&key).await.ok().flatten();
        raw.and_then(|s| serde_json::from_str(&s).ok())
    }

    /// Store a verdict with a TTL (seconds, floored at 1). Errors are swallowed
    /// — a failed cache write must not break the fetch path.
    pub async fn set(
        &self,
        repo_id: &str,
        ecosystem: &str,
        package: &str,
        version: &str,
        verdict: &CachedVerdict,
        ttl_secs: i64,
    ) {
        let key = verdict_key(repo_id, ecosystem, package, version);
        let payload = match serde_json::to_string(verdict) {
            Ok(p) => p,
            Err(_) => return,
        };
        let ttl = ttl_secs.max(1) as u64;
        let mut conn = self.conn.clone();
        let _: redis::RedisResult<()> = conn.set_ex(&key, payload, ttl).await;
    }

    /// Collect all verdict keys for a repo via SCAN (cursor-based, non-blocking).
    async fn verdict_keys(&self, repo_id: &str) -> Vec<String> {
        let pattern = format!("curation_verdict:{repo_id}:*");
        let mut conn = self.conn.clone();
        let mut keys = Vec::new();
        match conn.scan_match::<&str, String>(&pattern).await {
            Ok(mut iter) => {
                while let Some(k) = iter.next_item().await {
                    keys.push(k);
                }
            }
            Err(_) => {}
        }
        keys
    }

    /// Drop every cached verdict for a repo (called on policy create/update/delete
    /// so changes take effect immediately). Returns the count removed.
    pub async fn invalidate_repo(&self, repo_id: &str) -> usize {
        let keys = self.verdict_keys(repo_id).await;
        if keys.is_empty() {
            return 0;
        }
        let mut conn = self.conn.clone();
        let _: redis::RedisResult<()> = conn.del(&keys).await;
        keys.len()
    }

    /// List currently-cached verdicts for a repo whose decision is `Block`.
    /// Reflects packages that have been requested and blocked, within TTL.
    pub async fn list_blocked(&self, repo_id: &str) -> Vec<CachedVerdict> {
        let keys = self.verdict_keys(repo_id).await;
        let mut conn = self.conn.clone();
        let mut out = Vec::new();
        for k in keys {
            let raw: Option<String> = conn.get(&k).await.ok().flatten();
            if let Some(v) = raw.and_then(|s| serde_json::from_str::<CachedVerdict>(&s).ok()) {
                if v.verdict == Verdict::Block {
                    out.push(v);
                }
            }
        }
        out
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

    fn cv(v: Verdict, pkg: &str, ver: &str) -> CachedVerdict {
        CachedVerdict {
            verdict: v,
            reason: "test".to_string(),
            package: pkg.to_string(),
            version: ver.to_string(),
        }
    }

    #[test]
    fn key_format_includes_repo() {
        assert_eq!(
            verdict_key("repo1", "pypi", "requests", "2.31.0"),
            "curation_verdict:repo1:pypi:requests:2.31.0"
        );
    }

    fn redis_url() -> String {
        std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:30379".to_string())
    }

    #[tokio::test]
    async fn roundtrip_get_set() {
        let cache = VerdictCache::connect(&redis_url()).await.expect("redis");
        let v = cv(Verdict::Block, "rt-pkg", "1.0.0");
        cache.set("repoA", "pypi", "rt-pkg", "1.0.0", &v, 60).await;
        assert_eq!(cache.get("repoA", "pypi", "rt-pkg", "1.0.0").await, Some(v));
        // Different repo, same package -> independent (no collision).
        assert_eq!(cache.get("repoB", "pypi", "rt-pkg", "1.0.0").await, None);
    }

    #[tokio::test]
    async fn invalidate_and_list_blocked() {
        let cache = VerdictCache::connect(&redis_url()).await.expect("redis");
        let repo = "repo-inv";
        cache
            .set(
                repo,
                "pypi",
                "blk",
                "1.0.0",
                &cv(Verdict::Block, "blk", "1.0.0"),
                300,
            )
            .await;
        cache
            .set(
                repo,
                "pypi",
                "ok",
                "2.0.0",
                &cv(Verdict::Allow, "ok", "2.0.0"),
                300,
            )
            .await;

        let blocked = cache.list_blocked(repo).await;
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].package, "blk");

        let removed = cache.invalidate_repo(repo).await;
        assert!(removed >= 2);
        assert!(cache.get(repo, "pypi", "blk", "1.0.0").await.is_none());
        assert!(cache.list_blocked(repo).await.is_empty());
    }
}
