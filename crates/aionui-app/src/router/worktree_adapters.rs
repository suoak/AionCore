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
