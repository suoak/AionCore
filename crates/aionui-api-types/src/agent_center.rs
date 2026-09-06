//! HTTP contract types for `/api/agent-center/*` (CSBU WorkMate 智能体中心).
//!
//! Agent Center evolves the existing Assistant entity (same `id`); these types
//! layer visibility / ACL / skill pins / MCP allowlist / publish revisions
//! without a second runtime.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

use crate::{
    AssistantConversationOverridesRequest, AssistantConversationRequest, AssistantDetailResponse, AssistantResponse,
    CreateAssistantRequest, UpdateAssistantRequest,
};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentVisibility {
    Private,
    Team,
    Enterprise,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentPublishStatus {
    Draft,
    Published,
    Archived,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentMcpPolicy {
    /// Only `mcp_ids` / assistant default_mcp_ids; **empty = mount no MCP**.
    Allowlist,
    /// Explicit opt-in: intersect with the caller's globally enabled MCP set.
    InheritUserEnabled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SkillVersionPolicy {
    Pin,
    Latest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentSkillRef {
    pub skill_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default = "default_pin_policy")]
    pub version_policy: SkillVersionPolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pinned_version: Option<String>,
}

fn default_pin_policy() -> SkillVersionPolicy {
    SkillVersionPolicy::Pin
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeScopeRef {
    pub knowhub_space_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_ids: Option<Vec<String>>,
    #[serde(default = "default_read_access")]
    pub access: String,
}

fn default_read_access() -> String {
    "read".to_string()
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentAclRole {
    Owner,
    Editor,
    User,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRoleBinding {
    pub subject_type: String,
    pub subject_id: String,
    pub role: AgentAclRole,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentWorkflowOutputFormat {
    Markdown,
    PlainText,
    Json,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentWorkflowInputDefinition {
    #[serde(default = "default_text_input_kind")]
    pub kind: String,
    #[serde(default = "default_true")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<String>,
}

fn default_text_input_kind() -> String {
    "text".to_owned()
}

impl Default for AgentWorkflowInputDefinition {
    fn default() -> Self {
        Self {
            kind: default_text_input_kind(),
            required: true,
            placeholder: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentWorkflowOutputDefinition {
    #[serde(default = "default_markdown_output")]
    pub format: AgentWorkflowOutputFormat,
}

fn default_markdown_output() -> AgentWorkflowOutputFormat {
    AgentWorkflowOutputFormat::Markdown
}

impl Default for AgentWorkflowOutputDefinition {
    fn default() -> Self {
        Self {
            format: default_markdown_output(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentWorkflowNodeDefinition {
    pub id: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub config: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentWorkflowEdgeDefinition {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentWorkflowDefinition {
    #[serde(default = "default_workflow_schema_version")]
    pub schema_version: u32,
    #[serde(default = "default_manual_trigger")]
    pub trigger: String,
    #[serde(default)]
    pub input: AgentWorkflowInputDefinition,
    #[serde(default)]
    pub output: AgentWorkflowOutputDefinition,
    #[serde(default = "default_workflow_nodes")]
    pub nodes: Vec<AgentWorkflowNodeDefinition>,
    #[serde(default = "default_workflow_edges")]
    pub edges: Vec<AgentWorkflowEdgeDefinition>,
}

fn default_workflow_schema_version() -> u32 {
    1
}

fn default_manual_trigger() -> String {
    "manual".to_owned()
}

fn default_workflow_nodes() -> Vec<AgentWorkflowNodeDefinition> {
    vec![
        AgentWorkflowNodeDefinition {
            id: "start".to_owned(),
            kind: "start".to_owned(),
            label: None,
            config: BTreeMap::new(),
        },
        AgentWorkflowNodeDefinition {
            id: "agent".to_owned(),
            kind: "agent".to_owned(),
            label: None,
            config: BTreeMap::new(),
        },
        AgentWorkflowNodeDefinition {
            id: "output".to_owned(),
            kind: "output".to_owned(),
            label: None,
            config: BTreeMap::new(),
        },
    ]
}

fn default_workflow_edges() -> Vec<AgentWorkflowEdgeDefinition> {
    vec![
        AgentWorkflowEdgeDefinition {
            source: "start".to_owned(),
            target: "agent".to_owned(),
        },
        AgentWorkflowEdgeDefinition {
            source: "agent".to_owned(),
            target: "output".to_owned(),
        },
    ]
}

impl Default for AgentWorkflowDefinition {
    fn default() -> Self {
        Self {
            schema_version: default_workflow_schema_version(),
            trigger: default_manual_trigger(),
            input: AgentWorkflowInputDefinition::default(),
            output: AgentWorkflowOutputDefinition::default(),
            nodes: default_workflow_nodes(),
            edges: default_workflow_edges(),
        }
    }
}

impl AgentWorkflowDefinition {
    /// Validates the executable subset supported by workflow schema v1.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema_version != 1 {
            return Err("workflow.schema_version must be 1");
        }
        if self.trigger != "manual" {
            return Err("workflow.trigger must be manual");
        }
        if self.input.kind != "text" {
            return Err("workflow.input.kind must be text");
        }

        if self.nodes.len() < 3 || self.nodes.len() > 20 {
            return Err("workflow must contain between 3 and 20 nodes");
        }
        let mut node_ids = HashSet::new();
        for node in &self.nodes {
            if node.id.trim().is_empty() || node.kind.trim().is_empty() {
                return Err("workflow nodes require non-empty id and kind");
            }
            if !node_ids.insert(node.id.as_str()) {
                return Err("workflow node ids must be unique");
            }
            if !matches!(
                node.kind.as_str(),
                "start" | "agent" | "tool" | "approval" | "condition" | "output"
            ) {
                return Err("workflow contains an unsupported node kind");
            }
        }
        if self.nodes.first().map(|node| node.kind.as_str()) != Some("start")
            || self.nodes.get(1).map(|node| node.kind.as_str()) != Some("agent")
            || self.nodes.last().map(|node| node.kind.as_str()) != Some("output")
        {
            return Err("workflow must start with start -> agent and end with output");
        }
        if self.nodes.iter().filter(|node| node.kind == "start").count() != 1
            || self.nodes.iter().filter(|node| node.kind == "agent").count() != 1
            || self.nodes.iter().filter(|node| node.kind == "output").count() != 1
        {
            return Err("workflow requires exactly one start, agent, and output node");
        }
        if self.edges.len() != self.nodes.len() - 1
            || self
                .nodes
                .windows(2)
                .zip(&self.edges)
                .any(|(nodes, edge)| edge.source != nodes[0].id || edge.target != nodes[1].id)
        {
            return Err("workflow edges must form the declared linear node order");
        }

        Ok(())
    }

    /// Validates configuration required before creating an immutable published revision.
    pub fn validate_for_publish(&self) -> Result<(), &'static str> {
        self.validate()?;
        for node in &self.nodes {
            let required_key = match node.kind.as_str() {
                "tool" => Some("tool_id"),
                "approval" => Some("message"),
                "condition" => Some("expression"),
                _ => None,
            };
            if required_key.is_some_and(|key| node.config.get(key).is_none_or(|value| value.trim().is_empty())) {
                return Err("workflow configurable nodes require complete configuration");
            }
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterMeta {
    pub visibility: AgentVisibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enterprise_id: Option<String>,
    pub status: AgentPublishStatus,
    pub version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_revision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub knowledge_scopes: Vec<KnowledgeScopeRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_refs: Vec<AgentSkillRef>,
    pub mcp_policy: AgentMcpPolicy,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub role_bindings: Vec<AgentRoleBinding>,
    #[serde(default)]
    pub workflow: AgentWorkflowDefinition,
}

impl Default for AgentCenterMeta {
    fn default() -> Self {
        Self {
            visibility: AgentVisibility::Private,
            team_id: None,
            enterprise_id: None,
            status: AgentPublishStatus::Draft,
            version: 0,
            published_revision_id: None,
            knowledge_scopes: Vec::new(),
            skill_refs: Vec::new(),
            mcp_policy: AgentMcpPolicy::Allowlist,
            role_bindings: Vec::new(),
            workflow: AgentWorkflowDefinition::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterListItem {
    pub assistant: AssistantResponse,
    pub meta: AgentCenterMeta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterDetailResponse {
    pub assistant: AssistantDetailResponse,
    pub meta: AgentCenterMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct AgentCenterMetaPatch {
    #[serde(default)]
    pub visibility: Option<AgentVisibility>,
    #[serde(default)]
    pub team_id: Option<Option<String>>,
    #[serde(default)]
    pub enterprise_id: Option<Option<String>>,
    #[serde(default)]
    pub knowledge_scopes: Option<Vec<KnowledgeScopeRef>>,
    #[serde(default)]
    pub skill_refs: Option<Vec<AgentSkillRef>>,
    #[serde(default)]
    pub mcp_policy: Option<AgentMcpPolicy>,
    #[serde(default)]
    pub role_bindings: Option<Vec<AgentRoleBinding>>,
    #[serde(default)]
    pub workflow: Option<AgentWorkflowDefinition>,
    /// When set with `mcp_policy=allowlist`, written to assistant defaults as fixed mcp ids.
    #[serde(default)]
    pub mcp_ids: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CreateAgentCenterRequest {
    #[serde(flatten)]
    pub assistant: CreateAssistantRequest,
    #[serde(default)]
    pub meta: AgentCenterMetaPatch,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct UpdateAgentCenterRequest {
    #[serde(flatten)]
    pub assistant: UpdateAssistantRequest,
    #[serde(default)]
    pub meta: AgentCenterMetaPatch,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PublishAgentCenterRequest {
    #[serde(default)]
    pub changelog: Option<String>,
    /// When true (default), skill_refs with `latest` are resolved to a pin in the snapshot.
    #[serde(default = "default_true")]
    pub pin_skills_on_publish: bool,
}

impl Default for PublishAgentCenterRequest {
    fn default() -> Self {
        Self {
            changelog: None,
            pin_skills_on_publish: true,
        }
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterRevisionResponse {
    pub id: String,
    pub revision: i64,
    pub changelog: Option<String>,
    pub created_by: Option<String>,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<Value>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentCenterPreviewMode {
    /// Live draft config (unpublished or edits after last publish).
    Draft,
    /// Running against the last published revision metadata.
    Published,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCenterRunPlanResponse {
    pub assistant_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision_id: Option<String>,
    pub revision: i64,
    /// Clarifies whether this try-run uses draft or published agent config.
    pub preview_mode: AgentCenterPreviewMode,
    pub workflow: AgentWorkflowDefinition,
    /// Ready-to-POST body for `POST /api/conversations` (existing runtime path).
    pub create_conversation: CreateConversationRequestWire,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentWorkflowRunStatus {
    Running,
    WaitingApproval,
    Completed,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentWorkflowNodeRunStatus {
    Pending,
    Running,
    WaitingApproval,
    Completed,
    Skipped,
    Rejected,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentWorkflowNodeRun {
    pub node_id: String,
    pub kind: String,
    pub status: AgentWorkflowNodeRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentWorkflowNextAction {
    RunAgent {
        create_conversation: CreateConversationRequestWire,
    },
    InvokeTool {
        tool_id: String,
    },
    AwaitApproval {
        node_id: String,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AgentWorkflowRunResponse {
    pub id: String,
    pub assistant_id: String,
    pub status: AgentWorkflowRunStatus,
    pub current_node_index: usize,
    pub workflow: AgentWorkflowDefinition,
    pub nodes: Vec<AgentWorkflowNodeRun>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_action: Option<AgentWorkflowNextAction>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StartAgentWorkflowRunRequest {
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub variables: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvanceAgentWorkflowRunRequest {
    #[serde(default = "default_true")]
    pub success: bool,
    #[serde(default)]
    pub output: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentWorkflowApprovalDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecideAgentWorkflowApprovalRequest {
    pub decision: AgentWorkflowApprovalDecision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// Serde-friendly mirror of create-conversation fields (request type is Deserialize-only upstream).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateConversationRequestWire {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant: Option<AssistantConversationRequest>,
    #[serde(default)]
    pub extra: Value,
}

impl CreateConversationRequestWire {
    pub fn for_assistant(assistant_id: &str, overrides: Option<AssistantConversationOverridesRequest>) -> Self {
        Self {
            name: None,
            assistant: Some(AssistantConversationRequest {
                id: assistant_id.to_owned(),
                locale: None,
                conversation_overrides: overrides,
            }),
            extra: Value::Object(Default::default()),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AgentCenterListQuery {
    /// `mine` | `team` | `enterprise`
    #[serde(default = "default_scope_mine")]
    pub scope: String,
    #[serde(default)]
    pub team_id: Option<String>,
}

fn default_scope_mine() -> String {
    "mine".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skill_ref_defaults_to_pin() {
        let raw = serde_json::json!({"skill_key": "workmate-presentation"});
        let parsed: AgentSkillRef = serde_json::from_value(raw).unwrap();
        assert_eq!(parsed.version_policy, SkillVersionPolicy::Pin);
    }

    #[test]
    fn mcp_policy_allowlist_roundtrip() {
        let meta = AgentCenterMeta::default();
        assert_eq!(meta.mcp_policy, AgentMcpPolicy::Allowlist);
        let v = serde_json::to_value(&meta).unwrap();
        assert_eq!(v["mcp_policy"], "allowlist");
    }

    #[test]
    fn workflow_defaults_to_single_agent_manual_flow() {
        let workflow = AgentWorkflowDefinition::default();
        assert_eq!(workflow.trigger, "manual");
        assert_eq!(workflow.nodes.len(), 3);
        assert_eq!(workflow.edges.len(), 2);
        assert_eq!(workflow.output.format, AgentWorkflowOutputFormat::Markdown);
        assert_eq!(workflow.validate(), Ok(()));
    }

    #[test]
    fn workflow_rejects_edges_to_unknown_nodes() {
        let mut workflow = AgentWorkflowDefinition::default();
        workflow.edges[0].target = "missing".to_owned();
        assert_eq!(
            workflow.validate(),
            Err("workflow edges must form the declared linear node order")
        );
    }

    #[test]
    fn workflow_requires_tool_configuration_only_for_publish() {
        let mut workflow = AgentWorkflowDefinition::default();
        workflow.nodes.insert(
            2,
            AgentWorkflowNodeDefinition {
                id: "tool-1".to_owned(),
                kind: "tool".to_owned(),
                label: None,
                config: BTreeMap::new(),
            },
        );
        workflow.edges = vec![
            AgentWorkflowEdgeDefinition {
                source: "start".to_owned(),
                target: "agent".to_owned(),
            },
            AgentWorkflowEdgeDefinition {
                source: "agent".to_owned(),
                target: "tool-1".to_owned(),
            },
            AgentWorkflowEdgeDefinition {
                source: "tool-1".to_owned(),
                target: "output".to_owned(),
            },
        ];
        assert_eq!(workflow.validate(), Ok(()));
        assert_eq!(
            workflow.validate_for_publish(),
            Err("workflow configurable nodes require complete configuration")
        );
        workflow.nodes[2]
            .config
            .insert("tool_id".to_owned(), "github".to_owned());
        assert_eq!(workflow.validate_for_publish(), Ok(()));
    }
}
