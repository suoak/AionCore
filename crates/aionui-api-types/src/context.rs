use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ContextPurpose {
    Planning,
    Execution,
    Verification,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ContextScope {
    SingleScope { scope_id: String },
    MultiScope { scope_ids: Vec<String> },
    AllAccessible,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextProviderIdentity {
    pub provider: String,
    pub instance_id: String,
    pub display_name: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextProviderCapabilities {
    pub single_scope: bool,
    pub multi_scope: bool,
    pub all_accessible: bool,
    pub provenance: bool,
    pub permission_aware: bool,
    pub freshness: bool,
    pub content_fetch: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextDiscoveryRequest {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default)]
    pub hints: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextSource {
    pub provider: String,
    pub source_id: String,
    pub kind: String,
    pub display_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_source_id: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextQuery {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub query: String,
    pub scope: ContextScope,
    pub limit: usize,
    #[serde(default)]
    pub filters: BTreeMap<String, serde_json::Value>,
    pub purpose: ContextPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextResourceRef {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextProvenance {
    pub provider: String,
    pub source_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<ContextResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub space: Option<ContextResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_base: Option<ContextResourceRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub document: Option<ContextResourceRef>,
    #[serde(default)]
    pub provider_data: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextHit {
    pub provider: String,
    pub source_id: String,
    pub title: String,
    pub snippet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    pub permission_scope: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_or_updated_at: Option<String>,
    pub retrieved_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    pub provenance: ContextProvenance,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextAuthorizationRequest {
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub source_id: String,
    pub scope: ContextScope,
    pub purpose: ContextPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextAuthorization {
    pub allowed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_scope: Option<String>,
    pub rule_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContextFetchRequest {
    pub task_id: String,
    pub source_id: String,
    pub purpose: ContextPurpose,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ContextDocument {
    pub provider: String,
    pub source_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version_or_updated_at: Option<String>,
    pub retrieved_at: i64,
    pub provenance: ContextProvenance,
}
