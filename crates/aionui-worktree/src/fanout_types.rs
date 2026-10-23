//! E3-4 fan-out request / response types (`/api/fanout-groups*`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::types::{DiffStatResponse, WorktreeErrorInfo, WorktreeResponse};

pub const GROUP_PREPARING: &str = "preparing";
pub const GROUP_RUNNING: &str = "running";
pub const GROUP_COMPARING: &str = "comparing";
pub const GROUP_ADOPTED: &str = "adopted";
pub const GROUP_DISCARDED: &str = "discarded";
pub const GROUP_CANCELLED: &str = "cancelled";
pub const GROUP_FAILED: &str = "failed";

/// Terminal group states: nothing runs and no worktree is touched any more.
pub const GROUP_TERMINAL: [&str; 4] = [GROUP_ADOPTED, GROUP_DISCARDED, GROUP_CANCELLED, GROUP_FAILED];

/// Slots per group (inclusive).
pub const MIN_SLOTS: usize = 2;
pub const MAX_SLOTS: usize = 5;
/// `max_parallel` when the request omits it (capped at the slot count).
pub const DEFAULT_MAX_PARALLEL: u8 = 3;
/// Slot labels, in order.
pub const SLOT_LABELS: [&str; MAX_SLOTS] = ["A", "B", "C", "D", "E"];
/// `error_code` on a ready slot worktree whose conversation never started.
pub const SLOT_START_FAILED: &str = "SLOT_START_FAILED";

/// One slot: an existing assistant (its own backend, permission mode, MCP and
/// skills) and optionally a model. Unknown fields are rejected, so a slot can
/// never carry inline MCP servers or a permission override.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FanoutSlotRequest {
    pub assistant_id: String,
    /// A model id string, or (aionrs) a `{provider_id, model}` object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    /// Accepted for forward compatibility and ignored: every slot runs with
    /// the assistant's own permission mode.
    #[serde(default, skip_serializing)]
    pub permission: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateFanoutGroupRequest {
    pub repo_path: String,
    #[serde(default)]
    pub base_ref: Option<String>,
    pub prompt: String,
    /// Passed through to every slot's first message.
    #[serde(default)]
    pub files: Vec<Value>,
    #[serde(default)]
    pub sessions: Vec<Value>,
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub max_parallel: Option<u8>,
    #[serde(default)]
    pub title: Option<String>,
    pub slots: Vec<FanoutSlotRequest>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListFanoutGroupsQuery {
    #[serde(default)]
    pub task_session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdoptStrategy {
    /// Keep the winner's branch; merge nothing.
    #[default]
    BranchOnly,
    /// `git merge --squash` into the target branch, one commit.
    Squash,
    /// `git merge --no-ff` into the target branch.
    NoFf,
    /// Cherry-pick the winner's commits onto the target branch.
    CherryPick,
}

impl AdoptStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BranchOnly => "branch_only",
            Self::Squash => "squash",
            Self::NoFf => "no_ff",
            Self::CherryPick => "cherry_pick",
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
pub struct AdoptCleanup {
    /// Remove the other slots' worktrees (directories are force-removed).
    #[serde(default = "default_true")]
    pub remove_others: bool,
    /// Keep the other slots' branches even when they have no unmerged work.
    #[serde(default)]
    pub keep_branches: bool,
}

impl Default for AdoptCleanup {
    fn default() -> Self {
        Self {
            remove_others: true,
            keep_branches: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdoptFanoutRequest {
    pub slot: String,
    #[serde(default)]
    pub strategy: AdoptStrategy,
    /// Branch to merge into; must be the branch checked out in the source
    /// repository (default: its current branch).
    #[serde(default)]
    pub target_branch: Option<String>,
    /// Message for the snapshot commit of uncommitted winner changes, and for
    /// the squash / merge commit.
    #[serde(default)]
    pub snapshot_message: Option<String>,
    #[serde(default)]
    pub cleanup: AdoptCleanup,
}

/// Persisted in `fanout_groups.adopt_result`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct AdoptResult {
    pub slot: String,
    pub strategy: AdoptStrategy,
    pub branch: String,
    pub branch_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_commit: Option<String>,
    /// Worktree ids removed by cleanup.
    #[serde(default)]
    pub removed_worktrees: Vec<String>,
    /// Other slots' branches kept because they hold unmerged commits (or
    /// `keep_branches` was set).
    #[serde(default)]
    pub preserved_branches: Vec<String>,
    /// Cleanup that did not finish (the worktree is left for the manager).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cleanup_errors: Vec<SlotCleanupError>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SlotCleanupError {
    pub worktree_id: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FanoutSlotResponse {
    pub slot: String,
    pub worktree_id: String,
    /// `preparing` | `starting` | `running` | `finished` | `failed` |
    /// `cancelled` | `adopted` | `removed`.
    pub status: String,
    pub worktree_status: String,
    pub assistant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    pub branch: String,
    pub path: String,
    /// Set once the worktree is removed with its branch kept.
    pub branch_preserved: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<WorktreeErrorInfo>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FanoutGroupResponse {
    pub id: String,
    pub status: String,
    pub title: String,
    pub source_repo_path: String,
    pub base_ref: String,
    pub base_commit: String,
    pub prompt: String,
    pub max_parallel: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub winner_slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adopt_result: Option<AdoptResult>,
    pub slots: Vec<FanoutSlotResponse>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FanoutCompareSlot {
    pub slot: String,
    pub status: String,
    pub worktree: WorktreeResponse,
    pub assistant_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diff: Option<DiffStatResponse>,
    /// The slot's last assistant reply, truncated.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    /// Token usage / test evidence (not collected yet; always null).
    pub usage: Option<Value>,
    pub tests: Option<Value>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FanoutFileCell {
    pub status: String,
    pub additions: u32,
    pub deletions: u32,
}

/// One changed path and what each slot did to it (absent: untouched).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct FanoutFileRow {
    pub path: String,
    pub slots: std::collections::BTreeMap<String, FanoutFileCell>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FanoutCompareResponse {
    pub group: FanoutGroupResponse,
    pub slots: Vec<FanoutCompareSlot>,
    pub files: Vec<FanoutFileRow>,
}
