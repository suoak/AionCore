//! M7 E3-2 glue between managed worktrees and the conversation / SCM crates,
//! which must not depend on `aionui-worktree` directly.

use std::path::Path;
use std::sync::Arc;

use aionui_common::OnConversationDelete;
use aionui_conversation::{IWorktreeBinding, WorktreeBindingError, WorktreeBindingTarget};
use aionui_project::scm::IManagedWorktreeLookup;
use aionui_worktree::{WorktreeError, WorktreeService};
use async_trait::async_trait;

/// Statuses a conversation may bind from.
const BINDABLE: [&str; 2] = ["ready", "unbound"];

pub(crate) struct WorktreeConversationBinding {
    pub(crate) service: WorktreeService,
}

fn internal(error: WorktreeError) -> WorktreeBindingError {
    match error {
        WorktreeError::NotFound(_) => WorktreeBindingError::NotFound,
        other => WorktreeBindingError::Internal(other.to_string()),
    }
}

#[async_trait]
impl IWorktreeBinding for WorktreeConversationBinding {
    async fn resolve(&self, user_id: &str, worktree_id: &str) -> Result<WorktreeBindingTarget, WorktreeBindingError> {
        let row = self
            .service
            .binding_candidate(user_id, worktree_id)
            .await
            .map_err(internal)?;
        if !BINDABLE.contains(&row.status.as_str()) {
            return Err(WorktreeBindingError::NotReady { status: row.status });
        }
        if row.conversation_id.is_some() {
            return Err(WorktreeBindingError::AlreadyBound);
        }
        if !Path::new(&row.path).is_dir() {
            return Err(WorktreeBindingError::NotReady {
                status: "orphaned".into(),
            });
        }
        Ok(WorktreeBindingTarget {
            worktree_id: row.id,
            path: row.path,
            fanout_group_id: row.fanout_group_id,
            fanout_slot: row.slot,
        })
    }

    async fn bind(&self, user_id: &str, worktree_id: &str, conversation_id: &str) -> Result<(), WorktreeBindingError> {
        if self
            .service
            .bind_conversation(user_id, worktree_id, conversation_id)
            .await
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(WorktreeBindingError::AlreadyBound)
        }
    }
}

/// Conversation delete → bound worktrees become `unbound`; directories stay.
pub(crate) struct WorktreeUnbindOnDelete {
    pub(crate) service: WorktreeService,
}

#[async_trait]
impl OnConversationDelete for WorktreeUnbindOnDelete {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        self.service.on_conversation_deleted(user_id, conversation_id).await;
    }
}

/// SCM discover → `ScmRepository.managed_worktree_id`.
pub(crate) struct ScmManagedWorktreeLookup {
    pub(crate) service: WorktreeService,
}

#[async_trait]
impl IManagedWorktreeLookup for ScmManagedWorktreeLookup {
    async fn managed_worktree_id(&self, workdir: &Path) -> Option<String> {
        self.service.managed_id_for_path(workdir).await
    }
}

/// Wire the worktree service into the shared conversation service and the
/// TaskSession trace. Called once from `build_module_states`.
pub(crate) fn wire_worktree_bindings(services: &crate::services::AppServices, service: &WorktreeService) {
    service.set_task_sessions(Arc::new(aionui_db::SqliteTaskSessionRepository::new(
        services.database.pool().clone(),
    )));
    services
        .conversation_service
        .with_worktree_binding(Arc::new(WorktreeConversationBinding {
            service: service.clone(),
        }));
    services
        .conversation_service
        .with_delete_hook(Arc::new(WorktreeUnbindOnDelete {
            service: service.clone(),
        }));
}

/// E3-4 fan-out → conversation service. Slot conversations are ordinary
/// assistant conversations bound to the slot worktree: the assistant's own
/// backend, permission mode, MCP and skills apply (no override, never YOLO).
pub(crate) struct FanoutConversationsAdapter {
    pub(crate) conversations: aionui_conversation::ConversationService,
    pub(crate) task_manager: Arc<dyn aionui_ai_agent::IWorkerTaskManager>,
    pub(crate) assistants: Arc<aionui_assistant::AssistantService>,
}

fn reply_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        other => other.get("content").and_then(|v| v.as_str()).map(str::to_owned),
    }
}

#[async_trait]
impl aionui_worktree::IFanoutConversations for FanoutConversationsAdapter {
    async fn check_assistant(&self, user_id: &str, assistant_id: &str) -> Result<(), String> {
        let assistant = self
            .assistants
            .get_for_user(user_id, assistant_id)
            .await
            .map_err(|e| e.to_string())?;
        if !assistant.enabled {
            return Err("assistant is disabled".into());
        }
        Ok(())
    }

    async fn create_slot_conversation(
        &self,
        user_id: &str,
        spec: aionui_worktree::SlotConversationSpec,
    ) -> Result<String, String> {
        let model = match spec.model {
            Some(value) if !value.is_null() => Some(serde_json::from_value(value).map_err(|e| format!("model: {e}"))?),
            _ => None,
        };
        let req = aionui_api_types::CreateConversationRequest {
            r#type: None,
            name: Some(spec.title),
            model,
            assistant: Some(aionui_api_types::AssistantConversationRequest {
                id: spec.assistant_id,
                locale: None,
                conversation_overrides: None,
            }),
            source: None,
            channel_chat_id: None,
            extra: serde_json::json!({}),
            worktree_id: Some(spec.worktree_id),
        };
        self.conversations
            .create(user_id, req)
            .await
            .map(|c| c.id)
            .map_err(|e| e.to_string())
    }

    async fn send_prompt(
        &self,
        user_id: &str,
        conversation_id: &str,
        prompt: &aionui_worktree::FanoutPrompt,
    ) -> Result<(), String> {
        let files = prompt
            .files
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("files: {e}"))?;
        let sessions = prompt
            .sessions
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("sessions: {e}"))?;
        let req = aionui_api_types::SendMessageRequest {
            content: prompt.content.clone(),
            files,
            sessions,
            inject_skills: Vec::new(),
            hidden: false,
        };
        self.conversations
            .send_message(user_id, conversation_id, req, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn runtime(&self, _user_id: &str, conversation_id: &str) -> aionui_worktree::SlotRuntime {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        aionui_worktree::SlotRuntime {
            busy: summary.is_processing || summary.state != aionui_api_types::ConversationRuntimeStateKind::Idle,
        }
    }

    async fn cancel_turn(&self, user_id: &str, conversation_id: &str) -> Result<(), String> {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        let Some(turn_id) = summary.turn_id else {
            return Ok(());
        };
        self.conversations
            .cancel(user_id, conversation_id, &turn_id, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn last_reply(&self, user_id: &str, conversation_id: &str) -> Option<String> {
        let message = self
            .conversations
            .latest_message_of_type(user_id, conversation_id, "text")
            .await
            .ok()
            .flatten()?;
        if message.position == Some(aionui_common::MessagePosition::Right) {
            return None;
        }
        reply_text(&message.content)
    }
}

/// Build the fan-out state on the worktree service and wire its conversation
/// port. Called once from `build_module_states`.
pub(crate) fn build_fanout_state(
    services: &crate::services::AppServices,
    worktree: &WorktreeService,
    assistants: Arc<aionui_assistant::AssistantService>,
) -> aionui_worktree::FanoutRouterState {
    let service = aionui_worktree::FanoutService::new(
        worktree.clone(),
        aionui_worktree::FanoutStore::new(services.database.pool().clone()),
    );
    service.set_conversations(Arc::new(FanoutConversationsAdapter {
        conversations: services.conversation_service.clone(),
        task_manager: services.worker_task_manager.clone(),
        assistants,
    }));
    aionui_worktree::FanoutRouterState { service }
}
