//! 3.0 C7: delegation grants and delegation runs (migration 083).

use aionui_common::TimestampMs;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::error::DbError;

pub const DELEGATION_STATUSES: &[&str] = &["starting", "running", "completed", "failed"];

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct DelegationRow {
    pub id: String,
    pub user_id: String,
    pub parent_conversation_id: String,
    pub child_conversation_id: Option<String>,
    pub assistant_id: String,
    pub task_text: String,
    pub status: String,
    pub error: Option<String>,
    pub result_text: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone)]
pub struct NewDelegation<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub parent_conversation_id: &'a str,
    pub assistant_id: &'a str,
    pub task_text: &'a str,
    pub created_at: TimestampMs,
}

#[async_trait]
pub trait IDelegationRepository: Send + Sync {
    /// Idempotent per (conversation, assistant).
    async fn grant(
        &self,
        user_id: &str,
        conversation_id: &str,
        assistant_id: &str,
        at: TimestampMs,
    ) -> Result<(), DbError>;
    async fn is_granted(&self, user_id: &str, conversation_id: &str, assistant_id: &str) -> Result<bool, DbError>;
    /// Inserts a `starting` row only while the parent has fewer than `limit`
    /// unfinished delegations. `false` = over the limit.
    async fn create_within_limit(&self, new: &NewDelegation<'_>, limit: i64) -> Result<bool, DbError>;
    async fn mark_running(&self, id: &str, child_conversation_id: &str, at: TimestampMs) -> Result<bool, DbError>;
    /// `starting|running` → `completed|failed`.
    async fn finish(
        &self,
        id: &str,
        status: &str,
        error: Option<&str>,
        result_text: Option<&str>,
        at: TimestampMs,
    ) -> Result<bool, DbError>;
    async fn get(&self, user_id: &str, id: &str) -> Result<Option<DelegationRow>, DbError>;
    async fn find_by_child(&self, user_id: &str, child_conversation_id: &str)
    -> Result<Option<DelegationRow>, DbError>;
    async fn list_for_parent(&self, user_id: &str, parent_conversation_id: &str)
    -> Result<Vec<DelegationRow>, DbError>;
    /// Boot: unfinished rows can no longer report back.
    async fn fail_unfinished(&self, at: TimestampMs) -> Result<u64, DbError>;
}
