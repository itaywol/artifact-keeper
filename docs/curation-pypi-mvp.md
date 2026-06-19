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

- [ ] **M1** Migration `135_curation_policies.sql` + `CurationPolicy` model
- [ ] **M2** Pure eval core: precedence + min-age predicate (unit-tested, no IO)
- [ ] **M3** pypi.org `upload_time` fetch (PEP 691 JSON), cached
- [ ] **M4** Redis verdict cache (client + TTL write-through) behind a trait
- [ ] **M5** Webhook client (timeout, retry, fail-mode)
- [ ] **M6** Wire into PyPI proxy: `/simple` filter + download block (Remote + via Virtual member)
- [ ] **M7** Policy CRUD handlers + API
- [ ] **M8** k8s manifests (Deployment×2, Service, ALB Ingress, ServiceAccount/IRSA, ExternalSecret) in platform-deployments

Gates per PR (CLAUDE.md): `cargo fmt` + `clippy -D warnings` + unit tests, ≥70%
coverage on changed lines, ≤3% duplication.
