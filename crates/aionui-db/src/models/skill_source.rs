//! Skill provenance and append-only version rows (M8 E10-1, migration 073).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

pub const SKILL_SOURCE_KINDS: &[&str] = &["url", "git", "local_snapshot", "registry"];
pub const SKILL_ORIGIN_APPS: &[&str] = &["claude", "codex"];
pub const SKILL_TRUST_LEVELS: &[&str] = &["trusted", "untrusted"];
pub const SKILL_VERSION_CREATORS: &[&str] = &["user", "import", "evolution"];
pub const SKILL_REVIEW_STATUSES: &[&str] = &["none", "pending", "approved", "rejected"];
pub const SKILL_VISIBILITIES: &[&str] = &["private", "team"];

/// Row mapping for `skill_sources`.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillSourceRow {
    pub id: String,
    pub user_id: Option<String>,
    pub skill_id: String,
    pub kind: String,
    pub locator: String,
    #[sqlx(rename = "ref")]
    pub git_ref: Option<String>,
    pub subpath: Option<String>,
    pub origin_app: Option<String>,
    pub last_fetched_at: Option<TimestampMs>,
    pub last_commit: Option<String>,
    pub trust: String,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// Row mapping for `skill_versions`. Rows are immutable once written.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillVersionRow {
    pub id: String,
    pub skill_id: String,
    pub version_no: i64,
    pub content_hash: String,
    pub manifest: String,
    pub has_scripts: bool,
    pub source_ref: Option<String>,
    pub created_by: String,
    pub created_at: TimestampMs,
}

/// Review-gate state of a skill (`skills.review_status`, visibility, team).
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SkillGovernanceRow {
    pub id: String,
    pub user_id: Option<String>,
    pub name: String,
    pub visibility: String,
    pub team_id: Option<String>,
    pub review_status: String,
}

/// D10-7 hard rule: evolution may never auto-apply to a skill that ships
/// scripts, whatever the gate mode or score says.
pub fn evolution_auto_apply_allowed(has_scripts: bool) -> bool {
    !has_scripts
}

/// Only these review states let a skill be enabled or injected into a
/// session; `pending` and `rejected` are hidden from the resolver.
pub fn review_status_is_live(status: &str) -> bool {
    matches!(status, "none" | "approved")
}
