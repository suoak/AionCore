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

// ---------------------------------------------------------------------------
// M8 E7-2: member-config preview + instantiation
// ---------------------------------------------------------------------------

/// Hard rule D7-4 as surfaced to clients: no participant of a Squad team may
/// spawn/add agents. Always `"forbidden"`; there is no template field for it.
pub const SQUAD_SUBAGENT_SPAWN_FORBIDDEN: &str = "forbidden";

/// One participant as it would be provisioned.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SquadPreviewParticipant {
    pub assistant_id: String,
    /// Agent slot name the Team will use (role_title for members, de-duplicated).
    pub agent_name: String,
    /// `"lead"` | `"teammate"`.
    pub role: String,
    pub role_title: String,
    pub role_description: String,
    pub max_parallel: u32,
    pub assistant: AssistantBrief,
    pub backend: Option<String>,
    /// `false` when the assistant is no longer in the team-selectable catalog.
    pub eligible: bool,
    /// Always [`SQUAD_SUBAGENT_SPAWN_FORBIDDEN`].
    pub subagent_spawn: String,
    /// Leader only, `route_only`: the Team MCP tools it keeps. `None` = unrestricted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_tools: Option<Vec<String>>,
    /// M8 E7-4: native tool profile applied at launch.
    pub tool_profile: SquadToolProfile,
    /// M8 E7-4: whether the backend honours `tool_profile` (`unenforceable` =
    /// the backend's own sub-agent/write/exec tools stay available).
    pub native_tool_enforcement: NativeToolEnforcement,
}

/// M8 E7-4: backend-native tool restriction applied to a Squad participant's
/// conversation (`extra.squad_tool_profile`).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SquadToolProfile {
    /// route_only Leader: no native sub-agent, file-write or exec tools.
    RouteOnlyLeader,
    /// Member: no native sub-agent tool.
    Member,
}

/// Whether CSBU WorkMate can strip a backend's native tools at launch.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NativeToolEnforcement {
    Enforced,
    /// The backend has no launch-time tool exclusion WorkMate can drive
    /// (codex, antigravity, other ACP; gemini/aionrs pending, see m8-s5.md).
    Unenforceable,
}

/// 400 code: a route_only Leader was put on a backend whose native
/// sub-agent / write / exec tools cannot be removed.
pub const SQUAD_LEADER_BACKEND_UNENFORCEABLE: &str = "squad_leader_backend_unenforceable";

/// Native claude tool names: sub-agent (`Task`, renamed `Agent` in newer CLIs).
pub const CLAUDE_SUBAGENT_TOOLS: &[&str] = &["Task", "Agent"];
/// Native claude file-write and exec tools.
pub const CLAUDE_WRITE_EXEC_TOOLS: &[&str] = &[
    "Write",
    "Edit",
    "MultiEdit",
    "NotebookEdit",
    "Bash",
    "BashOutput",
    "KillShell",
    "KillBash",
];

/// Whether Core can strip the backend's *native* tools for `profile`
/// (E7-4). claude: `--disallowedTools` (both profiles). aionrs: exact-name
/// `ToolPolicy::AllowOnly` — only usable for the route_only Leader, because a
/// Member allow-list would need the MCP tool names that only exist after
/// bootstrap. Everything else (gemini — `--exclude-tools` is not honoured by
/// ACP sessions in gemini-cli 0.65 —, codex, agy, other ACP) is unenforceable.
pub fn squad_native_tool_enforcement(backend: &str, profile: SquadToolProfile) -> NativeToolEnforcement {
    match (backend, profile) {
        ("claude", _) => NativeToolEnforcement::Enforced,
        ("aionrs", SquadToolProfile::RouteOnlyLeader) => NativeToolEnforcement::Enforced,
        _ => NativeToolEnforcement::Unenforceable,
    }
}

/// D7-1 `route_only`: the Team MCP tools a Squad Leader keeps.
pub const SQUAD_ROUTE_ONLY_LEADER_TOOLS: &[&str] = &[
    "team_send_message",
    "team_task_update",
    "team_task_list",
    "team_describe_assistant",
];

/// aionrs built-ins a route_only Leader keeps (read-only + ToolSearch, which
/// only loads schemas; policy still gates the loaded tool).
pub const AIONRS_ROUTE_ONLY_LEADER_BUILTINS: &[&str] = &["Read", "Grep", "Glob", "ViewImage", "ToolSearch"];

/// Full aionrs `AllowOnly` set for a route_only Leader: read-only built-ins
/// plus the Team MCP route tools under both names aionrs may expose them as
/// (bare, or `mcp__{server}_{tool}` on a name collision).
pub fn aionrs_route_only_leader_allowed_tools(route_tools: &[&str]) -> Vec<String> {
    let mut tools: Vec<String> = AIONRS_ROUTE_ONLY_LEADER_BUILTINS
        .iter()
        .map(|t| (*t).to_owned())
        .collect();
    for tool in route_tools {
        tools.push((*tool).to_owned());
        tools.push(format!("mcp__{}_{tool}", crate::TEAM_MCP_SERVER_NAME));
    }
    tools
}

/// claude `--disallowedTools` for a profile.
pub fn claude_disallowed_tools(profile: SquadToolProfile) -> Vec<String> {
    let mut tools: Vec<String> = CLAUDE_SUBAGENT_TOOLS.iter().map(|t| (*t).to_owned()).collect();
    if profile == SquadToolProfile::RouteOnlyLeader {
        tools.extend(CLAUDE_WRITE_EXEC_TOOLS.iter().map(|t| (*t).to_owned()));
    }
    tools
}

/// `GET /api/squads/{id}/preview`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SquadPreview {
    pub squad_template_id: String,
    pub name: String,
    pub leader_mode: SquadLeaderMode,
    /// Leader first, then members in template order.
    pub participants: Vec<SquadPreviewParticipant>,
    pub can_instantiate: bool,
    /// Stable codes explaining why `can_instantiate` is false
    /// (e.g. `squad_assistant_not_eligible`).
    pub blocking: Vec<SquadPreviewBlocker>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SquadPreviewBlocker {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_id: Option<String>,
}

/// `POST /api/squads/{id}/instantiate` body (all optional).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SquadInstantiateInput {
    /// Team name; defaults to the template name.
    #[serde(default)]
    pub name: Option<String>,
    /// Shared workspace; defaults to the Team provisioning default.
    #[serde(default)]
    pub workspace: Option<String>,
}

/// `POST /api/squads/{id}/instantiate` response.
#[derive(Debug, Clone, Serialize)]
pub struct SquadInstantiateResponse {
    pub squad_template_id: String,
    pub team: crate::TeamResponse,
}

/// M8 E7-5: Squad run state shown on board cards / team page
/// (`GET /api/teams/{id}/squad-run`). Derived from existing team task +
/// team-run data; Core stores nothing extra.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SquadRunState {
    /// Tasks exist but none has started and no run is active.
    WaitingBacklog,
    /// A team run is accepted/running/cancelling.
    Running,
    /// No active run, open (pending/in-progress) tasks remain.
    Paused,
    /// No active run and every live task is completed.
    Finished,
    /// The most recent team run failed and no run is active.
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SquadRunPayload {
    pub team_id: String,
    pub state: SquadRunState,
    pub leader_conversation_id: Option<String>,
    /// `route_only` | other template leader modes; `None` if unset.
    pub leader_mode: Option<String>,
    /// Epoch ms: newest task update, falling back to team `updated_at`.
    pub updated_at: i64,
}

#[cfg(test)]
mod e7_4_tests {
    use super::*;

    #[test]
    fn squad_run_state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&SquadRunState::WaitingBacklog).unwrap(),
            "\"waiting_backlog\""
        );
    }

    #[test]
    fn enforcement_matrix_is_backend_and_profile_aware() {
        use NativeToolEnforcement::*;
        use SquadToolProfile::*;
        assert_eq!(squad_native_tool_enforcement("claude", RouteOnlyLeader), Enforced);
        assert_eq!(squad_native_tool_enforcement("claude", Member), Enforced);
        assert_eq!(squad_native_tool_enforcement("aionrs", RouteOnlyLeader), Enforced);
        assert_eq!(squad_native_tool_enforcement("aionrs", Member), Unenforceable);
        for backend in ["gemini", "codex", "agy", "qwen", "opencode", ""] {
            assert_eq!(
                squad_native_tool_enforcement(backend, RouteOnlyLeader),
                Unenforceable,
                "{backend}"
            );
            assert_eq!(
                squad_native_tool_enforcement(backend, Member),
                Unenforceable,
                "{backend}"
            );
        }
    }

    #[test]
    fn claude_profiles_and_aionrs_leader_allow_list() {
        assert_eq!(claude_disallowed_tools(SquadToolProfile::Member), vec!["Task", "Agent"]);
        let leader = claude_disallowed_tools(SquadToolProfile::RouteOnlyLeader);
        assert!(leader.iter().any(|t| t == "Bash") && leader.iter().any(|t| t == "Write"));

        let allowed = aionrs_route_only_leader_allowed_tools(SQUAD_ROUTE_ONLY_LEADER_TOOLS);
        for denied in [
            "Spawn",
            "Write",
            "Edit",
            "ExecCommand",
            "Skill",
            "EnterPlanMode",
            "team_spawn_agent",
        ] {
            assert!(!allowed.iter().any(|t| t == denied), "{denied} must not be allowed");
        }
        assert!(allowed.iter().any(|t| t == "team_send_message"));
        assert!(allowed.iter().any(|t| t == "mcp__aionui-team_team_send_message"));
        assert!(allowed.iter().any(|t| t == "Read"));
    }

    #[test]
    fn tool_profile_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(SquadToolProfile::RouteOnlyLeader).unwrap(),
            "route_only_leader"
        );
        assert_eq!(
            serde_json::to_value(NativeToolEnforcement::Unenforceable).unwrap(),
            "unenforceable"
        );
    }
}
