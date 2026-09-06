//! Agent Center service — evolves Assistant with visibility / publish / run plan.
//!
//! Reuses [`AssistantService`] for identity/rules/defaults; stores Agent Center
//! fields in side tables so existing `/api/assistants` CRUD stays intact.

use std::sync::Arc;

use aionui_api_types::{
    AdvanceAgentWorkflowRunRequest, AgentCenterDetailResponse, AgentCenterListItem, AgentCenterMeta,
    AgentCenterMetaPatch, AgentCenterPreviewMode, AgentCenterRevisionResponse, AgentCenterRunPlanResponse,
    AgentMcpPolicy, AgentPublishStatus, AgentSkillRef, AgentVisibility, AgentWorkflowApprovalDecision,
    AgentWorkflowNextAction, AgentWorkflowNodeRun, AgentWorkflowNodeRunStatus, AgentWorkflowRunResponse,
    AgentWorkflowRunStatus, AssistantConversationOverridesRequest, AssistantDefaultListRequest,
    AssistantDefaultsRequest, CreateAgentCenterRequest, CreateConversationRequestWire,
    DecideAgentWorkflowApprovalRequest, PublishAgentCenterRequest, SkillVersionPolicy, StartAgentWorkflowRunRequest,
    UpdateAgentCenterRequest, UpdateAssistantRequest,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateAgentWorkflowRunParams, CreateAssistantDefinitionRevisionParams, IAgentWorkflowRunRepository,
    IAssistantAgentCenterRepository, IAssistantDefinitionRepository, IAssistantDefinitionRevisionRepository,
    UpsertAssistantAgentCenterParams,
};
use serde_json::json;

use crate::error::AssistantError;
use crate::service::AssistantService;

pub struct AgentCenterService {
    assistants: Arc<AssistantService>,
    definition_repo: Arc<dyn IAssistantDefinitionRepository>,
    center_repo: Arc<dyn IAssistantAgentCenterRepository>,
    revision_repo: Arc<dyn IAssistantDefinitionRevisionRepository>,
    workflow_run_repo: Arc<dyn IAgentWorkflowRunRepository>,
}

impl AgentCenterService {
    pub fn new(
        assistants: Arc<AssistantService>,
        definition_repo: Arc<dyn IAssistantDefinitionRepository>,
        center_repo: Arc<dyn IAssistantAgentCenterRepository>,
        revision_repo: Arc<dyn IAssistantDefinitionRevisionRepository>,
        workflow_run_repo: Arc<dyn IAgentWorkflowRunRepository>,
    ) -> Self {
        Self {
            assistants,
            definition_repo,
            center_repo,
            revision_repo,
            workflow_run_repo,
        }
    }

    pub async fn list_for_user(
        &self,
        user_id: &str,
        scope: &str,
        team_id: Option<&str>,
    ) -> Result<Vec<AgentCenterListItem>, AssistantError> {
        let assistants = self.assistants.list_for_user(user_id).await?;
        let mut out = Vec::new();
        for assistant in assistants {
            // Skip builtins for Agent Center list (center is for productized agents).
            if matches!(assistant.source, aionui_api_types::AssistantSource::Builtin) {
                continue;
            }
            let definition = match self
                .definition_repo
                .get_by_assistant_id_for_user(user_id, &assistant.id)
                .await
            {
                Ok(Some(definition)) => definition,
                Ok(None) => continue,
                Err(err) => {
                    tracing::warn!(
                        assistant_id = %assistant.id,
                        error = %err,
                        "agent-center: skipping assistant due to definition lookup error"
                    );
                    continue;
                }
            };
            let meta = match self.load_or_default_meta(&definition.id).await {
                Ok(meta) => meta,
                Err(err) => {
                    tracing::warn!(
                        assistant_id = %assistant.id,
                        definition_id = %definition.id,
                        error = %err,
                        "agent-center: skipping assistant due to meta load error"
                    );
                    continue;
                }
            };
            let include = match scope {
                "team" => {
                    meta.visibility == AgentVisibility::Team
                        && team_id.is_some_and(|tid| meta.team_id.as_deref() == Some(tid))
                }
                "enterprise" => meta.visibility == AgentVisibility::Enterprise,
                _ => {
                    // mine: private owned by user, or any draft/published the user owns (non-team listing)
                    meta.visibility == AgentVisibility::Private || definition.owner_type == "user"
                }
            };
            if include {
                out.push(AgentCenterListItem { assistant, meta });
            }
        }
        Ok(out)
    }

    pub async fn get_detail_for_user(
        &self,
        user_id: &str,
        id: &str,
        locale: Option<&str>,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let assistant = self.assistants.get_detail_for_user(user_id, id, locale).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let meta = self.load_or_default_meta(&definition.id).await?;
        Ok(AgentCenterDetailResponse { assistant, meta })
    }

    pub async fn create_for_user(
        &self,
        user_id: &str,
        req: CreateAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let created = self.assistants.create_for_user(user_id, req.assistant).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, &created.id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::Internal("created assistant missing definition".into()))?;

        let meta = self
            .upsert_meta(
                &definition.id,
                AgentCenterMeta::default(),
                &req.meta,
                /*keep_status*/ true,
            )
            .await?;

        // Apply MCP allowlist into assistant defaults when provided.
        if let Some(mcp_ids) = req.meta.mcp_ids.as_ref() {
            self.apply_mcp_defaults(user_id, &created.id, meta.mcp_policy, mcp_ids.clone())
                .await?;
        } else if matches!(meta.mcp_policy, AgentMcpPolicy::Allowlist) {
            // Empty allowlist = mount no MCP
            self.apply_mcp_defaults(user_id, &created.id, AgentMcpPolicy::Allowlist, Vec::new())
                .await?;
        }

        // Map skill_refs → default skill ids when provided.
        if let Some(refs) = req.meta.skill_refs.as_ref() {
            self.apply_skill_defaults(user_id, &created.id, refs).await?;
        }

        self.get_detail_for_user(user_id, &created.id, None).await
    }

    pub async fn update_for_user(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let _ = self.assistants.update_for_user(user_id, id, req.assistant).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let current = self.load_or_default_meta(&definition.id).await?;
        if current.status == AgentPublishStatus::Archived {
            return Err(AssistantError::Conflict("archived agents cannot be edited".into()));
        }
        let meta = self.upsert_meta(&definition.id, current, &req.meta, true).await?;
        if let Some(mcp_ids) = req.meta.mcp_ids.as_ref() {
            self.apply_mcp_defaults(user_id, id, meta.mcp_policy, mcp_ids.clone())
                .await?;
        }
        if let Some(refs) = req.meta.skill_refs.as_ref() {
            self.apply_skill_defaults(user_id, id, refs).await?;
        }
        self.get_detail_for_user(user_id, id, None).await
    }

    pub async fn publish_for_user(
        &self,
        user_id: &str,
        id: &str,
        req: PublishAgentCenterRequest,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let detail = self.get_detail_for_user(user_id, id, None).await?;
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;

        let mut meta = detail.meta.clone();
        if req.pin_skills_on_publish {
            for skill in &mut meta.skill_refs {
                if skill.version_policy == SkillVersionPolicy::Latest {
                    // Publish-time nail: keep key; mark as pin without inventing a version string
                    // when registry resolution is unavailable in this crate.
                    skill.version_policy = SkillVersionPolicy::Pin;
                }
            }
        }
        meta.workflow
            .validate_for_publish()
            .map_err(|message| AssistantError::BadRequest(message.into()))?;
        validate_workflow_tools(&meta, &detail.assistant.defaults.mcps.value)?;

        let next_revision = meta.version + 1;
        let revision_id = generate_prefixed_id("arev");
        let snapshot = json!({
            "assistant_id": id,
            "assistant": detail.assistant,
            "meta": meta,
            "published_at_ms": now_ms(),
            "revision": next_revision,
        });
        let snapshot_json = serde_json::to_string(&snapshot)
            .map_err(|e| AssistantError::Internal(format!("snapshot serialize: {e}")))?;

        self.revision_repo
            .create(&CreateAssistantDefinitionRevisionParams {
                id: &revision_id,
                assistant_definition_id: &definition.id,
                revision: next_revision,
                snapshot_json: &snapshot_json,
                changelog: req.changelog.as_deref(),
                created_by: Some(user_id),
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;

        let patch = AgentCenterMetaPatch {
            skill_refs: Some(meta.skill_refs.clone()),
            ..AgentCenterMetaPatch::default()
        };
        let mut base = meta;
        base.status = AgentPublishStatus::Published;
        base.version = next_revision;
        base.published_revision_id = Some(revision_id);
        let _ = self.upsert_meta_full(&definition.id, &mut base, &patch).await?;

        self.get_detail_for_user(user_id, id, None).await
    }

    /// Withdraw a published agent while retaining its immutable revision history.
    ///
    /// The editable assistant definition remains intact and becomes a draft again.
    /// A later publish creates the next revision rather than overwriting history.
    pub async fn unpublish_for_user(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<AgentCenterDetailResponse, AssistantError> {
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let mut meta = self.load_or_default_meta(&definition.id).await?;
        if meta.status != AgentPublishStatus::Published {
            return Err(AssistantError::Conflict(
                "only published agents can be unpublished".into(),
            ));
        }

        meta.status = AgentPublishStatus::Draft;
        meta.published_revision_id = None;
        let patch = AgentCenterMetaPatch::default();
        let _ = self.upsert_meta_full(&definition.id, &mut meta, &patch).await?;
        tracing::info!(
            assistant_id = %id,
            user_id = %user_id,
            version = meta.version,
            "agent-center: unpublished agent"
        );

        self.get_detail_for_user(user_id, id, None).await
    }

    pub async fn list_versions_for_user(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<Vec<AgentCenterRevisionResponse>, AssistantError> {
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(id.to_owned()))?;
        let rows = self
            .revision_repo
            .list(&definition.id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        Ok(rows
            .into_iter()
            .map(|row| AgentCenterRevisionResponse {
                id: row.id,
                revision: row.revision,
                changelog: row.changelog,
                created_by: row.created_by,
                created_at: row.created_at,
                snapshot: None,
            })
            .collect())
    }

    /// Build a `POST /api/conversations` payload from the published (or current draft) snapshot.
    pub async fn run_plan_for_user(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<AgentCenterRunPlanResponse, AssistantError> {
        let detail = self.get_detail_for_user(user_id, id, None).await?;
        if detail.meta.status == AgentPublishStatus::Archived {
            return Err(AssistantError::Conflict("archived agents cannot be run".into()));
        }

        let skill_ids: Vec<String> = if !detail.meta.skill_refs.is_empty() {
            detail.meta.skill_refs.iter().map(|s| s.skill_key.clone()).collect()
        } else {
            detail.assistant.defaults.skills.value.clone()
        };

        let mcp_ids = match detail.meta.mcp_policy {
            AgentMcpPolicy::Allowlist => Some(detail.assistant.defaults.mcps.value.clone()),
            AgentMcpPolicy::InheritUserEnabled => None, // conversation path inherits when overrides omit mcp_ids
        };

        let overrides = AssistantConversationOverridesRequest {
            model: detail.assistant.defaults.model.value.clone(),
            permission: detail.assistant.defaults.permission.value.clone(),
            thought_level: detail.assistant.defaults.thought_level.value.clone(),
            skill_ids: Some(skill_ids),
            disabled_builtin_skill_ids: Some(detail.assistant.capabilities.default_disabled_builtin_skill_ids.clone()),
            mcp_ids,
        };

        let preview_mode = match detail.meta.status {
            AgentPublishStatus::Published => AgentCenterPreviewMode::Published,
            _ => AgentCenterPreviewMode::Draft,
        };

        let mut create_conversation = CreateConversationRequestWire::for_assistant(id, Some(overrides));
        create_conversation.extra = json!({ "agent_workflow": detail.meta.workflow.clone() });

        Ok(AgentCenterRunPlanResponse {
            assistant_id: id.to_owned(),
            revision_id: detail.meta.published_revision_id.clone(),
            revision: detail.meta.version,
            preview_mode,
            workflow: detail.meta.workflow,
            create_conversation,
        })
    }

    pub async fn start_workflow_run_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
        req: StartAgentWorkflowRunRequest,
    ) -> Result<AgentWorkflowRunResponse, AssistantError> {
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, assistant_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(assistant_id.to_owned()))?;
        let detail = self.get_detail_for_user(user_id, assistant_id, None).await?;
        validate_workflow_tools(&detail.meta, &detail.assistant.defaults.mcps.value)?;
        let mut plan = self.run_plan_for_user(user_id, assistant_id).await?;
        plan.workflow
            .validate_for_publish()
            .map_err(|message| AssistantError::BadRequest(message.into()))?;

        let now = now_ms();
        let run_id = generate_prefixed_id("awrun");
        plan.create_conversation.extra["agent_workflow_run_id"] = json!(run_id.clone());
        let mut variables = req.variables;
        variables.insert("input".to_owned(), req.input);
        let mut run = AgentWorkflowRunResponse {
            id: run_id,
            assistant_id: assistant_id.to_owned(),
            status: AgentWorkflowRunStatus::Running,
            current_node_index: 1,
            workflow: plan.workflow,
            nodes: Vec::new(),
            variables,
            next_action: Some(AgentWorkflowNextAction::RunAgent {
                create_conversation: Box::new(plan.create_conversation),
            }),
            created_at: now,
            updated_at: now,
        };
        run.nodes = run
            .workflow
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| AgentWorkflowNodeRun {
                node_id: node.id.clone(),
                kind: node.kind.clone(),
                status: if index == 0 {
                    AgentWorkflowNodeRunStatus::Completed
                } else if index == 1 {
                    AgentWorkflowNodeRunStatus::Running
                } else {
                    AgentWorkflowNodeRunStatus::Pending
                },
                output: None,
                error: None,
                started_at: (index <= 1).then_some(now),
                completed_at: (index == 0).then_some(now),
            })
            .collect();
        let state_json = serialize_workflow_run(&run)?;
        self.workflow_run_repo
            .create(&CreateAgentWorkflowRunParams {
                id: &run.id,
                assistant_definition_id: &definition.id,
                user_id,
                status: workflow_run_status_str(run.status),
                state_json: &state_json,
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        tracing::info!(run_id = %run.id, assistant_id, user_id, "agent-workflow: run started");
        Ok(run)
    }

    pub async fn get_workflow_run_for_user(
        &self,
        user_id: &str,
        run_id: &str,
    ) -> Result<AgentWorkflowRunResponse, AssistantError> {
        let row = self
            .workflow_run_repo
            .get_for_user(user_id, run_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(run_id.to_owned()))?;
        parse_workflow_run(&row.state_json)
    }

    pub async fn list_workflow_runs_for_user(
        &self,
        user_id: &str,
        assistant_id: &str,
    ) -> Result<Vec<AgentWorkflowRunResponse>, AssistantError> {
        let definition = self
            .definition_repo
            .get_by_assistant_id_for_user(user_id, assistant_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(assistant_id.to_owned()))?;
        self.workflow_run_repo
            .list_for_assistant(user_id, &definition.id, 50)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .into_iter()
            .map(|row| parse_workflow_run(&row.state_json))
            .collect()
    }

    pub async fn advance_workflow_run_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        req: AdvanceAgentWorkflowRunRequest,
    ) -> Result<AgentWorkflowRunResponse, AssistantError> {
        let mut run = self.get_workflow_run_for_user(user_id, run_id).await?;
        if run.status != AgentWorkflowRunStatus::Running {
            return Err(AssistantError::Conflict(
                "workflow run is not awaiting node completion".into(),
            ));
        }
        let node = run
            .nodes
            .get_mut(run.current_node_index)
            .ok_or_else(|| AssistantError::Conflict("workflow run has no current node".into()))?;
        if node.status != AgentWorkflowNodeRunStatus::Running || !matches!(node.kind.as_str(), "agent" | "tool") {
            return Err(AssistantError::Conflict(
                "current workflow node cannot be advanced".into(),
            ));
        }
        let now = now_ms();
        if !req.success {
            node.status = AgentWorkflowNodeRunStatus::Failed;
            node.error = req.error.or_else(|| Some("node execution failed".into()));
            node.completed_at = Some(now);
            run.status = AgentWorkflowRunStatus::Failed;
            run.next_action = None;
        } else {
            node.status = AgentWorkflowNodeRunStatus::Completed;
            node.output = Some(req.output.clone());
            node.completed_at = Some(now);
            run.variables.insert(node.node_id.clone(), req.output);
            run.current_node_index += 1;
            settle_workflow_run(&mut run, now)?;
        }
        self.persist_workflow_run(user_id, run).await
    }

    pub async fn decide_workflow_approval_for_user(
        &self,
        user_id: &str,
        run_id: &str,
        req: DecideAgentWorkflowApprovalRequest,
    ) -> Result<AgentWorkflowRunResponse, AssistantError> {
        let mut run = self.get_workflow_run_for_user(user_id, run_id).await?;
        if run.status != AgentWorkflowRunStatus::WaitingApproval {
            return Err(AssistantError::Conflict(
                "workflow run is not waiting for approval".into(),
            ));
        }
        let node = run
            .nodes
            .get_mut(run.current_node_index)
            .ok_or_else(|| AssistantError::Conflict("workflow run has no approval node".into()))?;
        if node.kind != "approval" || node.status != AgentWorkflowNodeRunStatus::WaitingApproval {
            return Err(AssistantError::Conflict(
                "current workflow node is not an approval".into(),
            ));
        }
        let now = now_ms();
        node.output = Some(json!({
            "decision": match req.decision {
                AgentWorkflowApprovalDecision::Approve => "approve",
                AgentWorkflowApprovalDecision::Reject => "reject",
            },
            "comment": req.comment,
        }));
        match req.decision {
            AgentWorkflowApprovalDecision::Reject => {
                node.status = AgentWorkflowNodeRunStatus::Rejected;
                node.completed_at = Some(now);
                run.status = AgentWorkflowRunStatus::Rejected;
                run.next_action = None;
            }
            AgentWorkflowApprovalDecision::Approve => {
                node.status = AgentWorkflowNodeRunStatus::Completed;
                node.completed_at = Some(now);
                run.current_node_index += 1;
                run.status = AgentWorkflowRunStatus::Running;
                settle_workflow_run(&mut run, now)?;
            }
        }
        tracing::info!(run_id, user_id, decision = ?req.decision, "agent-workflow: approval decided");
        self.persist_workflow_run(user_id, run).await
    }

    async fn persist_workflow_run(
        &self,
        user_id: &str,
        mut run: AgentWorkflowRunResponse,
    ) -> Result<AgentWorkflowRunResponse, AssistantError> {
        run.updated_at = now_ms();
        let state_json = serialize_workflow_run(&run)?;
        self.workflow_run_repo
            .update_state(user_id, &run.id, workflow_run_status_str(run.status), &state_json)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
            .ok_or_else(|| AssistantError::NotFound(run.id.clone()))?;
        tracing::info!(run_id = %run.id, user_id, status = ?run.status, node_index = run.current_node_index, "agent-workflow: run advanced");
        Ok(run)
    }

    async fn load_or_default_meta(&self, definition_id: &str) -> Result<AgentCenterMeta, AssistantError> {
        match self
            .center_repo
            .get(definition_id)
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?
        {
            Some(row) => Ok(row_to_meta(&row)?),
            None => Ok(AgentCenterMeta::default()),
        }
    }

    async fn upsert_meta(
        &self,
        definition_id: &str,
        mut base: AgentCenterMeta,
        patch: &AgentCenterMetaPatch,
        _keep_status: bool,
    ) -> Result<AgentCenterMeta, AssistantError> {
        self.upsert_meta_full(definition_id, &mut base, patch).await
    }

    async fn upsert_meta_full(
        &self,
        definition_id: &str,
        base: &mut AgentCenterMeta,
        patch: &AgentCenterMetaPatch,
    ) -> Result<AgentCenterMeta, AssistantError> {
        if let Some(v) = patch.visibility {
            base.visibility = v;
        }
        if let Some(ref team) = patch.team_id {
            base.team_id = team.clone();
        }
        if let Some(ref ent) = patch.enterprise_id {
            base.enterprise_id = ent.clone();
        }
        if let Some(ref scopes) = patch.knowledge_scopes {
            base.knowledge_scopes = scopes.clone();
        }
        if let Some(ref refs) = patch.skill_refs {
            base.skill_refs = refs.clone();
        }
        if let Some(p) = patch.mcp_policy {
            base.mcp_policy = p;
        }
        if let Some(ref roles) = patch.role_bindings {
            base.role_bindings = roles.clone();
        }
        if let Some(ref workflow) = patch.workflow {
            workflow
                .validate()
                .map_err(|message| AssistantError::BadRequest(message.into()))?;
            base.workflow = workflow.clone();
        }

        let knowledge_scopes =
            serde_json::to_string(&base.knowledge_scopes).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let skill_refs =
            serde_json::to_string(&base.skill_refs).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let role_bindings =
            serde_json::to_string(&base.role_bindings).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let workflow_definition =
            serde_json::to_string(&base.workflow).map_err(|e| AssistantError::Internal(e.to_string()))?;
        let visibility = visibility_str(base.visibility);
        let status = status_str(base.status);
        let mcp_policy = mcp_policy_str(base.mcp_policy);

        let row = self
            .center_repo
            .upsert(&UpsertAssistantAgentCenterParams {
                assistant_definition_id: definition_id,
                visibility,
                team_id: base.team_id.as_deref(),
                enterprise_id: base.enterprise_id.as_deref(),
                status,
                version: base.version,
                published_revision_id: base.published_revision_id.as_deref(),
                knowledge_scopes: &knowledge_scopes,
                skill_refs: &skill_refs,
                mcp_policy,
                role_bindings: &role_bindings,
                workflow_definition: &workflow_definition,
            })
            .await
            .map_err(|e| AssistantError::Internal(e.to_string()))?;
        row_to_meta(&row)
    }

    async fn apply_mcp_defaults(
        &self,
        user_id: &str,
        assistant_id: &str,
        policy: AgentMcpPolicy,
        mcp_ids: Vec<String>,
    ) -> Result<(), AssistantError> {
        let mode = match policy {
            AgentMcpPolicy::Allowlist => "fixed",
            AgentMcpPolicy::InheritUserEnabled => "auto",
        };
        let update = UpdateAssistantRequest {
            defaults: Some(AssistantDefaultsRequest {
                mcps: Some(AssistantDefaultListRequest {
                    mode: mode.to_owned(),
                    value: mcp_ids,
                }),
                ..AssistantDefaultsRequest::default()
            }),
            ..UpdateAssistantRequest::default()
        };
        let _ = self.assistants.update_for_user(user_id, assistant_id, update).await?;
        Ok(())
    }

    async fn apply_skill_defaults(
        &self,
        user_id: &str,
        assistant_id: &str,
        refs: &[AgentSkillRef],
    ) -> Result<(), AssistantError> {
        let ids: Vec<String> = refs.iter().map(|r| r.skill_key.clone()).collect();
        let update = UpdateAssistantRequest {
            defaults: Some(AssistantDefaultsRequest {
                skills: Some(AssistantDefaultListRequest {
                    mode: "fixed".to_owned(),
                    value: ids.clone(),
                }),
                ..AssistantDefaultsRequest::default()
            }),
            enabled_skills: Some(ids),
            ..UpdateAssistantRequest::default()
        };
        let _ = self.assistants.update_for_user(user_id, assistant_id, update).await?;
        Ok(())
    }
}

fn row_to_meta(row: &aionui_db::AssistantAgentCenterRow) -> Result<AgentCenterMeta, AssistantError> {
    let visibility = match parse_visibility(&row.visibility) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                definition_id = %row.assistant_definition_id,
                raw = %row.visibility,
                error = %err,
                "agent-center: bad visibility; defaulting to private"
            );
            AgentVisibility::Private
        }
    };
    let status = match parse_status(&row.status) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                definition_id = %row.assistant_definition_id,
                raw = %row.status,
                error = %err,
                "agent-center: bad status; defaulting to draft"
            );
            AgentPublishStatus::Draft
        }
    };
    let mcp_policy = match parse_mcp_policy(&row.mcp_policy) {
        Ok(v) => v,
        Err(err) => {
            tracing::warn!(
                definition_id = %row.assistant_definition_id,
                raw = %row.mcp_policy,
                error = %err,
                "agent-center: bad mcp_policy; defaulting to allowlist"
            );
            AgentMcpPolicy::Allowlist
        }
    };
    Ok(AgentCenterMeta {
        visibility,
        team_id: row.team_id.clone(),
        enterprise_id: row.enterprise_id.clone(),
        status,
        version: row.version,
        published_revision_id: row.published_revision_id.clone(),
        knowledge_scopes: serde_json::from_str(&row.knowledge_scopes).unwrap_or_default(),
        skill_refs: serde_json::from_str(&row.skill_refs).unwrap_or_default(),
        mcp_policy,
        role_bindings: serde_json::from_str(&row.role_bindings).unwrap_or_default(),
        workflow: serde_json::from_str(&row.workflow_definition).unwrap_or_default(),
    })
}

fn parse_visibility(s: &str) -> Result<AgentVisibility, AssistantError> {
    match s {
        "private" => Ok(AgentVisibility::Private),
        "team" => Ok(AgentVisibility::Team),
        "enterprise" => Ok(AgentVisibility::Enterprise),
        other => Err(AssistantError::Internal(format!("bad visibility: {other}"))),
    }
}

fn parse_status(s: &str) -> Result<AgentPublishStatus, AssistantError> {
    match s {
        "draft" => Ok(AgentPublishStatus::Draft),
        "published" => Ok(AgentPublishStatus::Published),
        "archived" => Ok(AgentPublishStatus::Archived),
        other => Err(AssistantError::Internal(format!("bad status: {other}"))),
    }
}

fn parse_mcp_policy(s: &str) -> Result<AgentMcpPolicy, AssistantError> {
    match s {
        "allowlist" => Ok(AgentMcpPolicy::Allowlist),
        "inherit_user_enabled" => Ok(AgentMcpPolicy::InheritUserEnabled),
        other => Err(AssistantError::Internal(format!("bad mcp_policy: {other}"))),
    }
}

fn visibility_str(v: AgentVisibility) -> &'static str {
    match v {
        AgentVisibility::Private => "private",
        AgentVisibility::Team => "team",
        AgentVisibility::Enterprise => "enterprise",
    }
}

fn status_str(s: AgentPublishStatus) -> &'static str {
    match s {
        AgentPublishStatus::Draft => "draft",
        AgentPublishStatus::Published => "published",
        AgentPublishStatus::Archived => "archived",
    }
}

fn mcp_policy_str(p: AgentMcpPolicy) -> &'static str {
    match p {
        AgentMcpPolicy::Allowlist => "allowlist",
        AgentMcpPolicy::InheritUserEnabled => "inherit_user_enabled",
    }
}

fn workflow_run_status_str(status: AgentWorkflowRunStatus) -> &'static str {
    match status {
        AgentWorkflowRunStatus::Running => "running",
        AgentWorkflowRunStatus::WaitingApproval => "waiting_approval",
        AgentWorkflowRunStatus::Completed => "completed",
        AgentWorkflowRunStatus::Rejected => "rejected",
        AgentWorkflowRunStatus::Failed => "failed",
    }
}

fn validate_workflow_tools(meta: &AgentCenterMeta, enabled_mcp_ids: &[String]) -> Result<(), AssistantError> {
    if meta.mcp_policy != AgentMcpPolicy::Allowlist {
        return Ok(());
    }
    let has_unavailable_tool = meta
        .workflow
        .nodes
        .iter()
        .filter(|node| node.kind == "tool")
        .any(|node| {
            node.config
                .get("tool_id")
                .is_some_and(|tool_id| !enabled_mcp_ids.contains(tool_id))
        });
    if has_unavailable_tool {
        return Err(AssistantError::BadRequest(
            "workflow tool nodes must reference an enabled MCP server".into(),
        ));
    }
    Ok(())
}

fn serialize_workflow_run(run: &AgentWorkflowRunResponse) -> Result<String, AssistantError> {
    serde_json::to_string(run).map_err(|e| AssistantError::Internal(format!("workflow run serialize: {e}")))
}

fn parse_workflow_run(raw: &str) -> Result<AgentWorkflowRunResponse, AssistantError> {
    serde_json::from_str(raw).map_err(|e| AssistantError::Internal(format!("workflow run parse: {e}")))
}

fn settle_workflow_run(run: &mut AgentWorkflowRunResponse, now: i64) -> Result<(), AssistantError> {
    loop {
        let Some(definition) = run.workflow.nodes.get(run.current_node_index) else {
            run.status = AgentWorkflowRunStatus::Completed;
            run.next_action = None;
            return Ok(());
        };
        let node_id = definition.id.clone();
        let kind = definition.kind.clone();
        let config = definition.config.clone();
        let node = run
            .nodes
            .get_mut(run.current_node_index)
            .ok_or_else(|| AssistantError::Internal("workflow run node state is incomplete".into()))?;

        match kind.as_str() {
            "tool" => {
                let tool_id = config
                    .get("tool_id")
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| AssistantError::BadRequest("workflow tool node requires tool_id".into()))?;
                node.status = AgentWorkflowNodeRunStatus::Running;
                node.started_at = Some(now);
                run.status = AgentWorkflowRunStatus::Running;
                run.next_action = Some(AgentWorkflowNextAction::InvokeTool { tool_id });
                return Ok(());
            }
            "approval" => {
                let message = config
                    .get("message")
                    .filter(|value| !value.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| AssistantError::BadRequest("workflow approval node requires message".into()))?;
                node.status = AgentWorkflowNodeRunStatus::WaitingApproval;
                node.started_at = Some(now);
                run.status = AgentWorkflowRunStatus::WaitingApproval;
                run.next_action = Some(AgentWorkflowNextAction::AwaitApproval { node_id, message });
                return Ok(());
            }
            "condition" => {
                let expression = config
                    .get("expression")
                    .filter(|value| !value.trim().is_empty())
                    .ok_or_else(|| AssistantError::BadRequest("workflow condition node requires expression".into()))?;
                let passed = evaluate_guard_expression(expression, &run.variables)?;
                node.status = AgentWorkflowNodeRunStatus::Completed;
                node.started_at = Some(now);
                node.completed_at = Some(now);
                node.output = Some(serde_json::Value::Bool(passed));
                if passed {
                    run.current_node_index += 1;
                    continue;
                }
                for remaining in run.nodes.iter_mut().skip(run.current_node_index + 1) {
                    if remaining.kind == "output" {
                        remaining.status = AgentWorkflowNodeRunStatus::Completed;
                    } else {
                        remaining.status = AgentWorkflowNodeRunStatus::Skipped;
                    }
                    remaining.completed_at = Some(now);
                }
                run.current_node_index = run.nodes.len().saturating_sub(1);
                run.status = AgentWorkflowRunStatus::Completed;
                run.next_action = None;
                return Ok(());
            }
            "output" => {
                node.status = AgentWorkflowNodeRunStatus::Completed;
                node.started_at = Some(now);
                node.completed_at = Some(now);
                run.status = AgentWorkflowRunStatus::Completed;
                run.next_action = None;
                return Ok(());
            }
            _ => {
                return Err(AssistantError::Internal(format!(
                    "workflow cannot automatically settle node kind {kind}"
                )));
            }
        }
    }
}

fn evaluate_guard_expression(
    expression: &str,
    variables: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<bool, AssistantError> {
    let expression = expression.trim();
    if expression.eq_ignore_ascii_case("true") {
        return Ok(true);
    }
    if expression.eq_ignore_ascii_case("false") {
        return Ok(false);
    }
    for operator in [">=", "<=", "!=", "==", ">", "<"] {
        if let Some((left, right)) = expression.split_once(operator) {
            let actual = variables.get(left.trim()).ok_or_else(|| {
                AssistantError::BadRequest(format!("workflow condition variable is missing: {}", left.trim()))
            })?;
            let expected = parse_guard_literal(right.trim());
            return compare_guard_values(actual, &expected, operator);
        }
    }
    Err(AssistantError::BadRequest(
        "workflow condition must be true, false, or a simple variable comparison".into(),
    ))
}

fn parse_guard_literal(raw: &str) -> serde_json::Value {
    let unquoted = raw
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| raw.strip_prefix('\'').and_then(|value| value.strip_suffix('\'')));
    if let Some(value) = unquoted {
        return serde_json::Value::String(value.to_owned());
    }
    serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.to_owned()))
}

fn compare_guard_values(
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    operator: &str,
) -> Result<bool, AssistantError> {
    match operator {
        "==" => Ok(actual == expected),
        "!=" => Ok(actual != expected),
        ">" | ">=" | "<" | "<=" => {
            let left = actual
                .as_f64()
                .ok_or_else(|| AssistantError::BadRequest("workflow numeric comparison requires numbers".into()))?;
            let right = expected
                .as_f64()
                .ok_or_else(|| AssistantError::BadRequest("workflow numeric comparison requires numbers".into()))?;
            Ok(match operator {
                ">" => left > right,
                ">=" => left >= right,
                "<" => left < right,
                "<=" => left <= right,
                _ => false,
            })
        }
        _ => Err(AssistantError::BadRequest(
            "unsupported workflow condition operator".into(),
        )),
    }
}

#[cfg(test)]
mod workflow_run_tests {
    use super::*;

    #[test]
    fn guard_evaluator_supports_boolean_numeric_and_string_comparisons() {
        let variables = std::collections::BTreeMap::from([
            ("risk_score".to_owned(), json!(75)),
            ("region".to_owned(), json!("cn")),
        ]);
        assert!(evaluate_guard_expression("true", &variables).unwrap());
        assert!(evaluate_guard_expression("risk_score > 70", &variables).unwrap());
        assert!(evaluate_guard_expression("region == \"cn\"", &variables).unwrap());
    }

    #[test]
    fn guard_evaluator_rejects_arbitrary_expressions() {
        let error = evaluate_guard_expression("process.exit()", &Default::default()).unwrap_err();
        assert!(matches!(error, AssistantError::BadRequest(_)));
    }
}
