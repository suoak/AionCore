//! The conversation side of a fan-out, implemented by the app layer (the
//! worktree crate does not depend on the conversation crate).

use async_trait::async_trait;
use serde_json::Value;

/// What a slot conversation is created from.
#[derive(Debug, Clone)]
pub struct SlotConversationSpec {
    pub worktree_id: String,
    pub assistant_id: String,
    pub model: Option<Value>,
    pub title: String,
}

/// The first message every slot receives.
#[derive(Debug, Clone, Default)]
pub struct FanoutPrompt {
    pub content: String,
    pub files: Vec<Value>,
    pub sessions: Vec<Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotRuntime {
    /// A turn is in flight (starting, running, waiting for an approval or
    /// cancelling).
    pub busy: bool,
}

#[async_trait]
pub trait IFanoutConversations: Send + Sync {
    /// Fail fast (before any worktree is created) when the assistant does not
    /// exist for this user or cannot run (e.g. disabled).
    async fn check_assistant(&self, user_id: &str, assistant_id: &str) -> Result<(), String>;

    /// Create the slot's conversation from the assistant (its own backend,
    /// permission mode, MCP and skills; no override) bound to the worktree.
    async fn create_slot_conversation(&self, user_id: &str, spec: SlotConversationSpec) -> Result<String, String>;

    async fn send_prompt(&self, user_id: &str, conversation_id: &str, prompt: &FanoutPrompt) -> Result<(), String>;

    async fn runtime(&self, user_id: &str, conversation_id: &str) -> SlotRuntime;

    /// Stop the running turn, if any. Never deletes the conversation.
    async fn cancel_turn(&self, user_id: &str, conversation_id: &str) -> Result<(), String>;

    /// The slot's latest assistant reply text.
    async fn last_reply(&self, user_id: &str, conversation_id: &str) -> Option<String>;
}
