//! Curation models for package vetting through staging repos.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

/// An explicit allow/block rule for package curation.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CurationRule {
    pub id: Uuid,
    pub staging_repo_id: Option<Uuid>,
    pub package_pattern: String,
    pub version_constraint: String,
    pub architecture: String,
    pub action: String,
    pub priority: i32,
    pub reason: String,
    pub enabled: bool,
    pub created_by: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A package tracked in the curation staging catalog.
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CurationPackage {
    pub id: Uuid,
    pub staging_repo_id: Uuid,
    pub remote_repo_id: Uuid,
    pub format: String,
    pub package_name: String,
    pub version: String,
    pub release: Option<String>,
    pub architecture: Option<String>,
    pub checksum_sha256: Option<String>,
    pub upstream_path: String,
    pub status: String,
    pub evaluated_at: Option<DateTime<Utc>>,
    pub evaluated_by: Option<Uuid>,
    pub evaluation_reason: Option<String>,
    pub rule_id: Option<Uuid>,
    pub metadata: serde_json::Value,
    pub first_seen_at: DateTime<Utc>,
    pub upstream_updated_at: Option<DateTime<Utc>>,
}

/// Inline curation policy attached to a Remote repository. Drives the min-age
/// and webhook gates evaluated on the fetch path. Explicit allow/block lists
/// live in [`CurationRule`].
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct CurationPolicy {
    pub id: Uuid,
    pub remote_repo_id: Uuid,
    pub enabled: bool,

    pub min_age_enabled: bool,
    pub min_age_days: Option<i32>,

    pub webhook_enabled: bool,
    pub webhook_url: Option<String>,
    pub webhook_timeout_ms: i32,
    /// `"open"` or `"closed"`.
    pub webhook_fail_mode: String,

    /// `"allow"` or `"block"`.
    pub default_action: String,

    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
