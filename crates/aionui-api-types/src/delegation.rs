//! 3.0 C7: `@assistant` delegation.
//!
//! The user picks assistants in a conversation's `@` popover; that grants the
//! conversation's agent the right to hand them a task. The agent starts a
//! delegation (non-blocking) and reads its result later by handle.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// The longest task text an agent may hand over.
pub const MAX_DELEGATION_TASK_CHARS: usize = 16_000;
/// Unfinished delegations one conversation may have at once.
pub const MAX_CONCURRENT_DELEGATIONS: i64 = 3;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GrantDelegationRequest {
    pub assistant_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateDelegationRequest {
    pub assistant_id: String,
    pub task_text: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DelegationStatus {
    Starting,
    Running,
    Completed,
    Failed,
}

impl DelegationStatus {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "starting" => Self::Starting,
            "running" => Self::Running,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub fn is_finished(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DelegationResponse {
    /// The handle the agent passes back to read the result.
    pub id: String,
    pub parent_conversation_id: String,
    pub child_conversation_id: Option<String>,
    pub assistant_id: String,
    pub task_text: String,
    pub status: DelegationStatus,
    pub error: Option<String>,
    /// The child's final reply, once completed.
    pub result_text: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}
