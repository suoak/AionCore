//! 3.0 C7: `@assistant` delegation.
//!
//! Invariants (plan B §5.7):
//! - Only assistants the user explicitly picked in this conversation's `@`
//!   popover are delegable ([`ConversationService::grant_delegations`]).
//! - Depth 1: a delegated child conversation cannot delegate.
//! - At most [`MAX_CONCURRENT_DELEGATIONS`] unfinished delegations per
//!   conversation (enforced atomically by the insert).
//! - The child is a cold start: it receives only `task_text`, never the
//!   parent's history, and it runs in the parent's workspace with the
//!   assistant's own permission mode (no override is passed).
//! - Every delegation goes through the launch policy
//!   (`LaunchSurface::Delegation`), which is audited.
//!
//! The call is non-blocking: [`ConversationService::delegate`] returns a
//! handle; the child's final reply lands on the row when its turn settles and
//! is read with [`ConversationService::delegation_result`].

use std::sync::Arc;

use aionui_api_types::{DelegationResponse, DelegationStatus, MAX_CONCURRENT_DELEGATIONS, MAX_DELEGATION_TASK_CHARS};
use aionui_common::now_ms;
use aionui_db::{DelegationRow, IDelegationRepository, NewDelegation};
use async_trait::async_trait;

use crate::error::ConversationError;
use crate::service::ConversationService;

/// What the app launches for one delegation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DelegationLaunchSpec {
    pub delegation_id: String,
    pub parent_conversation_id: String,
    pub assistant_id: String,
    pub task_text: String,
    /// The parent's workspace; `None` lets the assistant pick its default.
    pub workspace: Option<String>,
}

/// Implemented by the app: create the child conversation from the assistant
/// and send it `task_text`. Returns the child conversation id.
#[async_trait]
pub trait DelegationLaunchPort: Send + Sync {
    async fn launch(&self, user_id: &str, spec: &DelegationLaunchSpec) -> Result<String, ConversationError>;
}

pub(crate) fn delegation_response(row: DelegationRow) -> Result<DelegationResponse, ConversationError> {
    let status = DelegationStatus::parse(&row.status)
        .ok_or_else(|| ConversationError::internal(format!("Unknown delegation status '{}'", row.status)))?;
    Ok(DelegationResponse {
        id: row.id,
        parent_conversation_id: row.parent_conversation_id,
        child_conversation_id: row.child_conversation_id,
        assistant_id: row.assistant_id,
        task_text: row.task_text,
        status,
        error: row.error,
        result_text: row.result_text,
        created_at: row.created_at,
        updated_at: row.updated_at,
    })
}

fn unprocessable(code: &str, message: impl std::fmt::Display) -> ConversationError {
    ConversationError::Unprocessable {
        reason: format!("{code}: {message}"),
    }
}

impl ConversationService {
    pub fn with_delegation_repo(&self, repo: Arc<dyn IDelegationRepository>) {
        if let Ok(mut guard) = self.delegation_repo.write() {
            *guard = Some(repo);
        }
    }

    pub fn with_delegation_launch_port(&self, port: Arc<dyn DelegationLaunchPort>) {
        if let Ok(mut guard) = self.delegation_launch.write() {
            *guard = Some(port);
        }
    }

    fn delegation_repo_optional(&self) -> Option<Arc<dyn IDelegationRepository>> {
        self.delegation_repo.read().ok().and_then(|guard| guard.clone())
    }

    fn delegation_repo(&self) -> Result<Arc<dyn IDelegationRepository>, ConversationError> {
        self.delegation_repo_optional()
            .ok_or_else(|| ConversationError::internal("Delegation is not configured"))
    }

    fn delegation_launch_port(&self) -> Result<Arc<dyn DelegationLaunchPort>, ConversationError> {
        self.delegation_launch
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| ConversationError::internal("Delegation is not configured"))
    }

    async fn owned_conversation(
        &self,
        user_id: &str,
        conversation_id: &str,
    ) -> Result<aionui_db::models::ConversationRow, ConversationError> {
        self.conversation_repo()
            .get(user_id, conversation_id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Conversation '{conversation_id}' not found")))
    }

    /// Records the user's explicit `@assistant` picks for this conversation.
    /// Called by the user-authenticated route only — never by an agent.
    pub async fn grant_delegations(
        &self,
        user_id: &str,
        conversation_id: &str,
        assistant_ids: &[String],
    ) -> Result<(), ConversationError> {
        let repo = self.delegation_repo()?;
        self.owned_conversation(user_id, conversation_id).await?;
        if repo.find_by_child(user_id, conversation_id).await?.is_some() {
            return Err(unprocessable(
                "delegation_depth",
                "A delegated conversation cannot delegate further",
            ));
        }
        let now = now_ms();
        for assistant_id in assistant_ids.iter().map(|id| id.trim()).filter(|id| !id.is_empty()) {
            repo.grant(user_id, conversation_id, assistant_id, now).await?;
        }
        Ok(())
    }

    /// Starts a delegation for the agent of `parent_conversation_id`.
    pub async fn delegate(
        &self,
        user_id: &str,
        parent_conversation_id: &str,
        assistant_id: &str,
        task_text: &str,
    ) -> Result<DelegationResponse, ConversationError> {
        let repo = self.delegation_repo()?;
        let port = self.delegation_launch_port()?;
        let assistant_id = assistant_id.trim();
        let task_text = task_text.trim();
        if assistant_id.is_empty() {
            return Err(ConversationError::bad_request("assistant_id is required"));
        }
        if task_text.is_empty() {
            return Err(ConversationError::bad_request("task_text is required"));
        }
        if task_text.chars().count() > MAX_DELEGATION_TASK_CHARS {
            return Err(ConversationError::bad_request(format!(
                "task_text is longer than {MAX_DELEGATION_TASK_CHARS} characters"
            )));
        }
        let parent = self.owned_conversation(user_id, parent_conversation_id).await?;
        if repo.find_by_child(user_id, parent_conversation_id).await?.is_some() {
            return Err(unprocessable(
                "delegation_depth",
                "A delegated conversation cannot delegate further",
            ));
        }
        if !repo.is_granted(user_id, parent_conversation_id, assistant_id).await? {
            return Err(ConversationError::Forbidden {
                reason: format!("delegation_not_granted: the user has not picked @{assistant_id} in this conversation"),
            });
        }

        let mut facts = aionui_db::LaunchFacts::new(aionui_db::LaunchSurface::Delegation);
        facts.target = Some(("conversation", parent_conversation_id));
        facts.assistant_id = Some(assistant_id);
        self.enforce_launch_policy(user_id, &facts).await?;

        let id = uuid::Uuid::new_v4().to_string();
        let created = repo
            .create_within_limit(
                &NewDelegation {
                    id: &id,
                    user_id,
                    parent_conversation_id,
                    assistant_id,
                    task_text,
                    created_at: now_ms(),
                },
                MAX_CONCURRENT_DELEGATIONS,
            )
            .await?;
        if !created {
            return Err(ConversationError::Busy {
                reason: format!(
                    "delegation_limit: {MAX_CONCURRENT_DELEGATIONS} delegations are already running here; \
                     read their results first"
                ),
            });
        }

        let workspace = serde_json::from_str::<serde_json::Value>(&parent.extra)
            .ok()
            .and_then(|extra| {
                extra
                    .get("workspace")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            })
            .filter(|path| !path.trim().is_empty());
        let spec = DelegationLaunchSpec {
            delegation_id: id.clone(),
            parent_conversation_id: parent_conversation_id.to_owned(),
            assistant_id: assistant_id.to_owned(),
            task_text: task_text.to_owned(),
            workspace,
        };
        match port.launch(user_id, &spec).await {
            Ok(child_id) => {
                // A child that already settled (sub-second) finished the row
                // through `on_delegation_turn_settled` keyed by child id, which
                // needs the id on the row first — so a lost race only means
                // the row stays `starting` → handled below.
                repo.mark_running(&id, &child_id, now_ms()).await?;
            }
            Err(error) => {
                repo.finish(&id, "failed", Some(&error.to_string()), None, now_ms())
                    .await?;
                return Err(error);
            }
        }
        tracing::info!(delegation_id = %id, parent = %parent_conversation_id, assistant = %assistant_id, "delegation started");
        self.delegation_result(user_id, parent_conversation_id, &id).await
    }

    /// The agent reads its own delegations only.
    pub async fn delegation_result(
        &self,
        user_id: &str,
        parent_conversation_id: &str,
        id: &str,
    ) -> Result<DelegationResponse, ConversationError> {
        let row = self
            .delegation_repo()?
            .get(user_id, id)
            .await?
            .filter(|row| row.parent_conversation_id == parent_conversation_id)
            .ok_or_else(|| ConversationError::not_found_reason(format!("Delegation '{id}' not found")))?;
        delegation_response(row)
    }

    pub async fn list_delegations(
        &self,
        user_id: &str,
        parent_conversation_id: &str,
    ) -> Result<Vec<DelegationResponse>, ConversationError> {
        self.owned_conversation(user_id, parent_conversation_id).await?;
        self.delegation_repo()?
            .list_for_parent(user_id, parent_conversation_id)
            .await?
            .into_iter()
            .map(delegation_response)
            .collect()
    }

    /// Called from `notify_turn_settled` for every conversation turn.
    pub(crate) async fn on_delegation_turn_settled(
        &self,
        user_id: &str,
        conversation_id: &str,
        completed: bool,
        error_message: Option<&str>,
        assistant_output: Option<&str>,
    ) {
        let Some(repo) = self.delegation_repo_optional() else {
            return;
        };
        let row = match repo.find_by_child(user_id, conversation_id).await {
            Ok(Some(row)) => row,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(%error, conversation_id, "delegation lookup failed on turn settle");
                return;
            }
        };
        let (status, error) = if completed {
            ("completed", None)
        } else {
            (
                "failed",
                Some(error_message.unwrap_or("the delegated turn did not complete")),
            )
        };
        if let Err(error) = repo.finish(&row.id, status, error, assistant_output, now_ms()).await {
            tracing::warn!(%error, delegation_id = %row.id, "failed to record a delegation result");
        }
    }

    /// Boot: children of a previous run cannot report back any more.
    pub async fn fail_interrupted_delegations(&self) -> Result<u64, ConversationError> {
        match self.delegation_repo_optional() {
            Some(repo) => Ok(repo.fail_unfinished(now_ms()).await?),
            None => Ok(0),
        }
    }
}
