//! The runner's view of the rest of the app. Implemented in `aionui-app`
//! (assistants, worktrees, conversations, task sessions, the event bus); a
//! fake implementation drives the runner tests.

use std::path::PathBuf;

use crate::error::BatchError;
use crate::summary::BatchSummary;
use crate::validate::TargetLocation;

/// One item attempt about to run.
#[derive(Debug, Clone)]
pub struct ItemSpec {
    pub job_id: String,
    pub job_title: String,
    pub item_id: String,
    pub seq: i64,
    pub location: TargetLocation,
    pub assistant_id: String,
    pub agent_type: Option<String>,
    pub project_id: Option<String>,
}

/// Where the attempt runs: its conversation and the directory the checks
/// are evaluated in (the managed worktree, or the in-place workspace).
#[derive(Debug, Clone)]
pub struct PreparedItem {
    pub conversation_id: String,
    pub workspace: PathBuf,
}

/// Terminal outcome of one attempt, mirrored onto its TaskSession.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    Passed,
    Failed,
    Timeout,
    Cancelled,
}

/// Progress pushed to the owner's clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchEvent {
    JobUpdated {
        job_id: String,
        status: String,
    },
    ItemUpdated {
        job_id: String,
        item_id: String,
        status: String,
    },
}

#[async_trait::async_trait]
pub trait IBatchExecutor: Send + Sync {
    /// The assistant exists for the user and is enabled.
    async fn check_assistant(&self, user_id: &str, assistant_id: &str) -> Result<(), BatchError>;

    /// The target directory exists (and is a git repository for `Repo`).
    async fn check_target(&self, location: &TargetLocation) -> Result<(), BatchError>;

    /// Enough free disk for `concurrency` simultaneous worktrees of `repos`.
    async fn disk_precheck(&self, repos: &[PathBuf], concurrency: u32) -> Result<(), BatchError>;

    /// Creates the attempt's TaskSession (agent mode, running) and returns its id.
    async fn create_session(&self, user_id: &str, spec: &ItemSpec) -> Result<String, BatchError>;

    /// Creates the worktree (repo targets) and the conversation for the attempt.
    async fn prepare(&self, user_id: &str, task_session_id: &str, spec: &ItemSpec) -> Result<PreparedItem, BatchError>;

    async fn send_prompt(&self, user_id: &str, conversation_id: &str, prompt: &str) -> Result<(), BatchError>;

    /// A turn is in flight for the conversation.
    async fn is_busy(&self, conversation_id: &str) -> bool;

    /// Best-effort stop of the in-flight turn.
    async fn cancel_turn(&self, user_id: &str, conversation_id: &str);

    /// Mirrors the attempt outcome onto its TaskSession (best-effort).
    async fn finish_session(&self, user_id: &str, task_session_id: &str, outcome: AttemptOutcome, summary: &str);

    fn notify(&self, user_id: &str, event: BatchEvent);

    /// The job reached `completed` / `failed` / `cancelled`: raise (or
    /// refresh) its single Inbox summary (best-effort).
    async fn job_finished(&self, user_id: &str, summary: &BatchSummary);
}
