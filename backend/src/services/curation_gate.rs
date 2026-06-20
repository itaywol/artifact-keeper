//! Curation gate orchestrator — the brain on the fetch path.
//!
//! Resolves a verdict for a `(package, version)` fetched through a Remote repo:
//! Redis cache → explicit rules → min-age → webhook → combine → cache. The
//! decision + cache-TTL logic is pure ([`decide`] / [`cache_ttl`]); IO (DB,
//! upstream fetch, webhook, Redis) lives in the async methods.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::models::curation::CurationPolicy;
use crate::services::curation_cache::{CachedVerdict, VerdictCache};
use crate::services::curation_eval::{
    self, evaluate, min_age_block_ttl_secs, min_age_gate, ExplicitRule, Gate, GateResult, Verdict,
};
use crate::services::curation_service::CurationService;
use crate::services::curation_webhook::{self, WebhookPayload};
use crate::services::pypi_metadata;

const ECOSYSTEM: &str = "pypi";
const DEFAULT_ALLOW_TTL: i64 = 3600;
const EXPLICIT_TTL: i64 = 3600;
const FAILCLOSED_TTL: i64 = 60;
const WEBHOOK_BLOCK_TTL: i64 = 300;
const UPLOAD_TIMES_TTL: i64 = 600;

/// Pure verdict decision. The webhook result is injected (caller made the
/// call); min-age is derived from `age_days`.
///
/// `min_age_days = Some` means the gate is enabled. `webhook = Some((result,
/// _ttl))` means the webhook gate is enabled. Min-age uses a fail-closed
/// stance when the publish time is unknown.
pub fn decide(
    explicit: ExplicitRule,
    min_age_days: Option<i64>,
    age_days: Option<f64>,
    webhook: Option<(GateResult, Option<i64>)>,
    webhook_fail_closed: bool,
    default_action: Verdict,
) -> curation_eval::EvalResult {
    let min_age_gate = min_age_days.map(|d| Gate {
        result: min_age_gate(age_days, d),
        fail_closed: true,
    });
    let webhook_gate = webhook.map(|(result, _)| Gate {
        result,
        fail_closed: webhook_fail_closed,
    });
    evaluate(explicit, min_age_gate, webhook_gate, default_action)
}

/// TTL (seconds) for caching the verdict, mirroring [`decide`]'s precedence so
/// a min-age block expires exactly when the version ages past the threshold.
pub fn cache_ttl(
    explicit: ExplicitRule,
    min_age_days: Option<i64>,
    age_days: Option<f64>,
    webhook: Option<(GateResult, Option<i64>)>,
) -> i64 {
    match explicit {
        ExplicitRule::Allow | ExplicitRule::Block => return EXPLICIT_TTL,
        ExplicitRule::None => {}
    }
    if let Some(d) = min_age_days {
        match min_age_gate(age_days, d) {
            GateResult::Block => {
                return match age_days {
                    Some(age) => min_age_block_ttl_secs(age, d),
                    None => FAILCLOSED_TTL,
                };
            }
            GateResult::Unavailable => return FAILCLOSED_TTL,
            GateResult::Pass => {}
        }
    }
    if let Some((result, ttl)) = webhook {
        match result {
            GateResult::Block => return ttl.unwrap_or(WEBHOOK_BLOCK_TTL),
            GateResult::Unavailable => return FAILCLOSED_TTL,
            GateResult::Pass => return ttl.unwrap_or(DEFAULT_ALLOW_TTL),
        }
    }
    DEFAULT_ALLOW_TTL
}

/// Build the PEP 691 JSON simple-index URL for a project on an upstream.
pub fn simple_json_url(upstream_url: &str, package: &str) -> String {
    let base = upstream_url.trim_end_matches('/');
    let base = base.strip_suffix("/simple").unwrap_or(base);
    format!("{base}/simple/{package}/")
}

/// Orchestrator. Cheap to construct per request.
pub struct CurationGate<'a> {
    pub db: &'a PgPool,
    pub cache: Option<&'a VerdictCache>,
    pub http: &'a reqwest::Client,
}

impl<'a> CurationGate<'a> {
    pub fn new(db: &'a PgPool, cache: Option<&'a VerdictCache>, http: &'a reqwest::Client) -> Self {
        Self { db, cache, http }
    }

    /// Load the curation policy for a Remote repo, if one is enabled.
    pub async fn load_policy(&self, remote_repo_id: uuid::Uuid) -> Option<CurationPolicy> {
        sqlx::query_as::<_, CurationPolicy>(
            "SELECT * FROM curation_policies WHERE remote_repo_id = $1 AND enabled = true",
        )
        .bind(remote_repo_id)
        .fetch_optional(self.db)
        .await
        .ok()
        .flatten()
    }

    /// Resolve an explicit allow/block rule for this package/version.
    async fn explicit_lookup(
        &self,
        remote_repo_id: uuid::Uuid,
        package: &str,
        version: &str,
    ) -> ExplicitRule {
        let rules = sqlx::query_as::<_, crate::models::curation::CurationRule>(
            r#"SELECT * FROM curation_rules
               WHERE enabled = true AND (staging_repo_id = $1 OR staging_repo_id IS NULL)
               ORDER BY priority ASC, created_at ASC"#,
        )
        .bind(remote_repo_id)
        .fetch_all(self.db)
        .await
        .unwrap_or_default();

        for r in &rules {
            if CurationService::pattern_matches(&r.package_pattern, package)
                && CurationService::version_matches(&r.version_constraint, version)
            {
                return match r.action.as_str() {
                    "allow" => ExplicitRule::Allow,
                    "block" => ExplicitRule::Block,
                    _ => continue,
                };
            }
        }
        ExplicitRule::None
    }

    /// Fetch per-version publish times from the upstream PEP 691 JSON index,
    /// cached in Redis. Returns an empty map on any failure (→ unknown ages →
    /// min-age fail-closed).
    pub async fn upload_times(
        &self,
        upstream_url: &str,
        package: &str,
    ) -> HashMap<String, DateTime<Utc>> {
        let cache_key = format!("curation_uploadtimes:{ECOSYSTEM}:{package}");

        if let Some(cache) = self.cache {
            if let Some(map) = cache.get_raw(&cache_key).await {
                if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(&map) {
                    return parse_rfc3339_map(&parsed);
                }
            }
        }

        let url = simple_json_url(upstream_url, package);
        let body = match self
            .http
            .get(&url)
            .header("Accept", "application/vnd.pypi.simple.v1+json")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp.text().await.unwrap_or_default(),
            _ => return HashMap::new(),
        };

        let times = pypi_metadata::parse_upload_times(&body);

        if let Some(cache) = self.cache {
            let as_str: HashMap<String, String> = times
                .iter()
                .map(|(k, v)| (k.clone(), v.to_rfc3339()))
                .collect();
            if let Ok(payload) = serde_json::to_string(&as_str) {
                cache.set_raw(&cache_key, &payload, UPLOAD_TIMES_TTL).await;
            }
        }
        times
    }

    /// Evaluate a single version, consulting/refreshing the Redis cache.
    pub async fn evaluate(
        &self,
        policy: &CurationPolicy,
        upstream_url: &str,
        package: &str,
        version: &str,
        sha256: Option<&str>,
    ) -> Verdict {
        if let Some(cache) = self.cache {
            let repo_id = policy.remote_repo_id.to_string();
            if let Some(c) = cache.get(&repo_id, ECOSYSTEM, package, version).await {
                return c.verdict;
            }
        }

        let times = if policy.min_age_enabled {
            self.upload_times(upstream_url, package).await
        } else {
            HashMap::new()
        };
        self.evaluate_uncached(policy, package, version, sha256, &times)
            .await
    }

    /// Evaluate against a prefetched upload-times map (used for both single
    /// lookups and bulk index filtering so the upstream JSON is fetched once).
    async fn evaluate_uncached(
        &self,
        policy: &CurationPolicy,
        package: &str,
        version: &str,
        sha256: Option<&str>,
        times: &HashMap<String, DateTime<Utc>>,
    ) -> Verdict {
        let explicit = self
            .explicit_lookup(policy.remote_repo_id, package, version)
            .await;

        let age_days = times
            .get(version)
            .map(|t| pypi_metadata::age_days(*t, Utc::now()));

        let webhook = if policy.webhook_enabled {
            if let Some(url) = policy.webhook_url.as_deref() {
                let payload = WebhookPayload {
                    ecosystem: ECOSYSTEM,
                    package,
                    version,
                    sha256,
                };
                let v = curation_webhook::call(
                    self.http,
                    url,
                    policy.webhook_timeout_ms.max(1) as u64,
                    &payload,
                )
                .await;
                Some((v.gate, v.ttl_secs))
            } else {
                None
            }
        } else {
            None
        };

        let min_age_days = if policy.min_age_enabled {
            policy.min_age_days.map(|d| d as i64)
        } else {
            None
        };
        let default_action = parse_action(&policy.default_action);
        let webhook_fail_closed = policy.webhook_fail_mode == "closed";

        let res = decide(
            explicit,
            min_age_days,
            age_days,
            webhook,
            webhook_fail_closed,
            default_action,
        );
        let ttl = cache_ttl(explicit, min_age_days, age_days, webhook);

        if let Some(cache) = self.cache {
            let repo_id = policy.remote_repo_id.to_string();
            cache
                .set(
                    &repo_id,
                    ECOSYSTEM,
                    package,
                    version,
                    &CachedVerdict {
                        verdict: res.verdict,
                        reason: res.reason.clone(),
                        package: package.to_string(),
                        version: version.to_string(),
                    },
                    ttl,
                )
                .await;
        }
        res.verdict
    }

    /// Filter a version list to those allowed by the policy. Fetches upstream
    /// upload times once. Used by the `/simple` index hook.
    pub async fn allowed_versions(
        &self,
        policy: &CurationPolicy,
        upstream_url: &str,
        package: &str,
        versions: &[String],
    ) -> std::collections::HashSet<String> {
        let times = if policy.min_age_enabled {
            self.upload_times(upstream_url, package).await
        } else {
            HashMap::new()
        };
        let mut allowed = std::collections::HashSet::new();
        for v in versions {
            // Reuse the per-version Redis verdict where present, else evaluate
            // against the already-fetched upload-times map.
            let cached = match self.cache {
                Some(cache) => {
                    let repo_id = policy.remote_repo_id.to_string();
                    cache.get(&repo_id, ECOSYSTEM, package, v).await
                }
                None => None,
            };
            let verdict = match cached {
                Some(c) => c.verdict,
                None => {
                    self.evaluate_uncached(policy, package, v, None, &times)
                        .await
                }
            };
            if verdict == Verdict::Allow {
                allowed.insert(v.clone());
            }
        }
        allowed
    }
}

fn parse_action(s: &str) -> Verdict {
    match s {
        "block" => Verdict::Block,
        _ => Verdict::Allow,
    }
}

fn parse_rfc3339_map(m: &HashMap<String, String>) -> HashMap<String, DateTime<Utc>> {
    m.iter()
        .filter_map(|(k, v)| {
            DateTime::parse_from_rfc3339(v)
                .ok()
                .map(|t| (k.clone(), t.with_timezone(&Utc)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_json_url_handles_trailing_and_simple_suffix() {
        assert_eq!(
            simple_json_url("https://pypi.org", "requests"),
            "https://pypi.org/simple/requests/"
        );
        assert_eq!(
            simple_json_url("https://pypi.org/", "requests"),
            "https://pypi.org/simple/requests/"
        );
        assert_eq!(
            simple_json_url("https://pypi.org/simple", "requests"),
            "https://pypi.org/simple/requests/"
        );
    }

    // --- decide: precedence + gates ---

    #[test]
    fn decide_explicit_block_wins() {
        let r = decide(
            ExplicitRule::Block,
            Some(14),
            Some(100.0),
            Some((GateResult::Pass, None)),
            true,
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
    }

    #[test]
    fn decide_min_age_too_new_blocks() {
        let r = decide(
            ExplicitRule::None,
            Some(14),
            Some(3.0),
            None,
            true,
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
    }

    #[test]
    fn decide_min_age_old_enough_allows() {
        let r = decide(
            ExplicitRule::None,
            Some(14),
            Some(30.0),
            None,
            true,
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Allow);
    }

    #[test]
    fn decide_min_age_unknown_fail_closed() {
        let r = decide(
            ExplicitRule::None,
            Some(14),
            None,
            None,
            true,
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
    }

    #[test]
    fn decide_webhook_block_after_min_age_pass() {
        let r = decide(
            ExplicitRule::None,
            Some(14),
            Some(30.0),
            Some((GateResult::Block, None)),
            true,
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
    }

    // --- cache_ttl ---

    #[test]
    fn ttl_min_age_block_counts_down() {
        // 10 days old, need 14 → 4 days.
        assert_eq!(
            cache_ttl(ExplicitRule::None, Some(14), Some(10.0), None),
            4 * 86_400
        );
    }

    #[test]
    fn ttl_min_age_unknown_is_short() {
        assert_eq!(
            cache_ttl(ExplicitRule::None, Some(14), None, None),
            FAILCLOSED_TTL
        );
    }

    #[test]
    fn ttl_webhook_block_uses_hint() {
        assert_eq!(
            cache_ttl(
                ExplicitRule::None,
                None,
                None,
                Some((GateResult::Block, Some(42)))
            ),
            42
        );
    }

    #[test]
    fn ttl_allow_default_when_no_hint() {
        assert_eq!(
            cache_ttl(
                ExplicitRule::None,
                Some(14),
                Some(30.0),
                Some((GateResult::Pass, None))
            ),
            DEFAULT_ALLOW_TTL
        );
    }

    #[test]
    fn ttl_explicit_is_long() {
        assert_eq!(
            cache_ttl(ExplicitRule::Block, None, None, None),
            EXPLICIT_TTL
        );
    }
}
