use aionui_common::{
    AgentType, ConversationSource, ConversationStatus, MessagePosition, MessageStatus, MessageType, PaginatedResult,
    ProviderWithModel, TimestampMs,
};
use serde::{Deserialize, Serialize};

use crate::acp::AcpConfigOptionDto;
use crate::agent_build_extra::SessionMcpServer;
use crate::chat_file::ChatFileRef;

/// Per-MCP snapshot status stored in `conversation.extra`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationMcpStatusKind {
    Loaded,
    Failed,
    Unsupported,
}

/// A single MCP item shown in the conversation-scoped MCP list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationMcpStatus {
    pub id: String,
    pub name: String,
    pub status: ConversationMcpStatusKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Typed runtime MCP snapshot for a conversation, persisted in
/// `conversation.extra` as the four fields `mcp_server_ids` /
/// `session_mcp_servers` / `mcp_servers` / `mcp_statuses`.
///
/// Shared by `aionui-conversation` (which builds it), `aionui-team` (which
/// refreshes it on attach) and `aionui-app` (which wires the two), so the team
/// refresh path never has to reason about raw JSON or raw DB rows.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct McpRuntimeSnapshot {
    /// Enabled non-builtin MCP row ids (the `mcp_server_ids` extra field).
    pub mcp_server_ids: Vec<String>,
    /// Enabled builtin MCP servers in neutral form (the `session_mcp_servers`
    /// extra field); stdio launch commands are already resolved.
    pub session_mcp_servers: Vec<SessionMcpServer>,
    /// Merged display names, deduped by name (the `mcp_servers` extra field).
    pub mcp_servers: Vec<String>,
    /// Per-server load status classification (the `mcp_statuses` extra field).
    pub mcp_statuses: Vec<ConversationMcpStatus>,
}

// ── Request types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AssistantConversationOverridesRequest {
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub permission: Option<String>,
    #[serde(default)]
    pub thought_level: Option<String>,
    #[serde(default)]
    pub skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub disabled_builtin_skill_ids: Option<Vec<String>>,
    #[serde(default)]
    pub mcp_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AssistantConversationRequest {
    pub id: String,
    #[serde(default)]
    pub locale: Option<String>,
    #[serde(default)]
    pub conversation_overrides: Option<AssistantConversationOverridesRequest>,
}

/// Body for `POST /api/conversations`.
#[derive(Debug, Deserialize)]
pub struct CreateConversationRequest {
    #[serde(default)]
    pub r#type: Option<AgentType>,
    pub name: Option<String>,
    pub model: Option<ProviderWithModel>,
    pub assistant: Option<AssistantConversationRequest>,
    pub source: Option<ConversationSource>,
    pub channel_chat_id: Option<String>,
    pub extra: serde_json::Value,
}

/// Body for `POST /api/conversations/{id}/fork`.
///
/// Forks the conversation at `message_id` (inclusive) into a NEW conversation
/// that inherits the parent's workspace/agent/history. The backend session
/// materializes lazily on the fork's first open (see `AcpBuildExtra.fork`).
#[derive(Debug, Deserialize)]
pub struct ForkConversationRequest {
    /// The fork point: a message in the parent conversation (inclusive).
    pub message_id: String,
    /// Optional name for the forked conversation; defaults to the parent's.
    #[serde(default)]
    pub name: Option<String>,
}

/// Prompt media capability projection for one conversation, sourced from
/// the effective `prompt_capabilities` projection (ACP handshake or constructed
/// backend descriptor). `None` =
/// unknown/unsupported — the UI then hints that media attachments are sent
/// as file paths. Filled only on the single-conversation detail path (list
/// responses omit it — no N+1).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptCapabilityView {
    /// Agent takes native image content blocks.
    #[serde(default)]
    pub image: bool,
    /// Agent takes native audio content blocks.
    #[serde(default)]
    pub audio: bool,
}

/// Session-fork capability projection for one conversation, sourced from
/// the effective `session_capabilities.fork` projection (ACP handshake or
/// constructed backend descriptor). `Some` = the fork
/// entry point may be shown; `None` = hidden. Filled only on the single-
/// conversation detail path (list responses omit it — no N+1).
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ForkCapabilityView {
    /// Whether the backend can fork at an ARBITRARY turn (codex
    /// `thread/fork.lastTurnId`). `false` = HEAD fork only → the UI shows the
    /// entry point only on the last turn's messages.
    #[serde(default)]
    pub at_turn: bool,
}

/// Body for `PATCH /api/conversations/:id`.
///
/// All fields optional — only supplied fields are applied.
/// `extra` uses merge semantics (patch, not replace).
#[derive(Debug, Deserialize)]
pub struct UpdateConversationRequest {
    pub name: Option<String>,
    /// Intent of a `name` change: `"user"` = explicit rename (agent titles
    /// will never overwrite it afterwards), `"auto"` = frontend-derived
    /// default title (keeps the name overwritable by agent titles).
    /// Absent defaults to `"user"` so old clients' renames stay protected.
    /// Ignored when `name` is absent.
    #[serde(default)]
    pub name_source: Option<String>,
    pub pinned: Option<bool>,
    pub model: Option<ProviderWithModel>,
    pub extra: Option<serde_json::Value>,
}

/// Body for `POST /api/conversations/clone`.
///
/// Despite the name, this endpoint no longer supports cloning from an
/// existing conversation — it's kept as a distinct route because multiple
/// call sites pass a pre-built `CreateConversationRequest` payload shape.
#[derive(Debug, Deserialize)]
pub struct CloneConversationRequest {
    pub conversation: CreateConversationRequest,
}

/// A conversation the user referenced with `@@` in the send box.
///
/// Id only, deliberately: the conversation name is mutable (an agent can
/// rename it — that is what `name_source: "agent"` records), so a
/// client-supplied name may already be stale. The backend resolves the name
/// from the id, in the same query it needs anyway to decide the `workspace`
/// field. Single source of truth.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SessionRef {
    pub id: String,
}

/// Body for `POST /api/conversations/:id/messages`.
///
/// `msg_id` is server-generated — clients must not provide one.
#[derive(Debug, Deserialize)]
pub struct SendMessageRequest {
    pub content: String,
    #[serde(default)]
    pub files: Vec<ChatFileRef>,
    /// Conversations the user referenced with `@@`. Resolved at the send
    /// boundary into the `[[AION_SESSIONS]]` block appended to `content`.
    #[serde(default)]
    pub sessions: Vec<SessionRef>,
    #[serde(default)]
    pub inject_skills: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
}

/// Response for `POST /api/conversations/:id/messages`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendMessageResponse {
    pub msg_id: String,
    pub turn_id: String,
    /// B5 mid-turn interjection: `true` when the message was delivered INTO the
    /// already-running turn (`turn_id` is then the ACTIVE turn's id, no new
    /// turn was opened, and the HTTP status is 200 instead of the normal 202).
    /// Absent/false for an ordinary send that opened a new turn.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub delivered_midturn: bool,
    pub runtime: ConversationRuntimeSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_status: Option<ConversationInputStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationInputMode {
    Followup,
    Steer,
    Inject,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationInputStatus {
    Held,
    Dispatching,
    Accepted,
    Applied,
    Canceled,
    Failed,
}

impl ConversationInputStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Canceled | Self::Failed)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ToolEnforcementLevel {
    Native,
    ApprovalGate,
    ObserveOnly,
}

/// Where a [`ConversationToolSurface`] came from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceSource {
    /// claude `system/init` `tools[]` / `mcp_servers[]`.
    ClaudeInit,
    /// aionrs engine tool registry (`AgentEngine::tool_names`).
    AionrsRegistry,
    /// codex `mcpServerStatus/list`: MCP servers + tools only (codex builtins
    /// have no discovery wire, so they are absent rather than guessed).
    CodexMcpStatus,
    /// M7 E1: the backend reports NO tool list (generic ACP vendors,
    /// antigravity). The surface is what WorkMate launched it with: the MCP
    /// servers WorkMate injected and the launch-time isolation. `tools` is
    /// empty because it is unknown, not because there are none.
    LaunchConfig,
}

/// How the session's MCP surface is scoped.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpIsolation {
    /// Only WorkMate-managed servers can reach the session.
    Strict,
    /// Machine-ambient MCP config may add servers on top of WorkMate's.
    Ambient,
}

/// M7 E1: can WorkMate see which MCP servers the agent ACTUALLY loaded?
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpDiscovery {
    /// The backend reported its loaded servers; `external` flags are real.
    #[default]
    Reported,
    /// The backend has no discovery wire: only WorkMate-injected servers are
    /// listed, and with `mcp_isolation = ambient` the agent may ALSO load
    /// machine/workspace MCP that WorkMate cannot see (status unknown).
    Unavailable,
}

impl McpDiscovery {
    pub fn is_reported(&self) -> bool {
        matches!(self, Self::Reported)
    }
}

/// One MCP server as the agent reported it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSurfaceMcpServer {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Loaded by the agent from machine-ambient config, NOT injected by
    /// WorkMate (outside the conversation's MCP selection). Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
}

/// E2 (C-09): the real tool list of a live session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationToolSurface {
    pub source: ToolSurfaceSource,
    /// Every tool name advertised to the model, verbatim.
    pub tools: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<ToolSurfaceMcpServer>,
    pub mcp_isolation: McpIsolation,
    /// aionrs only: the runtime tool policy in effect (`strict_planning` /
    /// `unrestricted`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<String>,
    /// E2 (C-03/C-09): every tool / skill normalized to one canonical shape,
    /// regardless of how the backend spells it (claude `mcp__srv__tool`, aionrs
    /// bare `tool` or collision-prefixed `mcp__srv_tool`). Omitted when empty so
    /// the pre-normalization wire shape is unchanged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ToolSurfaceEntry>,
    /// C-02: `ToolSearch` is registered but no tool is deferred, so it can
    /// only ever return nothing. Omitted when false.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tool_search_inert: bool,
    /// UI hint (plan D, detect-only): names of the `external` servers above.
    /// Non-empty ⇒ the session can call MCP tools the user did not select for
    /// it. Omitted when empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub external_mcp_servers: Vec<String>,
    /// M7 E1: whether the loaded MCP set was observed (`reported`, default,
    /// omitted on the wire so pre-M7 JSON is unchanged) or cannot be
    /// (`unavailable`, see [`McpDiscovery`]).
    #[serde(default, skip_serializing_if = "McpDiscovery::is_reported")]
    pub mcp_discovery: McpDiscovery,
}

impl ConversationToolSurface {
    /// M7 E1: surface of a backend that reports no tools (generic ACP,
    /// antigravity): the WorkMate-injected servers plus the launch isolation.
    pub fn launch_config(managed_mcp_servers: Vec<String>, isolation: McpIsolation) -> Self {
        Self {
            source: ToolSurfaceSource::LaunchConfig,
            tools: Vec::new(),
            mcp_servers: managed_mcp_servers
                .into_iter()
                .map(|name| ToolSurfaceMcpServer {
                    name,
                    status: None,
                    external: false,
                })
                .collect(),
            mcp_isolation: isolation,
            tool_policy: None,
            entries: Vec::new(),
            tool_search_inert: false,
            external_mcp_servers: Vec::new(),
            mcp_discovery: McpDiscovery::Unavailable,
        }
    }
}

/// Where one normalized tool comes from.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ToolSurfaceKind {
    Builtin,
    Mcp,
    Skill,
}

/// One normalized tool / skill of a live session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolSurfaceEntry {
    /// The name exactly as the backend advertises it to the model.
    pub name: String,
    /// Backend-independent identity: builtin → its name; MCP →
    /// `mcp__<server>__<tool>` with the REPORTED server name; skill →
    /// `skill:<name>`.
    pub canonical: String,
    pub kind: ToolSurfaceKind,
    /// MCP only: the owning server, as the backend reported it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server: Option<String>,
    /// aionrs only: the schema is deferred behind `ToolSearch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred: Option<bool>,
}

/// Normalize an aionrs registry. aionrs registers an MCP tool under its bare
/// name unless it collides (with a builtin or another server's tool), in which
/// case it becomes `mcp__<server>_<tool>` (single underscore). `mcp_tools` is the
/// `(server, tool)` list from the session's MCP managers, so both spellings
/// resolve to the SAME canonical id without guessing at underscores.
pub fn normalize_aionrs_tools(registry_names: &[String], mcp_tools: &[(String, String)]) -> Vec<ToolSurfaceEntry> {
    registry_names
        .iter()
        .map(|name| {
            let prefixed = mcp_tools
                .iter()
                .find(|(server, tool)| *name == format!("mcp__{server}_{tool}"));
            let bare = || {
                // A bare name is MCP only when aionrs did NOT prefix it (no
                // collision); if the prefixed spelling exists, the bare one is the
                // builtin it collided with.
                let mut owners = mcp_tools.iter().filter(|(server, tool)| {
                    tool == name && !registry_names.contains(&format!("mcp__{server}_{tool}"))
                });
                match (owners.next(), owners.next()) {
                    (Some(owner), None) => Some(owner),
                    _ => None,
                }
            };
            match prefixed.or_else(bare) {
                Some((server, tool)) => ToolSurfaceEntry::mcp(name, server, tool),
                None => ToolSurfaceEntry::builtin(name),
            }
        })
        .collect()
}

/// Canonical id of an MCP tool: `mcp__<server>__<tool>`.
pub fn canonical_mcp_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{server}__{tool}")
}

/// claude spells server names inside tool names with every character outside
/// `[A-Za-z0-9_-]` replaced by `_`.
fn claude_tool_server_token(server: &str) -> String {
    server
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

impl ToolSurfaceEntry {
    pub fn builtin(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            canonical: name.to_owned(),
            kind: ToolSurfaceKind::Builtin,
            server: None,
            deferred: None,
        }
    }

    pub fn mcp(name: &str, server: &str, tool: &str) -> Self {
        Self {
            name: name.to_owned(),
            canonical: canonical_mcp_tool_name(server, tool),
            kind: ToolSurfaceKind::Mcp,
            server: Some(server.to_owned()),
            deferred: None,
        }
    }

    pub fn skill(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            canonical: format!("skill:{name}"),
            kind: ToolSurfaceKind::Skill,
            server: None,
            deferred: None,
        }
    }

    /// Classify one claude tool name. `mcp__<token>__<tool>` is MCP; the token is
    /// matched back to a reported server (longest match wins, so a server named
    /// `a__b` is not split early); an unmatched token is used verbatim.
    pub fn from_claude_tool(name: &str, reported_servers: &[String]) -> Self {
        let Some(rest) = name.strip_prefix("mcp__") else {
            return Self::builtin(name);
        };
        let mut best: Option<(&str, &str)> = None;
        for server in reported_servers {
            let token = claude_tool_server_token(server);
            if let Some(tool) = rest.strip_prefix(&token).and_then(|r| r.strip_prefix("__"))
                && !tool.is_empty()
                && best.is_none_or(|(s, _)| claude_tool_server_token(s).len() < token.len())
            {
                best = Some((server.as_str(), tool));
            }
        }
        match best {
            Some((server, tool)) => Self::mcp(name, server, tool),
            None => match rest.split_once("__") {
                Some((server, tool)) if !server.is_empty() && !tool.is_empty() => Self::mcp(name, server, tool),
                _ => Self::builtin(name),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationCapabilities {
    pub followup: bool,
    pub steer: bool,
    pub inject: bool,
    pub tool_enforcement: ToolEnforcementLevel,
    /// E2 (C-08/C-09): the tool surface the running agent ACTUALLY exposes,
    /// as reported by the agent itself. Absent until a live session reported it
    /// (never synthesized from configuration).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_surface: Option<ConversationToolSurface>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub negotiated_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SubmitConversationInputRequest {
    pub mode: ConversationInputMode,
    pub content: String,
    #[serde(default)]
    pub files: Vec<ChatFileRef>,
    #[serde(default)]
    pub inject_skills: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
    pub client_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationInputResponse {
    pub input_id: String,
    pub conversation_id: String,
    pub mode: ConversationInputMode,
    pub status: ConversationInputStatus,
    pub content: String,
    pub files: Vec<ChatFileRef>,
    pub inject_skills: Vec<String>,
    pub hidden: bool,
    pub client_key: String,
    pub turn_id: Option<String>,
    pub msg_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

pub type ConversationInputListResponse = Vec<ConversationInputResponse>;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListConversationInputsQuery {
    pub terminal_limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationInputReceipt {
    pub input: ConversationInputResponse,
    pub runtime: ConversationRuntimeSummary,
    pub capabilities: ConversationCapabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InputChangedEvent {
    pub user_id: String,
    pub input: ConversationInputResponse,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CapabilitiesChangedEvent {
    pub user_id: String,
    pub conversation_id: String,
    pub capabilities: ConversationCapabilities,
}

/// Body for `POST /api/conversations/:id/cancel`.
#[derive(Debug, Clone, Deserialize)]
pub struct CancelConversationRequest {
    pub turn_id: String,
}

/// Response for `POST /api/conversations/:id/cancel`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelConversationResponse {
    pub runtime: ConversationRuntimeSummary,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cancellation_state: Option<CancellationState>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CancellationState {
    Requested,
    Cancelling,
    ConvergedIdle,
    ForceTerminated,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CancellationChangedEvent {
    pub user_id: String,
    pub conversation_id: String,
    pub turn_id: String,
    pub state: CancellationState,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationRuntimeStateKind {
    Idle,
    Starting,
    Running,
    Cancelling,
    Restarting,
    WaitingConfirmation,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationRuntimeSummary {
    pub state: ConversationRuntimeStateKind,
    pub can_send_message: bool,
    pub has_task: bool,
    pub task_status: Option<ConversationStatus>,
    pub is_processing: bool,
    pub pending_confirmations: usize,
    pub turn_id: Option<String>,
    /// Whether a message sent right now reaches the agent without waiting for the
    /// current turn to end. The ONLY capability bit the frontend may gate mid-turn
    /// UI on — see `Capabilities::supports_midturn_delivery` for why
    /// `accepts_proactive_input` must never be exposed.
    #[serde(default)]
    pub supports_midturn_delivery: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EnsureConversationRuntimeResponse {
    pub recovered: bool,
    pub config_options: Vec<AcpConfigOptionDto>,
    pub runtime: ConversationRuntimeSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationAssistantIdentityResponse {
    pub id: String,
    pub source: String,
    pub name: String,
    pub avatar: String,
    pub backend: String,
}

// ── Query types ────────────────────────────────────────────────────

/// Query parameters for `GET /api/conversations`.
#[derive(Debug, Default, Deserialize)]
pub struct ListConversationsQuery {
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub source: Option<String>,
    pub cron_job_id: Option<String>,
    pub pinned: Option<bool>,
}

/// Query parameters for `GET /api/conversations/:id/messages`.
#[derive(Debug, Default, Deserialize)]
pub struct ListMessagesQuery {
    pub limit: Option<u32>,
    pub before: Option<String>,
    pub after: Option<String>,
    pub anchor_message_id: Option<String>,
    pub content_mode: Option<String>,
}

/// Body for `PATCH /api/conversations/:id/artifacts/:artifact_id`.
#[derive(Debug, Deserialize)]
pub struct UpdateConversationArtifactRequest {
    pub status: ConversationArtifactStatus,
}

/// Query parameters for `GET /api/messages/search`.
#[derive(Debug, Deserialize)]
pub struct SearchMessagesQuery {
    pub keyword: String,
    pub page: Option<u32>,
    pub page_size: Option<u32>,
}

// ── Response types ─────────────────────────────────────────────────

/// Full conversation object returned in API responses.
///
/// `model` is the canonical top-level field **only for `AgentType::Aionrs`**.
/// For every other agent type, `model` is always `None` here and the client
/// should read agent-specific model/mode fields out of `extra` (e.g. ACP uses
/// `extra.current_model_id` / `extra.current_mode_id`). See
/// `docs/superpowers/specs/2026-05-12-conversation-type-aware-model-design.md`.
///
/// `Option<T>` fields use `skip_serializing_if = "Option::is_none"` so the
/// serialized JSON omits the key entirely when the value is absent. This
/// keeps the wire shape tight and matches what the frontend mapper already
/// tolerates (`'model' in r` guard handles missing keys).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConversationResponse {
    pub id: String,
    pub name: String,
    /// Origin of the current `name`: `"user"` (explicit rename, protected),
    /// `"agent"` (agent-generated title), or absent for a default/placeholder
    /// name that agents may replace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_source: Option<String>,
    pub r#type: AgentType,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<ProviderWithModel>,
    pub status: ConversationStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<ConversationRuntimeSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<ConversationSource>,
    pub pinned: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned_at: Option<TimestampMs>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_chat_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<ConversationAssistantIdentityResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    /// Service-layer post-fill on the DETAIL path only (like `runtime`);
    /// `None` on list responses. See [`ForkCapabilityView`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fork_capability: Option<ForkCapabilityView>,
    /// Service-layer post-fill on the DETAIL path only (like `runtime`);
    /// `None` on list responses. See [`PromptCapabilityView`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_capability: Option<PromptCapabilityView>,
    pub created_at: TimestampMs,
    pub modified_at: TimestampMs,
    pub extra: serde_json::Value,
}

/// Paginated list of conversations.
pub type ConversationListResponse = PaginatedResult<ConversationResponse>;

/// Single message object returned in API responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageResponse {
    pub id: String,
    pub conversation_id: String,
    pub msg_id: Option<String>,
    pub r#type: MessageType,
    pub content: serde_json::Value,
    pub position: Option<MessagePosition>,
    pub status: Option<MessageStatus>,
    pub hidden: bool,
    pub created_at: TimestampMs,
    /// Backend turn anchor (codex `Turn.id`) stamped on rows persisted while
    /// that turn streamed. Presence tells the UI a mid-history fork can anchor
    /// at (or after) this message; absent on legacy/copied rows and on
    /// backends without turn-anchored forks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend_turn_id: Option<String>,
}

/// Cursor-paginated list of messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageListResponse {
    pub items: Vec<MessageResponse>,
    pub oldest_cursor: Option<String>,
    pub newest_cursor: Option<String>,
    pub has_more_before: bool,
    pub has_more_after: bool,
}

/// Response for `GET /api/conversations/active-count`.
#[derive(Debug, Serialize)]
pub struct ActiveCountResponse {
    pub count: usize,
}

/// Artifact kind discriminant for conversation-bound UI artifacts.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactKind {
    CronTrigger,
    SkillSuggest,
}

/// Durable artifact state exposed to the client.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ConversationArtifactStatus {
    Active,
    Pending,
    Dismissed,
    Saved,
}

/// Artifact object returned by conversation artifact APIs and websocket events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConversationArtifactResponse {
    pub id: String,
    pub conversation_id: String,
    pub cron_job_id: Option<String>,
    pub kind: ConversationArtifactKind,
    pub status: ConversationArtifactStatus,
    pub payload: serde_json::Value,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

/// List of conversation artifacts for a single conversation.
pub type ConversationArtifactListResponse = Vec<ConversationArtifactResponse>;

/// Payload of the `conversation.nameUpdated` websocket event, emitted when an
/// agent-generated session title is applied to a conversation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConversationNameUpdatedPayload {
    pub conversation_id: String,
    pub name: String,
}

/// Payload of the `message.statusChanged` websocket event (B5 mid-turn
/// interjection): a persisted message's `status` field changed outside the
/// normal stream flow. Today it carries the mid-turn user-message receipt
/// transition — `"pending"` (delivered to the CLI, not yet consumed) →
/// `"finish"` (the agent took it: claude `command_lifecycle` echo / codex
/// steer ack). The frontend flips the 待接收/已接收 badge on it, keyed by
/// `msg_id`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MessageStatusChangedPayload {
    pub user_id: String,
    pub conversation_id: String,
    pub msg_id: String,
    pub status: String,
}

/// A single item from cross-conversation message search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessageSearchItem {
    pub message_id: String,
    pub message_type: String,
    pub message_created_at: TimestampMs,
    pub preview_text: String,
    pub conversation: ConversationResponse,
}

/// Paginated search results for messages.
pub type MessageSearchResponse = PaginatedResult<MessageSearchItem>;

#[cfg(test)]
mod tool_surface_tests {
    use super::*;

    #[test]
    fn claude_and_aionrs_spellings_share_one_canonical_id() {
        let claude = ToolSurfaceEntry::from_claude_tool("mcp__docs__search", &["docs".into()]);
        let aionrs_bare = normalize_aionrs_tools(&["search".into()], &[("docs".into(), "search".into())]);
        let aionrs_prefixed = normalize_aionrs_tools(&["mcp__docs_search".into()], &[("docs".into(), "search".into())]);
        assert_eq!(claude.canonical, "mcp__docs__search");
        assert_eq!(aionrs_bare[0].canonical, claude.canonical);
        assert_eq!(aionrs_prefixed[0].canonical, claude.canonical);
        for e in [&claude, &aionrs_bare[0], &aionrs_prefixed[0]] {
            assert_eq!(e.kind, ToolSurfaceKind::Mcp);
            assert_eq!(e.server.as_deref(), Some("docs"));
        }
        assert_eq!(aionrs_prefixed[0].name, "mcp__docs_search", "raw name kept verbatim");
    }

    #[test]
    fn claude_server_with_dots_and_double_underscores_maps_back() {
        let servers = vec!["my.server".to_string(), "a".to_string(), "a__b".to_string()];
        let dotted = ToolSurfaceEntry::from_claude_tool("mcp__my_server__get", &servers);
        assert_eq!(dotted.server.as_deref(), Some("my.server"));
        assert_eq!(dotted.canonical, "mcp__my.server__get");
        let nested = ToolSurfaceEntry::from_claude_tool("mcp__a__b__run", &servers);
        assert_eq!(nested.server.as_deref(), Some("a__b"), "longest server match wins");
        let unknown = ToolSurfaceEntry::from_claude_tool("mcp__ambient__x", &servers);
        assert_eq!(unknown.server.as_deref(), Some("ambient"));
        assert_eq!(
            ToolSurfaceEntry::from_claude_tool("Bash", &servers).kind,
            ToolSurfaceKind::Builtin
        );
        assert_eq!(
            ToolSurfaceEntry::from_claude_tool("mcp__", &servers).kind,
            ToolSurfaceKind::Builtin
        );
    }

    #[test]
    fn aionrs_ambiguous_bare_name_stays_builtin_and_skill_is_tagged() {
        let tools = vec![("a".to_string(), "Read".to_string())];
        // A builtin `Read` collides, so aionrs would have prefixed the MCP one;
        // the bare `Read` in the registry is the builtin.
        let entries = normalize_aionrs_tools(&["Read".into(), "mcp__a_Read".into()], &tools);
        assert_eq!(entries[0].kind, ToolSurfaceKind::Builtin);
        assert_eq!(entries[1].kind, ToolSurfaceKind::Mcp);
        let skill = ToolSurfaceEntry::skill("pdf");
        assert_eq!(
            (skill.canonical.as_str(), skill.kind),
            ("skill:pdf", ToolSurfaceKind::Skill)
        );
    }

    #[test]
    fn surface_without_entries_keeps_legacy_json() {
        let surface = ConversationToolSurface {
            source: ToolSurfaceSource::ClaudeInit,
            tools: vec!["Bash".into()],
            mcp_servers: vec![],
            mcp_isolation: McpIsolation::Ambient,
            tool_policy: None,
            entries: vec![],
            tool_search_inert: false,
            external_mcp_servers: vec![],
            mcp_discovery: McpDiscovery::Reported,
        };
        let json = serde_json::to_value(&surface).unwrap();
        assert!(json.get("entries").is_none() && json.get("tool_search_inert").is_none());
        assert!(json.get("external_mcp_servers").is_none());
        assert!(json.get("mcp_discovery").is_none(), "reported is the omitted default");
        let legacy: ConversationToolSurface = serde_json::from_value(serde_json::json!({
            "source": "claude_init", "tools": ["Bash"], "mcp_isolation": "ambient"
        }))
        .unwrap();
        assert_eq!(legacy, surface);
    }

    #[test]
    fn launch_config_surface_wire_shape() {
        let surface = ConversationToolSurface::launch_config(vec!["fs".into()], McpIsolation::Strict);
        let json = serde_json::to_value(&surface).unwrap();
        assert_eq!(json["source"], "launch_config");
        assert_eq!(json["mcp_discovery"], "unavailable");
        assert_eq!(json["mcp_isolation"], "strict");
        assert_eq!(json["tools"], serde_json::json!([]));
        assert_eq!(json["mcp_servers"], serde_json::json!([{ "name": "fs" }]));
        let back: ConversationToolSurface = serde_json::from_value(json).unwrap();
        assert_eq!(back, surface);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── CreateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_create_request_full() {
        let raw = json!({
            "type": "acp",
            "name": "Code Review",
            "model": { "provider_id": "p1", "model": "claude-sonnet-4-20250514" },
            "assistant": {
                "id": "assistant-1",
                "locale": "zh-CN",
                "conversation_overrides": {
                    "model": "opus-4.1",
                    "permission": "yolo",
                    "thought_level": "high",
                    "skill_ids": ["skill-a"],
                    "disabled_builtin_skill_ids": ["builtin-a"],
                    "mcp_ids": ["mcp-a"]
                }
            },
            "source": "aionui",
            "channel_chat_id": "user:123",
            "extra": { "workspace": "/project" }
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert_eq!(req.name.as_deref(), Some("Code Review"));
        assert_eq!(req.model.unwrap().model, "claude-sonnet-4-20250514");
        assert_eq!(
            req.assistant,
            Some(AssistantConversationRequest {
                id: "assistant-1".into(),
                locale: Some("zh-CN".into()),
                conversation_overrides: Some(AssistantConversationOverridesRequest {
                    model: Some("opus-4.1".into()),
                    permission: Some("yolo".into()),
                    thought_level: Some("high".into()),
                    skill_ids: Some(vec!["skill-a".into()]),
                    disabled_builtin_skill_ids: Some(vec!["builtin-a".into()]),
                    mcp_ids: Some(vec!["mcp-a".into()]),
                }),
            })
        );
        assert_eq!(req.source, Some(ConversationSource::Aionui));
        assert_eq!(req.channel_chat_id.as_deref(), Some("user:123"));
        assert_eq!(req.extra["workspace"], "/project");
    }

    #[test]
    fn deserialize_create_request_minimal() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert!(req.name.is_none());
        assert!(req.assistant.is_none());
        assert!(req.source.is_none());
        assert!(req.channel_chat_id.is_none());
    }

    #[test]
    fn deserialize_create_request_without_model() {
        let raw = json!({
            "type": "acp",
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.r#type, Some(AgentType::Acp));
        assert!(req.model.is_none());
    }

    #[test]
    fn deserialize_create_request_missing_type_without_assistant() {
        let raw = json!({
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.r#type.is_none());
    }

    #[test]
    fn deserialize_create_request_missing_type_with_assistant() {
        let raw = json!({
            "assistant": { "id": "assistant-1" },
            "extra": {}
        });
        let req: CreateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.r#type.is_none());
        assert_eq!(req.assistant.unwrap().id, "assistant-1");
    }

    #[test]
    fn deserialize_create_request_missing_extra() {
        let raw = json!({
            "type": "acp",
            "model": { "provider_id": "p1", "model": "m1" }
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_create_request_invalid_type() {
        let raw = json!({
            "type": "invalid_type",
            "model": { "provider_id": "p1", "model": "m1" },
            "extra": {}
        });
        assert!(serde_json::from_value::<CreateConversationRequest>(raw).is_err());
    }

    // ── UpdateConversationRequest ───────────────────────────────────

    #[test]
    fn deserialize_update_request_partial() {
        let raw = json!({ "name": "New Name" });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("New Name"));
        // Absent name_source deserializes as None (the service treats a
        // name change without it as an explicit user rename).
        assert!(req.name_source.is_none());
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_request_name_source_auto() {
        let raw = json!({ "name": "Derived title", "name_source": "auto" });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name_source.as_deref(), Some("auto"));
    }

    #[test]
    fn conversation_name_updated_payload_round_trip() {
        let payload = ConversationNameUpdatedPayload {
            conversation_id: "conv_1".into(),
            name: "Fix login bug".into(),
        };
        let value = serde_json::to_value(&payload).unwrap();
        assert_eq!(value, json!({ "conversation_id": "conv_1", "name": "Fix login bug" }));
        let back: ConversationNameUpdatedPayload = serde_json::from_value(value).unwrap();
        assert_eq!(back, payload);
    }

    #[test]
    fn deserialize_update_request_all_fields() {
        let raw = json!({
            "name": "Updated",
            "pinned": true,
            "model": { "provider_id": "p2", "model": "new-model" },
            "extra": { "workspace": "/new" }
        });
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.name.as_deref(), Some("Updated"));
        assert_eq!(req.pinned, Some(true));
        assert!(req.model.is_some());
        assert_eq!(req.extra.as_ref().unwrap()["workspace"], "/new");
    }

    #[test]
    fn deserialize_update_request_empty() {
        let raw = json!({});
        let req: UpdateConversationRequest = serde_json::from_value(raw).unwrap();
        assert!(req.name.is_none());
        assert!(req.pinned.is_none());
        assert!(req.model.is_none());
        assert!(req.extra.is_none());
    }

    #[test]
    fn deserialize_update_artifact_request() {
        let raw = json!({ "status": "dismissed" });
        let req: UpdateConversationArtifactRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.status, ConversationArtifactStatus::Dismissed);
    }

    // ── CloneConversationRequest ────────────────────────────────────

    #[test]
    fn deserialize_clone_request() {
        let raw = json!({
            "conversation": {
                "type": "acp",
                "model": { "provider_id": "p1", "model": "m1" },
                "extra": {}
            }
        });
        let req: CloneConversationRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.conversation.r#type, Some(AgentType::Acp));
    }

    // ── ListConversationsQuery ──────────────────────────────────────

    #[test]
    fn deserialize_list_query_full() {
        let raw = json!({
            "cursor": "conv_last",
            "limit": 10,
            "source": "telegram",
            "cron_job_id": "cron_1",
            "pinned": true
        });
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.cursor.as_deref(), Some("conv_last"));
        assert_eq!(q.limit, Some(10));
        assert_eq!(q.source.as_deref(), Some("telegram"));
        assert_eq!(q.cron_job_id.as_deref(), Some("cron_1"));
        assert_eq!(q.pinned, Some(true));
    }

    #[test]
    fn deserialize_list_query_empty() {
        let raw = json!({});
        let q: ListConversationsQuery = serde_json::from_value(raw).unwrap();
        assert!(q.cursor.is_none());
        assert!(q.limit.is_none());
        assert!(q.source.is_none());
        assert!(q.cron_job_id.is_none());
        assert!(q.pinned.is_none());
    }

    // ── ListMessagesQuery ───────────────────────────────────────────

    #[test]
    fn deserialize_messages_query_defaults() {
        let raw = json!({});
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert!(q.limit.is_none());
        assert!(q.before.is_none());
        assert!(q.after.is_none());
        assert!(q.anchor_message_id.is_none());
        assert!(q.content_mode.is_none());
    }

    #[test]
    fn deserialize_messages_query_with_values() {
        let raw = json!({
            "limit": 200,
            "before": "v1.abc",
            "content_mode": "compact"
        });
        let q: ListMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.limit, Some(200));
        assert_eq!(q.before.as_deref(), Some("v1.abc"));
        assert!(q.after.is_none());
        assert!(q.anchor_message_id.is_none());
        assert_eq!(q.content_mode.as_deref(), Some("compact"));
    }

    // ── SearchMessagesQuery ─────────────────────────────────────────

    #[test]
    fn deserialize_search_query() {
        let raw = json!({ "keyword": "rust", "page": 1, "page_size": 20 });
        let q: SearchMessagesQuery = serde_json::from_value(raw).unwrap();
        assert_eq!(q.keyword, "rust");
        assert_eq!(q.page, Some(1));
        assert_eq!(q.page_size, Some(20));
    }

    #[test]
    fn deserialize_search_query_missing_keyword() {
        let raw = json!({ "page": 1 });
        assert!(serde_json::from_value::<SearchMessagesQuery>(raw).is_err());
    }

    // ── ConversationResponse ────────────────────────────────────────

    #[test]
    fn serialize_conversation_response_snake_case() {
        let resp = ConversationResponse {
            id: "conv_1".into(),
            name: "Test".into(),
            name_source: None,
            r#type: AgentType::Acp,
            model: Some(ProviderWithModel {
                provider_id: "p1".into(),
                model: "m1".into(),
                use_model: None,
            }),
            status: ConversationStatus::Pending,
            runtime: None,
            source: Some(ConversationSource::Aionui),
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            assistant: None,
            project_id: None,
            created_at: 1712345678000,
            modified_at: 1712345678000,
            extra: json!({ "workspace": "/project" }),
            fork_capability: None,
            prompt_capability: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "conv_1");
        assert_eq!(json["type"], "acp");
        assert_eq!(json["status"], "pending");
        assert_eq!(json["source"], "aionui");
        assert_eq!(json["created_at"], 1712345678000_i64);
        assert_eq!(json["modified_at"], 1712345678000_i64);
        assert_eq!(json["extra"]["workspace"], "/project");
        // Verify snake_case keys
        assert!(json.get("channelChatId").is_none());
        assert!(json.get("createdAt").is_none());
        assert!(json.get("pinnedAt").is_none());
        // Null-valued Option fields must be omitted from JSON.
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
    }

    #[test]
    fn serialize_conversation_response_omits_none_keys() {
        let resp = ConversationResponse {
            id: "conv_none".into(),
            name: "Test".into(),
            name_source: None,
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Pending,
            runtime: None,
            source: None,
            pinned: false,
            pinned_at: None,
            channel_chat_id: None,
            assistant: None,
            project_id: None,
            created_at: 1,
            modified_at: 1,
            extra: json!({}),
            fork_capability: None,
            prompt_capability: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("model").is_none(), "model None should be omitted");
        assert!(json.get("source").is_none(), "source None should be omitted");
        assert!(json.get("pinned_at").is_none(), "pinned_at None should be omitted");
        assert!(
            json.get("channel_chat_id").is_none(),
            "channel_chat_id None should be omitted"
        );
        // Non-optional fields still present.
        assert_eq!(json["id"], "conv_none");
        assert_eq!(json["type"], "acp");
        assert_eq!(json["pinned"], false);
    }

    #[test]
    fn conversation_response_roundtrip() {
        let resp = ConversationResponse {
            id: "conv_2".into(),
            name: "Round".into(),
            name_source: None,
            r#type: AgentType::Acp,
            model: None,
            status: ConversationStatus::Running,
            runtime: None,
            source: None,
            pinned: true,
            pinned_at: Some(1712345678000),
            channel_chat_id: Some("group:42".into()),
            assistant: None,
            project_id: None,
            created_at: 1000,
            modified_at: 2000,
            extra: json!({}),
            fork_capability: None,
            prompt_capability: None,
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: ConversationResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, resp.id);
        assert!(deserialized.pinned);
        assert_eq!(deserialized.pinned_at, Some(1712345678000));
        assert_eq!(deserialized.channel_chat_id.as_deref(), Some("group:42"));
    }

    // ── MessageResponse ─────────────────────────────────────────────

    #[test]
    fn serialize_message_response_snake_case() {
        let resp = MessageResponse {
            id: "msg_1".into(),
            conversation_id: "conv_1".into(),
            msg_id: Some("client_1".into()),
            r#type: MessageType::Text,
            content: json!({ "content": "Hello" }),
            position: Some(MessagePosition::Right),
            status: Some(MessageStatus::Finish),
            hidden: false,
            created_at: 1712345678000,
            backend_turn_id: None,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["id"], "msg_1");
        assert_eq!(json["conversation_id"], "conv_1");
        assert_eq!(json["msg_id"], "client_1");
        assert_eq!(json["type"], "text");
        assert_eq!(json["position"], "right");
        assert_eq!(json["status"], "finish");
        assert_eq!(json["hidden"], false);
        assert_eq!(json["created_at"], 1712345678000_i64);
        // Verify no camelCase leaks
        assert!(json.get("conversationId").is_none());
        assert!(json.get("msgId").is_none());
        assert!(json.get("createdAt").is_none());
    }

    #[test]
    fn message_response_roundtrip() {
        let resp = MessageResponse {
            id: "msg_2".into(),
            conversation_id: "conv_2".into(),
            msg_id: None,
            r#type: MessageType::ToolCall,
            content: json!({ "callId": "c1", "name": "bash" }),
            position: Some(MessagePosition::Left),
            status: None,
            hidden: true,
            created_at: 5000,
            backend_turn_id: None,
        };
        let serialized = serde_json::to_string(&resp).unwrap();
        let deserialized: MessageResponse = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.r#type, MessageType::ToolCall);
        assert!(deserialized.hidden);
        assert!(deserialized.msg_id.is_none());
        assert!(deserialized.status.is_none());
    }

    // ── MessageSearchItem ───────────────────────────────────────────

    #[test]
    fn serialize_search_item_snake_case() {
        let item = MessageSearchItem {
            message_id: "msg_1".into(),
            message_type: "text".into(),
            message_created_at: 1712345678000,
            preview_text: "matched snippet".into(),
            conversation: ConversationResponse {
                id: "conv_1".into(),
                name: "Code Review".into(),
                name_source: None,
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                project_id: None,
                created_at: 1712345678000,
                modified_at: 1712345678000,
                extra: json!({}),
                fork_capability: None,
                prompt_capability: None,
            },
        };
        let json = serde_json::to_value(&item).unwrap();
        assert_eq!(json["message_id"], "msg_1");
        assert_eq!(json["message_type"], "text");
        assert_eq!(json["message_created_at"], 1712345678000_i64);
        assert_eq!(json["preview_text"], "matched snippet");
        assert_eq!(json["conversation"]["id"], "conv_1");
        assert_eq!(json["conversation"]["name"], "Code Review");
        // Verify no camelCase leaks
        assert!(json.get("messageId").is_none());
        assert!(json.get("messageType").is_none());
        assert!(json.get("previewText").is_none());
    }

    #[test]
    fn search_item_roundtrip() {
        let item = MessageSearchItem {
            message_id: "msg_x".into(),
            message_type: "tips".into(),
            message_created_at: 9000,
            preview_text: "some content preview".into(),
            conversation: ConversationResponse {
                id: "conv_x".into(),
                name: "Search Test".into(),
                name_source: None,
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Finished,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                project_id: None,
                created_at: 9000,
                modified_at: 9000,
                extra: json!({}),
                fork_capability: None,
                prompt_capability: None,
            },
        };
        let serialized = serde_json::to_string(&item).unwrap();
        let deserialized: MessageSearchItem = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.message_id, "msg_x");
        assert_eq!(deserialized.message_type, "tips");
        assert_eq!(deserialized.preview_text, "some content preview");
        assert_eq!(deserialized.conversation.name, "Search Test");
    }

    // ── SendMessageRequest ──────────────────────────────────────────

    #[test]
    fn deserialize_send_message_full() {
        let raw = json!({
            "content": "Review this code",
            "files": [
                { "kind": "project", "pe_id": "pe1", "relative_path": "src/a.rs" },
                { "kind": "upload", "path": "/tmp/a.rs" },
                { "kind": "local", "path": "/Users/me/notes.txt" }
            ],
            "inject_skills": ["security-review"],
            "hidden": true
        });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Review this code");
        assert_eq!(
            req.files,
            vec![
                ChatFileRef::Project {
                    pe_id: "pe1".into(),
                    relative_path: "src/a.rs".into()
                },
                ChatFileRef::Upload {
                    path: "/tmp/a.rs".into()
                },
                ChatFileRef::Local {
                    path: "/Users/me/notes.txt".into()
                },
            ]
        );
        assert_eq!(req.inject_skills, vec!["security-review"]);
        assert!(req.hidden);
    }

    #[test]
    fn deserialize_send_message_minimal() {
        let raw = json!({ "content": "Hi" });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hi");
        assert!(req.files.is_empty());
        assert!(req.inject_skills.is_empty());
        assert!(!req.hidden);
    }

    #[test]
    fn deserialize_send_message_missing_content() {
        let raw = json!({});
        assert!(serde_json::from_value::<SendMessageRequest>(raw).is_err());
    }

    #[test]
    fn deserialize_send_message_ignores_client_msg_id() {
        // Clients may still send msg_id from stale builds — it must be ignored.
        let raw = json!({ "content": "Hi", "msg_id": "client-supplied" });
        let req: SendMessageRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.content, "Hi");
    }

    // ── Paginated type aliases ──────────────────────────────────────

    #[test]
    fn conversation_list_response_serialization() {
        let list: ConversationListResponse = PaginatedResult {
            items: vec![ConversationResponse {
                id: "conv_1".into(),
                name: "Test".into(),
                name_source: None,
                r#type: AgentType::Acp,
                model: None,
                status: ConversationStatus::Pending,
                runtime: None,
                source: None,
                pinned: false,
                pinned_at: None,
                channel_chat_id: None,
                assistant: None,
                project_id: None,
                created_at: 1000,
                modified_at: 1000,
                extra: json!({}),
                fork_capability: None,
                prompt_capability: None,
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["items"].as_array().unwrap().len(), 1);
        assert_eq!(json["total"], 1);
        assert_eq!(json["has_more"], false);
    }

    #[test]
    fn serialize_message_list_response_cursor_shape() {
        let resp = MessageListResponse {
            items: vec![],
            oldest_cursor: None,
            newest_cursor: None,
            has_more_before: false,
            has_more_after: false,
        };
        let raw = serde_json::to_value(resp).unwrap();
        assert_eq!(raw["items"], json!([]));
        assert!(raw["oldest_cursor"].is_null());
        assert!(raw["newest_cursor"].is_null());
        assert_eq!(raw["has_more_before"], false);
        assert_eq!(raw["has_more_after"], false);
        assert!(raw.get("total").is_none());
        assert!(raw.get("has_more").is_none());
    }

    #[test]
    fn message_search_response_serialization() {
        let resp: MessageSearchResponse = PaginatedResult {
            items: vec![MessageSearchItem {
                message_id: "m1".into(),
                message_type: "text".into(),
                message_created_at: 5000,
                preview_text: "matched".into(),
                conversation: ConversationResponse {
                    id: "c1".into(),
                    name: "Conv".into(),
                    name_source: None,
                    r#type: AgentType::Acp,
                    model: None,
                    status: ConversationStatus::Finished,
                    runtime: None,
                    source: None,
                    pinned: false,
                    pinned_at: None,
                    channel_chat_id: None,
                    assistant: None,
                    project_id: None,
                    created_at: 5000,
                    modified_at: 5000,
                    extra: json!({}),
                    fork_capability: None,
                    prompt_capability: None,
                },
            }],
            total: 1,
            has_more: false,
        };
        let json = serde_json::to_value(&resp).unwrap();
        assert_eq!(json["items"][0]["message_id"], "m1");
        assert_eq!(json["items"][0]["conversation"]["id"], "c1");
        assert_eq!(json["items"][0]["preview_text"], "matched");
        assert_eq!(json["total"], 1);
    }

    #[test]
    fn serialize_conversation_artifact_response() {
        let artifact = ConversationArtifactResponse {
            id: "conv_1:skill_suggest:cron_1".into(),
            conversation_id: "conv_1".into(),
            cron_job_id: Some("cron_1".into()),
            kind: ConversationArtifactKind::SkillSuggest,
            status: ConversationArtifactStatus::Active,
            payload: json!({
                "cron_job_id": "cron_1",
                "name": "daily-report",
                "description": "Daily report",
                "skillContent": "---\nname: daily-report\n---\nUse it.",
            }),
            created_at: 1000,
            updated_at: 2000,
        };

        let raw = serde_json::to_value(&artifact).unwrap();
        assert_eq!(raw["kind"], "skill_suggest");
        assert_eq!(raw["status"], "active");
        assert_eq!(raw["payload"]["name"], "daily-report");
    }
}
