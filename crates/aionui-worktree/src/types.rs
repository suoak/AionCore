//! HTTP DTOs for `/api/worktrees*`.

use serde::{Deserialize, Serialize};

use crate::store::WorktreeRow;

pub const STATUS_CREATING: &str = "creating";
pub const STATUS_READY: &str = "ready";
pub const STATUS_FAILED: &str = "failed";
pub const STATUS_UNBOUND: &str = "unbound";
pub const STATUS_REMOVING: &str = "removing";
pub const STATUS_REMOVED: &str = "removed";
pub const STATUS_ORPHANED: &str = "orphaned";

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateWorktreeRequest {
    pub repo_path: String,
    /// Empty / omitted: `origin/HEAD`, falling back to the current `HEAD`.
    #[serde(default)]
    pub base_ref: Option<String>,
    /// Omitted: `wm/<id8>-<slug(title)>`.
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub task_session_id: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListWorktreesQuery {
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub include_removed: Option<bool>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BranchPolicy {
    /// Delete the branch only when it has no unmerged commits.
    #[default]
    Auto,
    /// Always keep the branch.
    Keep,
    /// Always delete the branch (`git branch -D`).
    Force,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RemoveWorktreeQuery {
    #[serde(default)]
    pub branch: BranchPolicy,
    /// Remove even with uncommitted / untracked changes.
    #[serde(default)]
    pub force_dir: bool,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorktreeErrorInfo {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WorktreeResponse {
    pub id: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorktreeErrorInfo>,
    pub source_repo_path: String,
    pub repo_common_dir: String,
    pub path: String,
    pub branch: String,
    pub base_ref: String,
    pub base_commit: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fanout_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<i64>,
    pub branch_preserved: bool,
    /// Commits on the branch not in `base_commit` (detail view only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    /// Commits in `base_commit` not on the branch (detail view only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<i64>,
}

impl From<WorktreeRow> for WorktreeResponse {
    fn from(row: WorktreeRow) -> Self {
        let error = row.error_code.clone().map(|code| WorktreeErrorInfo {
            code,
            message: row.error_message.clone().unwrap_or_default(),
        });
        Self {
            id: row.id,
            status: row.status,
            phase: row.phase,
            error,
            source_repo_path: row.source_repo_path,
            repo_common_dir: row.repo_common_dir,
            path: row.path,
            branch: row.branch,
            base_ref: row.base_ref,
            base_commit: row.base_commit,
            fanout_group_id: row.fanout_group_id,
            slot: row.slot,
            task_session_id: row.task_session_id,
            conversation_id: row.conversation_id,
            size_bytes: row.size_bytes,
            branch_preserved: row.branch_preserved != 0,
            ahead: None,
            behind: None,
            created_at: row.created_at,
            updated_at: row.updated_at,
            removed_at: row.removed_at,
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct GitCapabilityResponse {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub min_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorktreeErrorInfo>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct PruneResponse {
    /// Records whose directory no longer existed and were marked removed.
    pub pruned: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct RecoverReport {
    pub interrupted_creates: usize,
    pub interrupted_removes: usize,
    pub stale_admin_entries: usize,
}

/// One changed file relative to the worktree's `base_commit`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffStatFile {
    /// Path relative to the worktree root, `/`-separated.
    pub path: String,
    /// Previous path for a rename.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// `added` | `modified` | `deleted` | `renamed` | `typechange` | `untracked`.
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
    pub binary: bool,
}

/// `GET /api/worktrees/{id}/diff-stat`: committed, staged, unstaged and
/// untracked changes against `base_commit`, in one list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffStatResponse {
    pub worktree_id: String,
    pub base_commit: String,
    pub files: Vec<DiffStatFile>,
    pub files_changed: u32,
    pub additions: u64,
    pub deletions: u64,
    /// More than [`crate::diff::MAX_DIFF_STAT_FILES`] files changed; `files`
    /// holds the first ones only (totals still cover every file).
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiffQuery {
    /// Worktree-relative file path.
    #[serde(default)]
    pub path: Option<String>,
}

/// `GET /api/worktrees/{id}/diff?path=`: unified diff of one file against
/// `base_commit` (untracked files diff against `/dev/null`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiffResponse {
    pub worktree_id: String,
    pub base_commit: String,
    pub path: String,
    /// `unchanged` when the file does not differ from the base.
    pub status: String,
    pub binary: bool,
    /// Unified diff text; empty for binary or unchanged files.
    pub patch: String,
    /// The patch exceeded [`crate::diff::MAX_PATCH_BYTES`] and was cut.
    pub truncated: bool,
}
