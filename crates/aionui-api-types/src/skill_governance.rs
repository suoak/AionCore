//! E10-2 skill source / version / review wire types
//! (`/api/skills/sources/import`, `/api/skills/id/{id}/{refresh,source,review,visibility}`,
//! `/api/skills/reviews/pending`, E10-3 `/versions/{a}/diff/{b}` and
//! `/versions/{n}/restore`). snake_case like the other skill endpoints.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ImportSourceKind {
    Url,
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFromSourceRequest {
    pub kind: ImportSourceKind,
    /// https URL (url / git) or absolute local directory (local).
    pub locator: String,
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>,
    #[serde(default)]
    pub subpath: Option<String>,
    /// `claude` | `codex` for local snapshots.
    #[serde(default)]
    pub origin_app: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSourceInfo {
    pub kind: String,
    pub locator: String,
    #[serde(rename = "ref")]
    pub git_ref: Option<String>,
    pub subpath: Option<String>,
    pub origin_app: Option<String>,
    pub trust: String,
    pub last_fetched_at: Option<i64>,
    pub last_commit: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillVersionInfo {
    pub version_no: i64,
    pub content_hash: String,
    pub has_scripts: bool,
    pub source_ref: Option<String>,
    pub created_by: String,
    pub created_at: i64,
    /// Raw manifest JSON: `{ "files": [{path,size,sha256,script}], "scripts": [path] }`.
    pub manifest: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillGovernanceInfo {
    pub skill_id: String,
    pub name: String,
    pub visibility: String,
    pub team_id: Option<String>,
    pub review_status: String,
}

/// Result of one import / refresh of one skill.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSourceSyncResult {
    pub skill: SkillGovernanceInfo,
    pub source: SkillSourceInfo,
    pub version: SkillVersionInfo,
    /// `false` when the content hash matched the latest version (no-op).
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImportFromSourceResponse {
    pub imported: Vec<SkillSourceSyncResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSourceDetailResponse {
    pub skill: SkillGovernanceInfo,
    pub source: Option<SkillSourceInfo>,
    /// Newest first.
    pub versions: Vec<SkillVersionInfo>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReviewSkillRequest {
    pub decision: ReviewDecision,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetVisibilityRequest {
    /// `private` | `team`
    pub visibility: String,
    #[serde(default)]
    pub team_id: Option<String>,
}

/// E10-5: one skill in the installation team library.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamSkillInfo {
    pub skill_id: String,
    pub name: String,
    pub description: String,
    pub owner_user_id: String,
    /// `none` | `pending` | `approved` | `rejected`
    pub review_status: String,
    /// Published by the caller.
    pub mine: bool,
    pub updated_at: i64,
}

/// E10-5: `GET /api/skills/team`. `enabled` is false outside multi-user role
/// modes; the UI then hides the team entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TeamSkillLibraryResponse {
    pub enabled: bool,
    pub team_id: Option<String>,
    /// Owner / admin: may publish, unpublish and review team skills.
    pub can_manage: bool,
    pub skills: Vec<TeamSkillInfo>,
}

/// E10-3: one changed file between two versions. Unchanged files are omitted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SkillFileDiff {
    pub path: String,
    /// `added` | `removed` | `modified`
    pub status: String,
    /// Binary (or non-UTF-8) files are only listed, never diffed.
    pub binary: bool,
    /// The file counts as a script in either version (D10-7).
    pub script: bool,
    /// Unified diff (3 lines of context) for text files; `None` for binary or
    /// oversized files.
    pub diff: Option<String>,
    /// The text was over the diff size cap, so only the change is listed.
    pub too_large: bool,
}

/// `GET /api/skills/id/{skill_id}/versions/{a}/diff/{b}`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillVersionDiffResponse {
    pub skill_id: String,
    pub from_version: i64,
    pub to_version: i64,
    pub files: Vec<SkillFileDiff>,
}

/// `POST /api/skills/id/{skill_id}/versions/{n}/restore`
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRestoreResponse {
    pub skill: SkillGovernanceInfo,
    /// The version the live skill now matches (a new one unless the restored
    /// content equals the latest version).
    pub version: SkillVersionInfo,
    pub restored_from: i64,
    /// `false` when the restored content equals the latest version (no-op).
    pub changed: bool,
}
