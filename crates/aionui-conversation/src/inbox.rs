//! M7 E5-4: user inbox (approvals, blocked runs, completions, failures).
//!
//! Read side and read-state mutations. Producers upsert by `dedupe_key` so a
//! repeated signal refreshes one row instead of stacking duplicates.

use crate::error::ConversationError;
use crate::service::ConversationService;
use aionui_api_types::{AgentErrorCode, WebSocketMessage};
use aionui_api_types::{InboxItemDto, InboxListQuery, InboxListResponse, InboxMarkReadResponse};
use aionui_db::{IInboxRepository, InboxFilter, InboxItemRow, UpsertInboxItemParams};
use aionui_realtime::EventBroadcaster;
use std::sync::Arc;

/// WebSocket event telling clients to refetch the inbox / unread badge.
pub(crate) const EVENT_INBOX_CHANGED: &str = "inbox.changed";

/// WebSocket event: a work item's Plan/Goal approval was requested or
/// decided (M8 E9). Lets the desktop react when the phone answers.
pub(crate) const EVENT_TASK_APPROVAL_CHANGED: &str = "task.approval.changed";

/// Inbox dedupe key of one Plan/Goal approval.
pub(crate) fn task_approval_inbox_key(approval_id: &str) -> String {
    format!("task_approval:{approval_id}")
}

/// Items older than this are pruned lazily on list.
pub(crate) const INBOX_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Per-user cap on stored items.
pub(crate) const INBOX_MAX_ITEMS: i64 = 5000;
const DEFAULT_LIST_LIMIT: i64 = 50;
const MAX_LIST_LIMIT: i64 = 200;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn to_dto(row: InboxItemRow) -> InboxItemDto {
    let origin = aionui_api_types::inbox_origin(&row.dedupe_key, &row.kind, &row.title).map(str::to_owned);
    InboxItemDto {
        origin,
        id: row.id,
        kind: row.kind,
        source_kind: row.source_kind,
        source_id: row.source_id,
        conversation_id: row.conversation_id,
        task_session_id: row.task_session_id,
        run_id: row.run_id,
        title: row.title,
        summary: row.summary,
        created_at: row.created_at,
        read_at: row.read_at,
        resolved_at: row.resolved_at,
    }
}

pub(crate) async fn list_inbox(
    repo: &dyn IInboxRepository,
    user_id: &str,
    query: InboxListQuery,
    now: i64,
) -> Result<InboxListResponse, ConversationError> {
    if let Err(error) = repo
        .prune_for_user(user_id, now - INBOX_RETENTION_MS, INBOX_MAX_ITEMS)
        .await
    {
        tracing::warn!(user_id, error = %error, "Failed to prune inbox before listing");
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT);
    let filter = InboxFilter {
        unread_only: query.unread_only.unwrap_or(false),
        before: query.before,
        limit,
    };
    let items = repo.list(user_id, &filter).await?.into_iter().map(to_dto).collect();
    let unread_count = repo.unread_count(user_id).await?;
    Ok(InboxListResponse { items, unread_count })
}

/// How a user turn settled, for the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TurnInboxKind {
    Completed,
    Failed,
    /// Stopped on something only the user can fix (sign-in, billing, setup).
    Blocked,
}

impl TurnInboxKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
        }
    }
}

/// Error codes that need the user to act before a retry can succeed.
pub(crate) fn error_code_blocks_on_user(code: Option<AgentErrorCode>) -> bool {
    matches!(
        code,
        Some(
            AgentErrorCode::UserAgentAuthRequired
                | AgentErrorCode::UserLlmProviderAuthFailed
                | AgentErrorCode::UserLlmProviderAwsSsoExpired
                | AgentErrorCode::UserLlmProviderPermissionDenied
                | AgentErrorCode::UserLlmProviderBillingRequired
                | AgentErrorCode::UserLlmProviderConfigError
                | AgentErrorCode::UserAgentNotInstalled
                | AgentErrorCode::UserAgentMissingEnv
        )
    )
}

/// Task-run origin of a signal; `None` means a plain conversation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct InboxTaskRef<'a> {
    pub task_session_id: &'a str,
    pub run_id: &'a str,
}

pub(crate) fn approval_dedupe_key(conversation_id: &str, call_id: &str) -> String {
    format!("approval:{conversation_id}:{call_id}")
}

fn turn_dedupe_key(conversation_id: &str, task: Option<InboxTaskRef<'_>>) -> String {
    match task {
        Some(task) => format!("run:{}", task.run_id),
        None => format!("turn:{conversation_id}"),
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('…');
    }
    out
}

/// Producer side: upsert/resolve items and announce `inbox.changed`.
/// All failures are logged and swallowed — the inbox never breaks a turn.
#[derive(Clone)]
pub(crate) struct InboxSink {
    repo: Arc<dyn IInboxRepository>,
    broadcaster: Arc<dyn EventBroadcaster>,
    push: Option<Arc<dyn InboxPushHook>>,
}

/// Inbox kinds that ask for the user's attention away from the desktop and
/// are handed to the [`InboxPushHook`] (M8 E9-6).
pub const INBOX_PUSH_KINDS: &[&str] = &["approval", "blocked"];

/// Notified after an attention-worthy inbox item (see [`INBOX_PUSH_KINDS`])
/// is raised or re-opened. Must not block: implementations spawn their own
/// delivery work.
pub trait InboxPushHook: Send + Sync {
    fn item_raised(&self, item: &InboxItemRow);
}

impl InboxSink {
    pub(crate) fn new(repo: Arc<dyn IInboxRepository>, broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            repo,
            broadcaster,
            push: None,
        }
    }

    pub(crate) fn with_push(mut self, push: Option<Arc<dyn InboxPushHook>>) -> Self {
        self.push = push;
        self
    }

    pub(crate) async fn announce(&self, user_id: &str, kind: Option<&str>, conversation_id: Option<&str>) {
        let unread_count = match self.repo.unread_count(user_id).await {
            Ok(count) => count,
            Err(error) => {
                tracing::warn!(user_id, error = %error, "inbox unread count failed");
                return;
            }
        };
        self.broadcaster.broadcast(WebSocketMessage::new(
            EVENT_INBOX_CHANGED,
            serde_json::json!({
                "user_id": user_id,
                "unread_count": unread_count,
                "kind": kind,
                "conversation_id": conversation_id,
            }),
        ));
    }

    async fn upsert(&self, params: &UpsertInboxItemParams<'_>) {
        match self.repo.upsert(params).await {
            Ok(item) => {
                self.announce(params.user_id, Some(params.kind), params.conversation_id)
                    .await;
                if let Some(push) = self.push.as_ref()
                    && INBOX_PUSH_KINDS.contains(&item.kind.as_str())
                {
                    push.item_raised(&item);
                }
            }
            Err(error) => tracing::warn!(
                user_id = params.user_id,
                kind = params.kind,
                error = %error,
                "inbox upsert failed"
            ),
        }
    }

    /// A tool call is waiting for the user's approval.
    pub(crate) async fn approval_asked(
        &self,
        user_id: &str,
        conversation_id: &str,
        task: Option<InboxTaskRef<'_>>,
        call_id: &str,
        tool_name: Option<&str>,
    ) {
        let key = approval_dedupe_key(conversation_id, call_id);
        let summary = tool_name.map(|name| truncate(name, 200));
        self.upsert(&UpsertInboxItemParams {
            user_id,
            kind: "approval",
            source_kind: if task.is_some() { "task_run" } else { "conversation" },
            source_id: task.map_or(conversation_id, |t| t.run_id),
            conversation_id: Some(conversation_id),
            task_session_id: task.map(|t| t.task_session_id),
            run_id: task.map(|t| t.run_id),
            title: "Approval needed",
            summary: summary.as_deref(),
            dedupe_key: &key,
            created_at: now_ms(),
        })
        .await;
    }

    /// The user (or policy) answered one approval.
    pub(crate) async fn approval_resolved(&self, user_id: &str, conversation_id: &str, call_id: &str) {
        let key = approval_dedupe_key(conversation_id, call_id);
        match self.repo.resolve_by_dedupe_key(user_id, &key, now_ms()).await {
            Ok(true) => self.announce(user_id, Some("approval"), Some(conversation_id)).await,
            Ok(false) => {}
            Err(error) => tracing::warn!(user_id, conversation_id, error = %error, "inbox resolve failed"),
        }
    }

    /// A user turn settled: close its open approvals and record the outcome.
    pub(crate) async fn turn_settled(
        &self,
        user_id: &str,
        conversation_id: &str,
        conversation_name: &str,
        task: Option<InboxTaskRef<'_>>,
        kind: TurnInboxKind,
        error_message: Option<&str>,
    ) {
        if let Err(error) = self
            .repo
            .resolve_open_approvals(user_id, conversation_id, now_ms())
            .await
        {
            tracing::warn!(user_id, conversation_id, error = %error, "inbox approval sweep failed");
        }
        let key = turn_dedupe_key(conversation_id, task);
        let title = if conversation_name.trim().is_empty() {
            "Conversation".to_owned()
        } else {
            truncate(conversation_name.trim(), 120)
        };
        let summary = match kind {
            TurnInboxKind::Completed => None,
            TurnInboxKind::Failed | TurnInboxKind::Blocked => error_message.map(|m| truncate(m, 300)),
        };
        self.upsert(&UpsertInboxItemParams {
            user_id,
            kind: kind.as_str(),
            source_kind: if task.is_some() { "task_run" } else { "conversation" },
            source_id: task.map_or(conversation_id, |t| t.run_id),
            conversation_id: Some(conversation_id),
            task_session_id: task.map(|t| t.task_session_id),
            run_id: task.map(|t| t.run_id),
            title: &title,
            summary: summary.as_deref(),
            dedupe_key: &key,
            created_at: now_ms(),
        })
        .await;
    }
}

impl ConversationService {
    pub(crate) fn inbox_sink(&self) -> Option<InboxSink> {
        self.inbox_repo()
            .map(|repo| InboxSink::new(repo, self.broadcaster().clone()).with_push(self.inbox_push_hook()))
    }

    /// Raise (or refresh, by `dedupe_key`) an inbox item for a producer that
    /// lives outside the turn runtime, e.g. cron Autopilot failures. Errors
    /// are logged and swallowed like every other inbox producer.
    pub async fn raise_inbox_item(&self, params: &UpsertInboxItemParams<'_>) {
        if let Some(sink) = self.inbox_sink() {
            sink.upsert(params).await;
        }
    }

    /// Resolve the open item with `dedupe_key`, if any (see `raise_inbox_item`).
    pub async fn resolve_inbox_item(&self, user_id: &str, dedupe_key: &str) {
        let Some(sink) = self.inbox_sink() else {
            return;
        };
        match sink.repo.resolve_by_dedupe_key(user_id, dedupe_key, now_ms()).await {
            Ok(true) => sink.announce(user_id, None, None).await,
            Ok(false) => {}
            Err(error) => tracing::warn!(user_id, dedupe_key, error = %error, "inbox resolve failed"),
        }
    }

    /// A work item's Plan/Goal approval changed (M8 E9): tell clients, and
    /// keep its Inbox `approval` item in step — open while pending (which
    /// also reaches IM push for paired devices), resolved otherwise.
    /// `superseded` are earlier approvals the new one cancelled or expired.
    pub(crate) async fn task_approval_changed(
        &self,
        user_id: &str,
        task_title: Option<&str>,
        approval: &aionui_db::models::TaskApprovalRow,
        superseded: &[String],
    ) {
        self.broadcaster().broadcast(WebSocketMessage::new(
            EVENT_TASK_APPROVAL_CHANGED,
            serde_json::json!({
                "user_id": user_id,
                "task_session_id": approval.task_session_id,
                "approval_id": approval.id,
                "approval_type": approval.approval_type,
                "status": approval.status,
            }),
        ));
        for id in superseded {
            self.resolve_inbox_item(user_id, &task_approval_inbox_key(id)).await;
        }
        let key = task_approval_inbox_key(&approval.id);
        if approval.status != "pending" {
            self.resolve_inbox_item(user_id, &key).await;
            return;
        }
        let label = if approval.approval_type == "goal" {
            "Goal"
        } else {
            "Plan"
        };
        let title = match task_title.map(str::trim).filter(|t| !t.is_empty()) {
            Some(task_title) => format!("{label} approval: {}", truncate(task_title, 100)),
            None => format!("{label} approval needed"),
        };
        self.raise_inbox_item(&UpsertInboxItemParams {
            user_id,
            kind: "approval",
            // 069 has no approval source kind; the item points at the work
            // item through `task_session_id` (and the planning run, if any).
            source_kind: "task_run",
            source_id: approval.run_id.as_deref().unwrap_or(&approval.task_session_id),
            conversation_id: None,
            task_session_id: Some(&approval.task_session_id),
            run_id: approval.run_id.as_deref(),
            title: &title,
            summary: None,
            dedupe_key: &key,
            created_at: now_ms(),
        })
        .await;
    }

    /// Dedupe key of the inbox item a plain (non-task) conversation turn
    /// raises when it settles; external producers reuse it to refresh that
    /// item instead of stacking a second one for the same failure.
    pub fn conversation_turn_inbox_key(conversation_id: &str) -> String {
        turn_dedupe_key(conversation_id, None)
    }

    pub async fn list_inbox(
        &self,
        user_id: &str,
        query: InboxListQuery,
    ) -> Result<InboxListResponse, ConversationError> {
        let Some(repo) = self.inbox_repo() else {
            return Ok(InboxListResponse {
                items: Vec::new(),
                unread_count: 0,
            });
        };
        list_inbox(repo.as_ref(), user_id, query, now_ms()).await
    }

    /// Mark one item read. Unknown or foreign ids are a no-op (`updated: 0`).
    pub async fn mark_inbox_read(&self, user_id: &str, id: &str) -> Result<InboxMarkReadResponse, ConversationError> {
        let Some(repo) = self.inbox_repo() else {
            return Ok(InboxMarkReadResponse {
                updated: 0,
                unread_count: 0,
            });
        };
        let updated = u64::from(repo.mark_read(user_id, id, now_ms()).await?);
        let unread_count = repo.unread_count(user_id).await?;
        if updated > 0
            && let Some(sink) = self.inbox_sink()
        {
            sink.announce(user_id, None, None).await;
        }
        Ok(InboxMarkReadResponse { updated, unread_count })
    }

    pub async fn mark_all_inbox_read(&self, user_id: &str) -> Result<InboxMarkReadResponse, ConversationError> {
        let Some(repo) = self.inbox_repo() else {
            return Ok(InboxMarkReadResponse {
                updated: 0,
                unread_count: 0,
            });
        };
        let updated = repo.mark_all_read(user_id, now_ms()).await?;
        let unread_count = repo.unread_count(user_id).await?;
        if updated > 0
            && let Some(sink) = self.inbox_sink()
        {
            sink.announce(user_id, None, None).await;
        }
        Ok(InboxMarkReadResponse { updated, unread_count })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_db::{SqliteInboxRepository, UpsertInboxItemParams};

    async fn repo() -> SqliteInboxRepository {
        let db = aionui_db::init_database_memory().await.expect("db");
        SqliteInboxRepository::new(db.pool().clone())
    }

    fn item<'a>(user: &'a str, key: &'a str, at: i64) -> UpsertInboxItemParams<'a> {
        UpsertInboxItemParams {
            user_id: user,
            kind: "completed",
            source_kind: "conversation",
            source_id: "c1",
            conversation_id: Some("c1"),
            task_session_id: None,
            run_id: None,
            title: "Done",
            summary: None,
            dedupe_key: key,
            created_at: at,
        }
    }

    #[tokio::test]
    async fn list_clamps_limit_and_reports_unread() {
        let repo = repo().await;
        let now = 10_000_000_000;
        for i in 0..3 {
            repo.upsert(&item("u1", &format!("k{i}"), now - i))
                .await
                .expect("upsert");
        }
        repo.upsert(&item("u2", "k0", now)).await.expect("other user");
        let query = InboxListQuery {
            limit: Some(0),
            ..InboxListQuery::default()
        };
        let resp = list_inbox(&repo, "u1", query, now).await.expect("list");
        assert_eq!(resp.items.len(), 1, "limit clamps to >= 1");
        assert_eq!(resp.unread_count, 3, "unread counts only this user");
    }

    #[tokio::test]
    async fn list_prunes_expired_items() {
        let repo = repo().await;
        let now = 10_000_000_000;
        repo.upsert(&item("u1", "old", now - INBOX_RETENTION_MS - 1))
            .await
            .expect("old");
        repo.upsert(&item("u1", "new", now)).await.expect("new");
        let resp = list_inbox(&repo, "u1", InboxListQuery::default(), now)
            .await
            .expect("list");
        assert_eq!(resp.items.len(), 1);
        assert_eq!(resp.unread_count, 1);
    }

    #[derive(Default)]
    struct Recorder(std::sync::Mutex<Vec<WebSocketMessage<serde_json::Value>>>);

    impl EventBroadcaster for Recorder {
        fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
            self.0.lock().expect("lock").push(event);
        }
    }

    async fn sink() -> (InboxSink, Arc<SqliteInboxRepository>, Arc<Recorder>) {
        let repo = Arc::new(repo().await);
        let rec = Arc::new(Recorder::default());
        (InboxSink::new(repo.clone(), rec.clone()), repo, rec)
    }

    #[derive(Default)]
    struct PushRecorder(std::sync::Mutex<Vec<(String, String)>>);

    impl InboxPushHook for PushRecorder {
        fn item_raised(&self, item: &InboxItemRow) {
            self.0.lock().expect("lock").push((item.kind.clone(), item.id.clone()));
        }
    }

    #[tokio::test]
    async fn only_approval_and_blocked_items_are_pushed() {
        let (sink, repo, _rec) = sink().await;
        let push = Arc::new(PushRecorder::default());
        let sink = sink.with_push(Some(push.clone()));
        sink.approval_asked("u1", "c1", None, "call-1", Some("Bash")).await;
        sink.turn_settled("u1", "c1", "Chat", None, TurnInboxKind::Completed, None)
            .await;
        sink.turn_settled("u1", "c2", "Chat", None, TurnInboxKind::Failed, Some("boom"))
            .await;
        sink.turn_settled("u1", "c3", "Chat", None, TurnInboxKind::Blocked, Some("needs input"))
            .await;
        let pushed = push.0.lock().expect("lock").clone();
        let kinds: Vec<&str> = pushed.iter().map(|(kind, _)| kind.as_str()).collect();
        assert_eq!(kinds, ["approval", "blocked"]);
        let stored = repo.list("u1", &all()).await.expect("list");
        assert!(pushed.iter().all(|(_, id)| stored.iter().any(|row| &row.id == id)));
    }

    fn all() -> InboxFilter {
        InboxFilter {
            unread_only: false,
            before: None,
            limit: 50,
        }
    }

    #[tokio::test]
    async fn approval_is_announced_then_resolved() {
        let (sink, repo, rec) = sink().await;
        sink.approval_asked("u1", "c1", None, "call-1", Some("Bash")).await;
        sink.approval_asked("u1", "c1", None, "call-1", Some("Bash")).await;
        let items = repo.list("u1", &all()).await.expect("list");
        assert_eq!(items.len(), 1, "same call dedupes");
        assert_eq!(items[0].kind, "approval");
        assert_eq!(items[0].summary.as_deref(), Some("Bash"));
        {
            let events = rec.0.lock().expect("lock");
            assert_eq!(events.last().map(|e| e.name.as_str()), Some(EVENT_INBOX_CHANGED));
            assert_eq!(events.last().and_then(|e| e.data["unread_count"].as_i64()), Some(1));
        }
        sink.approval_resolved("u1", "c1", "call-1").await;
        assert_eq!(repo.unread_count("u1").await.expect("count"), 0);
        let items = repo.list("u1", &all()).await.expect("list");
        assert!(items[0].resolved_at.is_some());
        let before = rec.0.lock().expect("lock").len();
        sink.approval_resolved("u1", "c1", "call-1").await;
        assert_eq!(rec.0.lock().expect("lock").len(), before, "no-op resolve stays quiet");
    }

    #[tokio::test]
    async fn turn_settled_sweeps_approvals_and_keeps_latest_outcome() {
        let (sink, repo, _rec) = sink().await;
        sink.approval_asked("u1", "c1", None, "call-1", None).await;
        sink.turn_settled("u1", "c1", "Fix build", None, TurnInboxKind::Failed, Some("boom"))
            .await;
        let items = repo.list("u1", &all()).await.expect("list");
        let failed = items.iter().find(|i| i.kind == "failed").expect("failed item");
        assert_eq!(failed.title, "Fix build");
        assert_eq!(failed.summary.as_deref(), Some("boom"));
        let approval = items.iter().find(|i| i.kind == "approval").expect("approval");
        assert!(approval.resolved_at.is_some(), "settled turn closes pending asks");

        sink.turn_settled("u1", "c1", "Fix build", None, TurnInboxKind::Completed, Some("ignored"))
            .await;
        let items = repo.list("u1", &all()).await.expect("list");
        let turn_items: Vec<_> = items.iter().filter(|i| i.kind != "approval").collect();
        assert_eq!(turn_items.len(), 1, "one row per conversation, latest wins");
        assert_eq!(turn_items[0].kind, "completed");
        assert_eq!(turn_items[0].summary, None);
    }

    #[tokio::test]
    async fn task_runs_get_their_own_rows() {
        let (sink, repo, _rec) = sink().await;
        let task = InboxTaskRef {
            task_session_id: "t1",
            run_id: "r1",
        };
        sink.turn_settled("u1", "c1", "", Some(task), TurnInboxKind::Blocked, Some("sign in"))
            .await;
        let items = repo.list("u1", &all()).await.expect("list");
        assert_eq!(items[0].source_kind, "task_run");
        assert_eq!(items[0].source_id, "r1");
        assert_eq!(items[0].task_session_id.as_deref(), Some("t1"));
        assert_eq!(items[0].title, "Conversation");
        assert_eq!(items[0].kind, "blocked");
    }

    #[tokio::test]
    async fn batch_and_autopilot_items_carry_their_origin() {
        let repo = repo().await;
        let batch = UpsertInboxItemParams {
            kind: "failed",
            source_kind: "fanout_group",
            source_id: "bj_1",
            conversation_id: None,
            title: "Batch failed: Release check",
            summary: Some("1 passed · 1 failed · 0 timed out (of 2)"),
            ..item("u1", "batch:bj_1", 10)
        };
        repo.upsert(&batch).await.expect("batch upsert");
        let autopilot = UpsertInboxItemParams {
            kind: "failed",
            title: "Autopilot: Nightly",
            ..item("u1", "turn:c1", 20)
        };
        repo.upsert(&autopilot).await.expect("autopilot upsert");
        repo.upsert(&item("u1", "turn:c2", 30)).await.expect("plain upsert");
        // A re-run refreshes the same batch row instead of adding one.
        repo.upsert(&UpsertInboxItemParams {
            kind: "completed",
            title: "Batch completed: Release check",
            created_at: 40,
            ..batch
        })
        .await
        .expect("batch refresh");

        let res = list_inbox(&repo, "u1", InboxListQuery::default(), 50)
            .await
            .expect("list");
        assert_eq!(res.items.len(), 3);
        let by_source = |id: &str| res.items.iter().find(|i| i.source_id == id).expect("item");
        let b = by_source("bj_1");
        assert_eq!((b.origin.as_deref(), b.kind.as_str()), (Some("batch"), "completed"));
        assert_eq!(b.source_kind, "fanout_group");
        let plain: Vec<_> = res.items.iter().filter(|i| i.source_id == "c1").collect();
        assert_eq!(plain.len(), 2);
        let a = plain
            .iter()
            .find(|i| i.title.starts_with("Autopilot"))
            .expect("autopilot");
        assert_eq!(a.origin.as_deref(), Some("autopilot"));
        assert!(plain.iter().any(|i| i.origin.is_none()));
    }

    #[test]
    fn blocking_codes_need_user_action() {
        assert!(error_code_blocks_on_user(Some(
            AgentErrorCode::UserLlmProviderBillingRequired
        )));
        assert!(error_code_blocks_on_user(Some(AgentErrorCode::UserAgentAuthRequired)));
        assert!(!error_code_blocks_on_user(Some(AgentErrorCode::AionuiStreamBroken)));
        assert!(!error_code_blocks_on_user(None));
    }
}
