//! Skill provenance, append-only versions and the review gate (M8 E10-1).

use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{SkillGovernanceRow, SkillSourceRow, SkillVersionRow};

/// Insert-or-replace the single source record of a skill.
#[derive(Debug, Clone)]
pub struct UpsertSkillSourceParams<'a> {
    /// Used only when the row is created; an existing row keeps its id.
    pub id: &'a str,
    pub user_id: Option<&'a str>,
    pub skill_id: &'a str,
    pub kind: &'a str,
    pub locator: &'a str,
    pub git_ref: Option<&'a str>,
    pub subpath: Option<&'a str>,
    pub origin_app: Option<&'a str>,
    pub trust: &'a str,
    pub now: TimestampMs,
}

/// A new snapshot of a skill. `version_no` is assigned by the repository.
#[derive(Clone)]
pub struct AppendSkillVersionParams<'a> {
    pub id: &'a str,
    pub skill_id: &'a str,
    pub content_hash: &'a str,
    /// JSON document (file list, sizes, script paths).
    pub manifest: &'a str,
    pub has_scripts: bool,
    pub source_ref: Option<&'a str>,
    pub created_by: &'a str,
    pub now: TimestampMs,
    /// Runs inside the insert transaction once `version_no` is known, before
    /// commit. An `Err` aborts and rolls the row back, so callers can put the
    /// version files in place first and never leave a row without files.
    /// Not called when the content equals the latest version.
    pub before_commit: Option<&'a (dyn Fn(i64) -> Result<(), String> + Send + Sync)>,
}

#[derive(Debug, Clone)]
pub struct AppendSkillVersionOutcome {
    pub version: SkillVersionRow,
    /// `false` when the content hash equals the latest version; nothing was
    /// written and `version` is that latest row.
    pub created: bool,
    /// `skills.review_status` after the call.
    pub review_status: String,
}

#[async_trait::async_trait]
pub trait ISkillSourceRepository: Send + Sync {
    async fn upsert_source(&self, params: UpsertSkillSourceParams<'_>) -> Result<SkillSourceRow, DbError>;
    async fn find_source(&self, skill_id: &str) -> Result<Option<SkillSourceRow>, DbError>;
    /// Stamp a successful fetch (url/git refresh).
    async fn record_fetch(
        &self,
        skill_id: &str,
        fetched_at: TimestampMs,
        last_commit: Option<&str>,
    ) -> Result<SkillSourceRow, DbError>;

    /// Append a version. A version that ships scripts always puts the skill
    /// back to `review_status = 'pending'` in the same transaction (D10-7).
    async fn append_version(&self, params: AppendSkillVersionParams<'_>) -> Result<AppendSkillVersionOutcome, DbError>;
    /// Newest first.
    async fn list_versions(&self, skill_id: &str) -> Result<Vec<SkillVersionRow>, DbError>;
    async fn find_version(&self, skill_id: &str, version_no: i64) -> Result<Option<SkillVersionRow>, DbError>;
    async fn latest_version(&self, skill_id: &str) -> Result<Option<SkillVersionRow>, DbError>;

    async fn governance(&self, skill_id: &str) -> Result<Option<SkillGovernanceRow>, DbError>;
    /// Skills owned by `user_id` waiting for review (`review_status = 'pending'`), newest first.
    async fn list_pending_reviews_for_user(&self, user_id: &str) -> Result<Vec<SkillGovernanceRow>, DbError>;
    /// Set the review state. Who may approve (owner vs team admin, D10-3) is
    /// decided by the service; this layer only validates the value.
    async fn set_review_status(&self, skill_id: &str, status: &str, now: TimestampMs) -> Result<(), DbError>;
    /// `team` requires a `team_id`; `private` clears it.
    async fn set_visibility(
        &self,
        skill_id: &str,
        visibility: &str,
        team_id: Option<&str>,
        now: TimestampMs,
    ) -> Result<(), DbError>;
}
