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

// ---------------------------------------------------------------------------
// Agent-facing contract for `aioncore delegate`. Same shape as
// `conversation_tools.rs`: the registry is the single source of truth for
// `delegate capabilities`, the CLI paths, and the send-boundary hint.
// ---------------------------------------------------------------------------

pub const DELEGATE_TOOLS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateToolName {
    DelegateStart,
    DelegateResult,
    DelegateList,
}

impl DelegateToolName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DelegateStart => "delegate_start",
            Self::DelegateResult => "delegate_result",
            Self::DelegateList => "delegate_list",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "delegate_start" => Self::DelegateStart,
            "delegate_result" => Self::DelegateResult,
            "delegate_list" => Self::DelegateList,
            _ => return None,
        })
    }

    pub fn all() -> [Self; 3] {
        [Self::DelegateStart, Self::DelegateResult, Self::DelegateList]
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DelegateToolDescriptor {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
    pub cli_command: Vec<String>,
    pub when: String,
    pub input_summary: String,
}

pub fn delegate_tool_descriptor(tool: DelegateToolName) -> DelegateToolDescriptor {
    use serde_json::json;
    let (description, input_schema, cli, when, summary) = match tool {
        DelegateToolName::DelegateStart => (
            "Hand one self-contained task to an assistant the user picked with `@` in THIS \
             conversation. Returns at once with a handle; the assistant works in a new \
             conversation in this workspace and sees only `task_text` (none of this chat), \
             so put every needed fact in it.",
            json!({
                "type": "object",
                "properties": {
                    "assistant_id": { "type": "string", "description": "Id of an assistant the user @-picked in this conversation." },
                    "task_text": { "type": "string", "description": format!("The complete task, at most {MAX_DELEGATION_TASK_CHARS} characters.") }
                },
                "required": ["assistant_id", "task_text"],
                "additionalProperties": false
            }),
            "start",
            "The user @-picked an assistant and the task (or a part of it) suits that assistant.",
            "{ assistant_id, task_text }",
        ),
        DelegateToolName::DelegateResult => (
            "Read one delegation by handle: status starting|running|completed|failed, and the \
             assistant's final reply in `result_text` once completed.",
            json!({
                "type": "object",
                "properties": { "id": { "type": "string", "description": "The handle `delegate start` returned." } },
                "required": ["id"],
                "additionalProperties": false
            }),
            "result",
            "After `delegate start`, to collect the result; poll sparingly.",
            "{ id }",
        ),
        DelegateToolName::DelegateList => (
            "List this conversation's delegations, oldest first.",
            json!({ "type": "object", "properties": {}, "additionalProperties": false }),
            "list",
            "To find a handle again or see which delegations are still running.",
            "{}",
        ),
    };
    DelegateToolDescriptor {
        name: tool.as_str().to_owned(),
        description: description.to_owned(),
        input_schema,
        cli_command: vec![cli.to_owned()],
        when: when.to_owned(),
        input_summary: summary.to_owned(),
    }
}

pub fn delegate_tool_descriptors() -> Vec<DelegateToolDescriptor> {
    DelegateToolName::all()
        .into_iter()
        .map(delegate_tool_descriptor)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_delegate_tool_round_trips_and_has_one_cli_word() {
        for tool in DelegateToolName::all() {
            assert_eq!(DelegateToolName::parse(tool.as_str()), Some(tool));
            let descriptor = delegate_tool_descriptor(tool);
            assert_eq!(descriptor.cli_command.len(), 1);
            assert_eq!(
                descriptor.input_schema["additionalProperties"],
                serde_json::json!(false)
            );
        }
        assert_eq!(DelegateToolName::parse("delegate_cancel"), None);
    }

    #[test]
    fn status_parse_matches_the_wire_value() {
        for status in [
            DelegationStatus::Starting,
            DelegationStatus::Running,
            DelegationStatus::Completed,
            DelegationStatus::Failed,
        ] {
            let wire = serde_json::to_value(status).unwrap();
            assert_eq!(DelegationStatus::parse(wire.as_str().unwrap()), Some(status));
        }
        assert!(DelegationStatus::Failed.is_finished());
        assert!(!DelegationStatus::Running.is_finished());
    }
}
