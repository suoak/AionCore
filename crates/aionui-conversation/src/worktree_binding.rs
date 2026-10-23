//! Conversation ↔ managed worktree binding port (M7 E3-2).
//!
//! A conversation created with `worktree_id` runs inside a managed worktree
//! owned by the worktree service. This crate must not depend on that service,
//! so it talks to it through [`IWorktreeBinding`], implemented by the
//! composition layer. Unbinding on conversation delete goes through the
//! regular `OnConversationDelete` hook, registered by the same layer.

use async_trait::async_trait;

use crate::error::ConversationError;

/// `conversation.extra` keys owned by the worktree binding. Only the server
/// writes them: stripped from every client create body, rejected on PATCH.
pub const WORKTREE_BINDING_EXTRA_KEYS: [&str; 3] = ["worktree_id", "fanout_group_id", "fanout_slot"];

/// What a conversation needs to know about a worktree it is about to bind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeBindingTarget {
    pub worktree_id: String,
    /// Absolute worktree directory; becomes `extra.workspace`.
    pub path: String,
    pub fanout_group_id: Option<String>,
    pub fanout_slot: Option<String>,
}

/// Why a binding was refused. Each variant maps to a stable HTTP status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorktreeBindingError {
    /// Unknown id, or owned by another user (indistinguishable on purpose).
    NotFound,
    /// The worktree is not in a bindable state (`creating`, `failed`, ...).
    NotReady {
        status: String,
    },
    /// Another conversation already holds it.
    AlreadyBound,
    Internal(String),
}

impl From<WorktreeBindingError> for ConversationError {
    fn from(error: WorktreeBindingError) -> Self {
        match error {
            WorktreeBindingError::NotFound => ConversationError::NotFoundReason {
                reason: "WORKTREE_NOT_FOUND: worktree not found".into(),
            },
            WorktreeBindingError::NotReady { status } => ConversationError::Busy {
                reason: format!("WORKTREE_INVALID_STATE: worktree is {status}, not ready"),
            },
            WorktreeBindingError::AlreadyBound => ConversationError::Busy {
                reason: "WORKTREE_ALREADY_BOUND: worktree is already bound to a conversation".into(),
            },
            WorktreeBindingError::Internal(message) => ConversationError::internal(message),
        }
    }
}

/// Port to the managed worktree service.
#[async_trait]
pub trait IWorktreeBinding: Send + Sync {
    /// Look up a bindable worktree of `user_id`: `ready`, or `unbound` after
    /// its previous conversation was deleted, and not held by a conversation.
    async fn resolve(&self, user_id: &str, worktree_id: &str) -> Result<WorktreeBindingTarget, WorktreeBindingError>;

    /// Atomically attach `conversation_id`. Must fail with `AlreadyBound` when
    /// another conversation won the race since [`IWorktreeBinding::resolve`].
    async fn bind(&self, user_id: &str, worktree_id: &str, conversation_id: &str) -> Result<(), WorktreeBindingError>;
}

/// Remove server-owned worktree keys from a client create body. Returns
/// whether anything was removed.
pub(crate) fn strip_client_worktree_binding(extra: &mut serde_json::Value) -> bool {
    let Some(obj) = extra.as_object_mut() else {
        return false;
    };
    let mut stripped = false;
    for key in WORKTREE_BINDING_EXTRA_KEYS {
        stripped |= obj.remove(key).is_some();
    }
    stripped
}

/// The worktree id a stored `extra` is bound to, if any.
pub(crate) fn bound_worktree_id(extra: &serde_json::Value) -> Option<&str> {
    extra
        .get("worktree_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

/// Whether `patch` would move a worktree-bound conversation off its worktree.
/// Re-sending the stored workspace unchanged is not a move.
pub(crate) fn patch_moves_bound_workspace(existing: &serde_json::Value, patch: &serde_json::Value) -> bool {
    bound_worktree_id(existing).is_some()
        && patch
            .get("workspace")
            .is_some_and(|value| existing.get("workspace") != Some(value))
}

pub(crate) const WORKSPACE_LOCKED_REASON: &str =
    "WORKSPACE_LOCKED: this conversation is bound to a managed worktree; its workspace cannot change";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::service::check_protected_extra_patch;

    #[test]
    fn strip_removes_every_binding_key() {
        let mut extra = json!({ "worktree_id": "w", "fanout_group_id": "g", "fanout_slot": "A", "keep": 1 });
        assert!(strip_client_worktree_binding(&mut extra));
        assert_eq!(extra, json!({ "keep": 1 }));
        assert!(!strip_client_worktree_binding(&mut extra));
        assert!(!strip_client_worktree_binding(&mut json!(null)));
    }

    #[test]
    fn workspace_lock_only_applies_to_bound_conversations() {
        let bound = json!({ "worktree_id": "w", "workspace": "/wt" });
        assert!(patch_moves_bound_workspace(
            &bound,
            &json!({ "workspace": "/elsewhere" })
        ));
        assert!(patch_moves_bound_workspace(&bound, &json!({ "workspace": null })));
        assert!(!patch_moves_bound_workspace(&bound, &json!({ "workspace": "/wt" })));
        assert!(!patch_moves_bound_workspace(&bound, &json!({ "name": "x" })));
        let blank = json!({ "worktree_id": " ", "workspace": "/a" });
        assert!(!patch_moves_bound_workspace(&blank, &json!({ "workspace": "/b" })));
        let unbound = json!({ "workspace": "/a" });
        assert!(!patch_moves_bound_workspace(&unbound, &json!({ "workspace": "/b" })));
    }

    #[test]
    fn binding_keys_are_protected_on_patch() {
        let existing = json!({ "worktree_id": "w", "fanout_slot": "A" });
        for incoming in [
            json!({ "worktree_id": "other" }),
            json!({ "worktree_id": null }),
            json!({ "fanout_slot": "B" }),
            json!({ "fanout_group_id": "g" }),
        ] {
            assert!(
                check_protected_extra_patch(&existing, &incoming, false).is_err(),
                "{incoming}"
            );
        }
        assert!(
            check_protected_extra_patch(&existing, &json!({ "worktree_id": "w", "fanout_slot": "A" }), false).is_ok()
        );
    }
}
