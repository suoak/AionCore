//! M7 E5-4: user inbox (approvals, blocked runs, completions, failures).
//!
//! Read side and read-state mutations. Producers upsert by `dedupe_key` so a
//! repeated signal refreshes one row instead of stacking duplicates.

use crate::error::ConversationError;
use crate::service::ConversationService;
use aionui_api_types::{InboxItemDto, InboxListQuery, InboxListResponse, InboxMarkReadResponse};
use aionui_db::{IInboxRepository, InboxFilter, InboxItemRow};

/// Items older than this are pruned lazily on list.
pub(crate) const INBOX_RETENTION_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Per-user cap on stored items.
pub(crate) const INBOX_MAX_ITEMS: i64 = 500;
const DEFAULT_LIST_LIMIT: i64 = 50;
const MAX_LIST_LIMIT: i64 = 200;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn to_dto(row: InboxItemRow) -> InboxItemDto {
    InboxItemDto {
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

impl ConversationService {
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
}
