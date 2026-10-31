use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::InboxItemRow;

/// Upsert payload. `id` is minted by the repository.
#[derive(Debug, Clone)]
pub struct UpsertInboxItemParams<'a> {
    pub user_id: &'a str,
    /// `approval` | `blocked` | `completed` | `failed`.
    pub kind: &'a str,
    /// `conversation` | `task_run` | `fanout_group` | `team`.
    pub source_kind: &'a str,
    pub source_id: &'a str,
    pub conversation_id: Option<&'a str>,
    pub task_session_id: Option<&'a str>,
    pub run_id: Option<&'a str>,
    pub title: &'a str,
    pub summary: Option<&'a str>,
    /// Producer-chosen identity; re-raising the same key refreshes the item
    /// (unread, unresolved) instead of duplicating it.
    pub dedupe_key: &'a str,
    pub created_at: TimestampMs,
}

#[derive(Debug, Clone, Default)]
pub struct InboxFilter {
    pub unread_only: bool,
    /// Exclusive `created_at` cursor for paging backwards.
    pub before: Option<TimestampMs>,
    pub limit: i64,
}

#[async_trait::async_trait]
pub trait IInboxRepository: Send + Sync {
    /// Insert, or refresh an existing item with the same `(user_id, dedupe_key)`:
    /// content and `created_at` are replaced and `read_at` / `resolved_at` cleared.
    async fn upsert(&self, params: &UpsertInboxItemParams<'_>) -> Result<InboxItemRow, DbError>;

    /// Newest first.
    async fn list(&self, user_id: &str, filter: &InboxFilter) -> Result<Vec<InboxItemRow>, DbError>;

    /// One item of this user, if it exists (M8 E9-7).
    async fn get(&self, user_id: &str, id: &str) -> Result<Option<InboxItemRow>, DbError>;

    /// Unread and unresolved items.
    async fn unread_count(&self, user_id: &str) -> Result<i64, DbError>;

    /// Returns false when the item does not exist for this user.
    async fn mark_read(&self, user_id: &str, id: &str, at: TimestampMs) -> Result<bool, DbError>;

    async fn mark_all_read(&self, user_id: &str, at: TimestampMs) -> Result<u64, DbError>;

    /// Resolve (and mark read) the item with this dedupe key, if any.
    async fn resolve_by_dedupe_key(&self, user_id: &str, dedupe_key: &str, at: TimestampMs) -> Result<bool, DbError>;

    /// Resolve every still-open approval item of one conversation (its turn
    /// settled, so the pending asks can no longer be answered). Returns rows touched.
    async fn resolve_open_approvals(
        &self,
        user_id: &str,
        conversation_id: &str,
        at: TimestampMs,
    ) -> Result<u64, DbError>;

    /// Drop items older than `cutoff`, then keep at most `max_items` newest.
    async fn prune_for_user(&self, user_id: &str, cutoff: TimestampMs, max_items: i64) -> Result<(), DbError>;
}
