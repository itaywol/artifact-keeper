//! Pure curation evaluation core — no IO.
//!
//! The caller resolves all facts (explicit rule match, package publish age,
//! webhook response) and feeds them here. This module encodes only the
//! precedence and AND semantics so the decision logic is fully unit-testable.
//!
//! Precedence (first decisive wins):
//!   1. Explicit rule  — `Block` denies, `Allow` bypasses remaining gates.
//!   2. Min-age gate    (if enabled).
//!   3. Webhook gate    (if enabled).
//!   4. Default stance  (when no rule and no gate decides).
//!
//! For a package not matched by an explicit rule, every *enabled* gate must
//! pass (logical AND). Any gate that blocks → the version is blocked.

/// Final curation decision for a `(package, version)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Verdict {
    Allow,
    Block,
}

/// Outcome of an explicit `curation_rules` lookup for this package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplicitRule {
    /// No explicit rule matched.
    None,
    /// An explicit allow rule matched — bypass all gates.
    Allow,
    /// An explicit block rule matched — absolute deny.
    Block,
}

/// Result of a single gate after its fact was (or could not be) resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateResult {
    /// Fact resolved and the gate is satisfied.
    Pass,
    /// Fact resolved and the gate rejects the package.
    Block,
    /// The fact could not be obtained (timeout, fetch error, unknown publish
    /// time). The gate's fail mode decides what this becomes.
    Unavailable,
}

/// A gate fed into [`evaluate`]. `None` means the gate is disabled.
#[derive(Debug, Clone, Copy)]
pub struct Gate {
    pub result: GateResult,
    /// When `true`, an `Unavailable` result blocks (fail-closed); otherwise it
    /// passes (fail-open).
    pub fail_closed: bool,
}

impl Gate {
    /// Collapse the gate (applying its fail mode) into a pass/block boolean.
    /// Returns `true` if the gate is satisfied.
    fn passes(&self) -> bool {
        match self.result {
            GateResult::Pass => true,
            GateResult::Block => false,
            GateResult::Unavailable => !self.fail_closed,
        }
    }
}

/// A resolved evaluation: the verdict plus a human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalResult {
    pub verdict: Verdict,
    pub reason: String,
}

/// Evaluate one `(package, version)` against the resolved facts.
///
/// * `explicit` — outcome of the explicit-rule lookup.
/// * `min_age` / `webhook` — `Some(gate)` if enabled, `None` if disabled.
/// * `default_action` — stance when nothing else decides.
pub fn evaluate(
    explicit: ExplicitRule,
    min_age: Option<Gate>,
    webhook: Option<Gate>,
    default_action: Verdict,
) -> EvalResult {
    // 1. Explicit rules short-circuit.
    match explicit {
        ExplicitRule::Block => {
            return EvalResult {
                verdict: Verdict::Block,
                reason: "explicit block rule".to_string(),
            };
        }
        ExplicitRule::Allow => {
            return EvalResult {
                verdict: Verdict::Allow,
                reason: "explicit allow rule (bypass)".to_string(),
            };
        }
        ExplicitRule::None => {}
    }

    // 2 + 3. Gates — AND semantics. First enabled gate that blocks decides.
    let mut any_gate = false;
    if let Some(g) = min_age {
        any_gate = true;
        if !g.passes() {
            return EvalResult {
                verdict: Verdict::Block,
                reason: min_age_block_reason(g),
            };
        }
    }
    if let Some(g) = webhook {
        any_gate = true;
        if !g.passes() {
            return EvalResult {
                verdict: Verdict::Block,
                reason: webhook_block_reason(g),
            };
        }
    }

    // 4. All enabled gates passed → allow. No gate enabled → default stance.
    if any_gate {
        EvalResult {
            verdict: Verdict::Allow,
            reason: "all gates passed".to_string(),
        }
    } else {
        EvalResult {
            verdict: default_action,
            reason: "default stance".to_string(),
        }
    }
}

fn min_age_block_reason(g: Gate) -> String {
    match g.result {
        GateResult::Unavailable => "min-age: publish time unknown (fail-closed)".to_string(),
        _ => "min-age: version too new".to_string(),
    }
}

fn webhook_block_reason(g: Gate) -> String {
    match g.result {
        GateResult::Unavailable => "webhook: unavailable (fail-closed)".to_string(),
        _ => "webhook: blocked".to_string(),
    }
}

/// Min-age gate from a known publish age. `None` age means the publish time
/// could not be determined → `Unavailable` (fail mode applies downstream).
pub fn min_age_gate(published_age_days: Option<f64>, min_age_days: i64) -> GateResult {
    match published_age_days {
        Some(age) if age >= min_age_days as f64 => GateResult::Pass,
        Some(_) => GateResult::Block,
        None => GateResult::Unavailable,
    }
}

/// Seconds until a too-new version crosses `min_age_days`, used as the Redis
/// TTL for a min-age block so the block expires exactly when the package
/// becomes old enough. Always at least 1s.
pub fn min_age_block_ttl_secs(published_age_days: f64, min_age_days: i64) -> i64 {
    let remaining_days = min_age_days as f64 - published_age_days;
    ((remaining_days * 86_400.0).ceil() as i64).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(result: GateResult, fail_closed: bool) -> Gate {
        Gate {
            result,
            fail_closed,
        }
    }

    // --- explicit precedence ---

    #[test]
    fn explicit_block_denies_even_if_gates_pass() {
        let r = evaluate(
            ExplicitRule::Block,
            Some(gate(GateResult::Pass, true)),
            Some(gate(GateResult::Pass, true)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
        assert_eq!(r.reason, "explicit block rule");
    }

    #[test]
    fn explicit_allow_bypasses_a_blocking_gate() {
        let r = evaluate(
            ExplicitRule::Allow,
            Some(gate(GateResult::Block, true)),
            Some(gate(GateResult::Block, true)),
            Verdict::Block,
        );
        assert_eq!(r.verdict, Verdict::Allow);
        assert!(r.reason.contains("bypass"));
    }

    // --- AND semantics ---

    #[test]
    fn both_gates_must_pass() {
        let r = evaluate(
            ExplicitRule::None,
            Some(gate(GateResult::Pass, true)),
            Some(gate(GateResult::Pass, true)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Allow);
        assert_eq!(r.reason, "all gates passed");
    }

    #[test]
    fn min_age_block_wins_before_webhook() {
        let r = evaluate(
            ExplicitRule::None,
            Some(gate(GateResult::Block, true)),
            Some(gate(GateResult::Pass, true)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
        assert_eq!(r.reason, "min-age: version too new");
    }

    #[test]
    fn webhook_block_when_min_age_passes() {
        let r = evaluate(
            ExplicitRule::None,
            Some(gate(GateResult::Pass, true)),
            Some(gate(GateResult::Block, true)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
        assert_eq!(r.reason, "webhook: blocked");
    }

    // --- fail modes ---

    #[test]
    fn unavailable_fail_closed_blocks() {
        let r = evaluate(
            ExplicitRule::None,
            None,
            Some(gate(GateResult::Unavailable, true)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Block);
        assert!(r.reason.contains("fail-closed"));
    }

    #[test]
    fn unavailable_fail_open_passes() {
        let r = evaluate(
            ExplicitRule::None,
            None,
            Some(gate(GateResult::Unavailable, false)),
            Verdict::Allow,
        );
        assert_eq!(r.verdict, Verdict::Allow);
    }

    // --- default stance ---

    #[test]
    fn no_gates_uses_default_allow() {
        let r = evaluate(ExplicitRule::None, None, None, Verdict::Allow);
        assert_eq!(r.verdict, Verdict::Allow);
        assert_eq!(r.reason, "default stance");
    }

    #[test]
    fn no_gates_uses_default_block() {
        let r = evaluate(ExplicitRule::None, None, None, Verdict::Block);
        assert_eq!(r.verdict, Verdict::Block);
    }

    // --- min_age_gate ---

    #[test]
    fn min_age_gate_boundaries() {
        assert_eq!(min_age_gate(Some(14.0), 14), GateResult::Pass); // exactly old enough
        assert_eq!(min_age_gate(Some(13.99), 14), GateResult::Block);
        assert_eq!(min_age_gate(Some(0.0), 14), GateResult::Block);
        assert_eq!(min_age_gate(Some(100.0), 14), GateResult::Pass);
        assert_eq!(min_age_gate(None, 14), GateResult::Unavailable);
    }

    // --- TTL ---

    #[test]
    fn min_age_ttl_counts_down_to_threshold() {
        // 10 days old, need 14 → 4 days remain.
        assert_eq!(min_age_block_ttl_secs(10.0, 14), 4 * 86_400);
    }

    #[test]
    fn min_age_ttl_never_below_one() {
        // Already at/over threshold should not produce 0 or negative.
        assert_eq!(min_age_block_ttl_secs(14.0, 14), 1);
        assert_eq!(min_age_block_ttl_secs(20.0, 14), 1);
    }

    #[test]
    fn min_age_ttl_rounds_up_partial_day() {
        // 13.5 days old, need 14 → 0.5 day = 43200s.
        assert_eq!(min_age_block_ttl_secs(13.5, 14), 43_200);
    }
}
