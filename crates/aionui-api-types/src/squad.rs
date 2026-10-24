//! Squad template wire types (M8 E7-1). Contract: m8-s5.md "E7 Squad".

use serde::{Deserialize, Serialize};

/// D7-5: a Squad has at most 6 participants, Leader included.
pub const SQUAD_MAX_PARTICIPANTS: usize = 6;

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SquadLeaderMode {
    /// D7-1 default: the Leader only dispatches, never does the work itself.
    #[default]
    RouteOnly,
    RouteAndWork,
}

impl SquadLeaderMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::RouteOnly => "route_only",
            Self::RouteAndWork => "route_and_work",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "route_only" => Some(Self::RouteOnly),
            "route_and_work" => Some(Self::RouteAndWork),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SquadVisibility {
    #[default]
    Private,
    /// Reserved for E12; refused with 403 until team roles land.
    Team,
}

impl SquadVisibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Team => "team",
        }
    }
}

fn default_max_parallel() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadMember {
    pub assistant_id: String,
    pub role_title: String,
    #[serde(default)]
    pub role_description: String,
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
}

/// Minimal Assistant info so the editor does not have to look each one up.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantBrief {
    pub id: String,
    /// Falls back to the id when the Assistant is missing.
    pub name: String,
    pub enabled: bool,
    /// The referenced Assistant was deleted, disabled or is no longer
    /// team-selectable.
    pub missing: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadTemplate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub leader_assistant_id: String,
    pub leader_mode: SquadLeaderMode,
    pub members: Vec<SquadMember>,
    pub visibility: SquadVisibility,
    pub team_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub deleted_at: Option<i64>,
    pub leader: Option<AssistantBrief>,
    /// Same order as `members`.
    pub member_assistants: Vec<AssistantBrief>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadTemplateInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub leader_assistant_id: String,
    #[serde(default)]
    pub leader_mode: SquadLeaderMode,
    pub members: Vec<SquadMember>,
    #[serde(default)]
    pub visibility: SquadVisibility,
}

/// `PATCH /api/squads/{id}`: any subset of the input fields plus the
/// optimistic-concurrency token (required).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadTemplatePatch {
    pub expected_updated_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_assistant_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leader_mode: Option<SquadLeaderMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub members: Option<Vec<SquadMember>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub visibility: Option<SquadVisibility>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListSquadsQuery {
    /// `1`/`true` also returns soft-deleted templates.
    #[serde(default)]
    pub include_deleted: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteSquadResponse {
    pub id: String,
    pub deleted_at: i64,
}
