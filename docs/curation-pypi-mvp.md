# PyPI Curation MVP — Design & Plan

Status: in progress · Branch: `feat/pypi-curation-mvp` · Fork: `itaywol/artifact-keeper`

## Goal

Gate packages fetched through a **Remote** PyPI repo with configurable curation
policies. A package version is served only if it passes the policies. Focus:
PyPI now; the engine is format-agnostic so other formats follow later.

Driving use case: **min-age cooldown** — block PyPI versions published less than
N days ago (supply-chain protection against freshly-published malicious
releases), plus a **generic webhook** for everything else (reputation, CVE,
license).

## Locked decisions

### Curation engine
| Decision | Choice |
|----------|--------|
| Enforcement timing | **Hybrid** — serve cached verdict; on miss, one synchronous evaluation, cache, then decide |
| Verdict grain | Per `(ecosystem, package, version)`. Cache key `pypi:<name>:<version>` |
| Composition | Explicit rules first (priority-ordered, first match): `allow` = bypass gates, `block` = absolute deny. Otherwise **min-age AND webhook both must pass** |
| Min-age | **Built-in primitive.** AK fetches pypi.org `upload_time` (PEP 691 JSON simple API) and blocks versions younger than `min_age_days` |
| Webhook | **Generic contract.** `POST {ecosystem,package,version,sha256}` → `{decision,reason,ttl}`. Any provider adapts behind it |
| Fail mode | **Per-policy**, default **fail-closed**. Short timeout + retry; reuse cached verdict if present |
| Review flow | **Automated only** — no human review queue |
| Hook points | **Both**: filter blocked versions out of `/simple` (clean pip cooldown UX) **and** hard-block at file download (defense in depth) |
| Verdict cache | **Redis only**, TTL-keyed |

Precedence (first decisive wins):
1. Explicit `curation_rules` (existing table), priority ASC — `block` → deny, `allow` → bypass remaining gates.
2. Min-age gate (if enabled).
3. Webhook gate (if enabled).
4. Default stance (`allow`).

AND semantics: for packages not matched by an explicit rule, **every enabled
gate must return allow**. Any gate block → version blocked.

### Topology
Virtual `pypi` aggregates **Local** (private wheels) + **Remote** (pypi.org,
curation attached). Curation gates only the Remote member. Virtual aggregation
already exists (`api/handlers/pypi.rs:221`).

### Redis TTL semantics
- **Min-age verdict** flips at a known instant → TTL = seconds until the version crosses `min_age_days`. A block expires exactly when the package becomes old enough; an allow is cached long.
- **Webhook verdict** → TTL = returned `ttl`.
- Combined verdict TTL = min of contributing TTLs.

## Data model

New `curation_policies` table — one policy per Remote repo:

| Column | Notes |
|--------|-------|
| `remote_repo_id` | FK repositories, the Remote being gated |
| `enabled` | master switch |
| `min_age_enabled`, `min_age_days` | built-in cooldown gate |
| `webhook_enabled`, `webhook_url`, `webhook_timeout_ms`, `webhook_fail_mode` | webhook gate; fail_mode ∈ (open, closed) default closed |
| `default_action` | stance when no rule/gate decides; default allow |

Explicit allow/block lists reuse the existing `curation_rules` table.

Deferred (not in MVP): durable `curation_verdict_log` (append-only audit). Redis-only
loses block/allow history beyond logs — revisit if debuggability hurts.

## Deployment (EKS, 2 replicas, ALB)

- Stateless HTTP; **no ALB session affinity** for pulls.
- ALB target group health check `/readyz` (DB + migrations); liveness `/livez`.
- **Shared secrets, byte-identical across both pods:** `jwt_secret` (HS256),
  AES-256-GCM credential encryption key. Delivered from **SSM via External
  Secrets Operator** → k8s Secret.
- **S3:** shared bucket via **IRSA** (native — `s3.rs:703` reads
  `AWS_WEB_IDENTITY_TOKEN_FILE`). Proxy cache + artifacts in S3, no pod-local state.
- **RDS Postgres:** migrations run **on boot** (sqlx advisory-lock safe for 2 replicas).
- **Scheduler HA:** wrap GC/lifecycle jobs in `pg_try_advisory_lock` (extend the
  existing stuck-scan pattern `scheduler_service.rs:238`). Inline curation needs
  no background sync job.
- **Search:** OpenSearch **off** (it's optional — `api/mod.rs:109`), PG full-text
  used. Algolia dropped.
- **Packaging:** hand-rolled k8s manifests in `platform-deployments`, ArgoCD-managed.
- **Redis:** add ElastiCache + a redis crate (no Redis in codebase today).

## Implementation plan

- [x] **M1** Migration `135_curation_policies.sql` + `CurationPolicy` model
- [x] **M2** Pure eval core: precedence + min-age predicate (13 tests, no IO)
- [x] **M3** pypi.org `upload_time` fetch (PEP 691 JSON), cached (6 tests)
- [x] **M4** Redis verdict cache (`curation_cache`, get/set + raw, live-Redis tests)
- [x] **M5** Webhook client (`curation_webhook`, pure mapper + retry/fail-mode, axum-server tests)
- [x] **M6** `curation_gate` orchestrator + download-path hard 403 gate (direct Remote + virtual member); AppState wiring + main.rs REDIS_URL init
- [x] **M7** Policy CRUD: `GET/PUT/DELETE /api/v1/curation/policies/{remote_repo_id}` (admin-only)
- [x] **M8** Hand-rolled k8s manifests in `deploy/k8s/` + lean `docker-compose.curation-dev.yml`

Verified via nix-shell: clippy `-D warnings` clean, 42 curation tests pass, fmt clean.

## Residual / follow-ups (not in this MVP)

1. **`/simple` index filtering** — the download gate is the hard enforcement
   (blocks the wheel/sdist with 403). The Remote path in `simple_project`
   passes the upstream index through verbatim, so blocked versions still
   *appear* in `pip index`/resolver listings; pip then 403s on download rather
   than falling back to an older allowed version. Filtering requires parsing +
   re-serializing the upstream HTML/JSON index per the gate. `allowed_versions`
   helper exists in `curation_gate` ready to wire here.
2. **Handler-path integration tests** — the gate logic is unit/integration
   tested; the two `pypi.rs` insertions need axum-test coverage against a
   seeded DB to satisfy the changed-lines coverage gate.
3. **min-age fail mode** — currently always fail-closed (publish time unknown →
   block). Add `min_age_fail_mode` column if a fail-open cooldown is wanted.
4. **OpenAPI docs** for the policy endpoints (skipped `#[utoipa::path]`).
5. **k8s manifests live in `deploy/k8s/` in this fork**, not in
   `platform-deployments` — drop them in as an ArgoCD Application source when
   ready; placeholders (`<ACCOUNT_ID>`, `<REGION>`, `<TAG>`, …) need filling.

Gates per PR (CLAUDE.md): `cargo fmt` + `clippy -D warnings` + unit tests, ≥70%
coverage on changed lines, ≤3% duplication.

---

# Production readiness

## P0 — blockers
- **Build & publish OUR image.** Manifests point at `ghcr.io/artifact-keeper/...`
  (upstream, no curation code). Build the fork backend + push to a registry you
  control; pin the tag in `deploy/k8s/deployment.yaml`.
- **Redis is effectively required.** It's optional/graceful, but with no cache
  every fetch re-runs the webhook + pypi.org JSON call → latency + upstream load.
  Provision ElastiCache, set `REDIS_URL` secret, decide fail behavior if Redis is down.
- **SSRF on webhook_url + upstream fetch.** Admin-set `webhook_url` and the
  upstream `upload_times` GET let the server hit arbitrary URLs. Reuse the
  existing webhook SSRF guard (block loopback/metadata/private IPs) for both.
- **Scheduler HA.** 2 replicas double-run GC/lifecycle (no leader election).
  Wrap those jobs in `pg_try_advisory_lock` before scaling >1 (pattern exists at
  `scheduler_service.rs:238`).
- **Coverage gate.** Handler insertions (`pypi.rs`, `main.rs`) + redis-backed
  cache methods aren't covered by non-live tests. Add axum-test integration
  tests (seeded PG + Redis) and a Redis service to CI Tier-2.

## P1 — should-have
- **`/simple` index filtering.** Today blocked versions still appear in the
  index → pip 403s instead of resolving to an allowed version. Wire
  `allowed_versions` into `simple_project` (HTML + PEP 691 JSON).
- **Webhook request signing.** Sign AK→webhook POSTs (HMAC) so the policy
  service can authenticate the caller. Currently unauthenticated.
- **Metrics + alerts.** Emit curation decision counters (allow/block), webhook
  latency, and **fail-open events** (a fired fail-open = guard was down — alert).
- **min-age fail mode** configurable (column + UI); currently always fail-closed.
- **DB pool sizing** vs RDS `max_connections` (2 replicas × pool ≤ max).

## P2 — nice-to-have
- **Durable verdict/audit log** (Postgres) — Redis-only loses block history on
  eviction; blocked-list only reflects cached+within-TTL. Add if audit needed.
- **OpenAPI** for the new endpoints (skipped `#[utoipa::path]`) → regenerate SDK.
- **Multi-format.** `ecosystem` is hardcoded `"pypi"`; generalize for npm/maven/…
- **Upstream latent bug:** existing `/rules/{id}`, `/packages/{id}` curation
  routes use axum-0.8 brace syntax under axum 0.7 → dead routes (rule CRUD).
  Fix to `:param` (separate from this feature; affects existing curation rules UI).

## Process
- Open PRs: `feat/pypi-curation-mvp` (backend), `feat/curation-ui` (web). Per
  workflow: parent Linear issue + sub-issues, `[ECH-XXXX]` titles.
- Decide: contribute upstream vs maintain private fork.
