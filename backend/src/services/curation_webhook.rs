//! Generic curation webhook client.
//!
//! Posts `{ecosystem, package, version, sha256}` to a policy-configured URL and
//! maps the `{decision, reason, ttl}` response into a [`GateResult`] plus an
//! optional cache TTL. Transport errors, non-200, or bad bodies map to
//! `Unavailable` so the policy's fail mode decides downstream.
//!
//! Curation is automated-only (no human review queue), so a `review` decision
//! is treated as a block.

use std::time::Duration;

use serde::Serialize;

use crate::services::curation_eval::GateResult;

/// Request body sent to the webhook.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload<'a> {
    pub ecosystem: &'a str,
    pub package: &'a str,
    pub version: &'a str,
    pub sha256: Option<&'a str>,
}

/// Result of a webhook evaluation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookVerdict {
    pub gate: GateResult,
    /// Cache TTL hint from the webhook, in seconds.
    pub ttl_secs: Option<i64>,
    pub reason: String,
}

fn unavailable(reason: impl Into<String>) -> WebhookVerdict {
    WebhookVerdict {
        gate: GateResult::Unavailable,
        ttl_secs: None,
        reason: reason.into(),
    }
}

/// Map an HTTP `(status, body)` into a verdict. Pure — all decision logic lives
/// here so it is fully unit-testable.
pub fn map_response(status: u16, body: &str) -> WebhookVerdict {
    if status != 200 {
        return unavailable(format!("webhook: http {status}"));
    }
    let v: serde_json::Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return unavailable("webhook: malformed json"),
    };
    let decision = v.get("decision").and_then(|d| d.as_str()).unwrap_or("");
    let reason = v
        .get("reason")
        .and_then(|r| r.as_str())
        .unwrap_or("")
        .to_string();
    let ttl_secs = v.get("ttl").and_then(|t| t.as_i64());

    match decision {
        "allow" => WebhookVerdict {
            gate: GateResult::Pass,
            ttl_secs,
            reason: if reason.is_empty() {
                "webhook: allow".to_string()
            } else {
                reason
            },
        },
        "block" => WebhookVerdict {
            gate: GateResult::Block,
            ttl_secs,
            reason: if reason.is_empty() {
                "webhook: block".to_string()
            } else {
                reason
            },
        },
        // Automated-only: a review verdict has no human queue, so block.
        "review" => WebhookVerdict {
            gate: GateResult::Block,
            ttl_secs,
            reason: "webhook: review treated as block (automated-only)".to_string(),
        },
        _ => unavailable("webhook: unknown decision"),
    }
}

/// Call the webhook once. Transport error / timeout → `Unavailable`.
async fn call_once(
    client: &reqwest::Client,
    url: &str,
    timeout: Duration,
    payload: &WebhookPayload<'_>,
) -> WebhookVerdict {
    match client.post(url).timeout(timeout).json(payload).send().await {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            map_response(status, &body)
        }
        Err(_) => unavailable("webhook: transport error/timeout"),
    }
}

/// Call the webhook with one retry when the first attempt is `Unavailable`
/// (covers a transient timeout / flaky endpoint).
pub async fn call(
    client: &reqwest::Client,
    url: &str,
    timeout_ms: u64,
    payload: &WebhookPayload<'_>,
) -> WebhookVerdict {
    let timeout = Duration::from_millis(timeout_ms);
    let first = call_once(client, url, timeout, payload).await;
    if first.gate == GateResult::Unavailable {
        return call_once(client, url, timeout, payload).await;
    }
    first
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- pure mapper ---

    #[test]
    fn allow_maps_to_pass_with_ttl() {
        let v = map_response(200, r#"{"decision":"allow","reason":"ok","ttl":120}"#);
        assert_eq!(v.gate, GateResult::Pass);
        assert_eq!(v.ttl_secs, Some(120));
        assert_eq!(v.reason, "ok");
    }

    #[test]
    fn block_maps_to_block() {
        let v = map_response(200, r#"{"decision":"block","reason":"malware"}"#);
        assert_eq!(v.gate, GateResult::Block);
        assert_eq!(v.ttl_secs, None);
        assert_eq!(v.reason, "malware");
    }

    #[test]
    fn review_treated_as_block() {
        let v = map_response(200, r#"{"decision":"review"}"#);
        assert_eq!(v.gate, GateResult::Block);
        assert!(v.reason.contains("automated-only"));
    }

    #[test]
    fn non_200_is_unavailable() {
        assert_eq!(map_response(503, "").gate, GateResult::Unavailable);
        assert_eq!(map_response(404, "{}").gate, GateResult::Unavailable);
    }

    #[test]
    fn bad_json_is_unavailable() {
        assert_eq!(map_response(200, "not json").gate, GateResult::Unavailable);
    }

    #[test]
    fn unknown_decision_is_unavailable() {
        let v = map_response(200, r#"{"decision":"maybe"}"#);
        assert_eq!(v.gate, GateResult::Unavailable);
    }

    #[test]
    fn empty_reason_gets_default() {
        let v = map_response(200, r#"{"decision":"allow"}"#);
        assert_eq!(v.reason, "webhook: allow");
    }

    // --- async call against a real local server ---

    async fn spawn_server(response: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{routing::post, Router};
        let app = Router::new().route("/v", post(move || async move { response }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v"), handle)
    }

    #[tokio::test]
    async fn call_hits_server_and_maps_allow() {
        let (url, h) = spawn_server(r#"{"decision":"allow","ttl":60}"#).await;
        let client = reqwest::Client::new();
        let payload = WebhookPayload {
            ecosystem: "pypi",
            package: "requests",
            version: "2.0.0",
            sha256: None,
        };
        let v = call(&client, &url, 2000, &payload).await;
        assert_eq!(v.gate, GateResult::Pass);
        assert_eq!(v.ttl_secs, Some(60));
        h.abort();
    }

    #[tokio::test]
    async fn call_on_dead_endpoint_is_unavailable() {
        // Port 1 is not listening -> transport error on both attempts.
        let client = reqwest::Client::new();
        let payload = WebhookPayload {
            ecosystem: "pypi",
            package: "requests",
            version: "2.0.0",
            sha256: None,
        };
        let v = call(&client, "http://127.0.0.1:1/v", 500, &payload).await;
        assert_eq!(v.gate, GateResult::Unavailable);
    }
}
