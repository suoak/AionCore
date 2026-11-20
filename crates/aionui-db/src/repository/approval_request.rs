//! 3.0 C9: remote approval requests, bound IM approvers and the request
//! lifetime setting (migration 084).

use aionui_common::TimestampMs;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

use crate::error::DbError;

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct ApprovalRequestRow {
    pub id: String,
    pub user_id: String,
    pub task_session_id: String,
    pub kind: String,
    pub approval_id: Option<String>,
    pub artifact_id: Option<String>,
    pub artifact_hash: Option<String>,
    pub fanout_group_id: Option<String>,
    pub slot: Option<String>,
    pub diff_digest: Option<String>,
    pub target_branch: Option<String>,
    pub strategy: Option<String>,
    pub digest: String,
    pub status: String,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub decided_at: Option<TimestampMs>,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub result_ref: Option<String>,
    pub error: Option<String>,
}

/// A request to insert. Any pending request of the same task and kind is
/// invalidated (`superseded`) in the same transaction.
#[derive(Debug, Clone, Default)]
pub struct NewApprovalRequest<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub task_session_id: &'a str,
    pub kind: &'a str,
    pub approval_id: Option<&'a str>,
    pub artifact_id: Option<&'a str>,
    pub artifact_hash: Option<&'a str>,
    pub fanout_group_id: Option<&'a str>,
    pub slot: Option<&'a str>,
    pub diff_digest: Option<&'a str>,
    pub target_branch: Option<&'a str>,
    pub strategy: Option<&'a str>,
    pub digest: &'a str,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
}

#[derive(Debug, Clone, Serialize, Deserialize, FromRow, PartialEq, Eq)]
pub struct RemoteApproverRow {
    pub user_id: String,
    pub platform_type: String,
    pub platform_user_id: String,
    pub created_at: TimestampMs,
}

#[async_trait]
pub trait IApprovalRequestRepository: Send + Sync {
    /// Insert a pending request; returns the rows it superseded.
    async fn create(&self, new: &NewApprovalRequest<'_>) -> Result<Vec<ApprovalRequestRow>, DbError>;
    async fn get(&self, user_id: &str, id: &str) -> Result<Option<ApprovalRequestRow>, DbError>;
    async fn list_for_task(&self, user_id: &str, task_session_id: &str) -> Result<Vec<ApprovalRequestRow>, DbError>;
    async fn list_pending(&self, user_id: &str) -> Result<Vec<ApprovalRequestRow>, DbError>;
    async fn find_for_approval(&self, user_id: &str, approval_id: &str) -> Result<Option<ApprovalRequestRow>, DbError>;
    /// The one-shot step: `pending → approved|rejected` only while pending and
    /// unexpired. `false` = already decided, expired or invalidated.
    async fn claim(
        &self,
        user_id: &str,
        id: &str,
        status: &str,
        decided_by: &str,
        at: TimestampMs,
    ) -> Result<bool, DbError>;
    /// Record what an approved request produced (or why executing failed).
    async fn record_result(&self, id: &str, result_ref: Option<&str>, error: Option<&str>) -> Result<(), DbError>;
    /// `pending → invalidated` with `reason`; `kind = None` = every kind.
    /// Returns the rows it changed.
    async fn invalidate_pending(
        &self,
        user_id: &str,
        task_session_id: &str,
        kind: Option<&str>,
        reason: &str,
        at: TimestampMs,
    ) -> Result<Vec<ApprovalRequestRow>, DbError>;
    /// `pending → invalidated` for one request (e.g. its diff changed).
    async fn invalidate(&self, user_id: &str, id: &str, reason: &str, at: TimestampMs) -> Result<bool, DbError>;
    /// `pending → expired` for every request past `expires_at`.
    async fn expire_due(&self, now: TimestampMs) -> Result<Vec<ApprovalRequestRow>, DbError>;

    async fn add_approver(
        &self,
        user_id: &str,
        platform_type: &str,
        platform_user_id: &str,
        at: TimestampMs,
    ) -> Result<(), DbError>;
    async fn remove_approver(
        &self,
        user_id: &str,
        platform_type: &str,
        platform_user_id: &str,
    ) -> Result<bool, DbError>;
    async fn list_approvers(&self, user_id: &str) -> Result<Vec<RemoteApproverRow>, DbError>;
    async fn is_approver(&self, user_id: &str, platform_type: &str, platform_user_id: &str) -> Result<bool, DbError>;

    async fn ttl_minutes(&self, user_id: &str) -> Result<Option<i64>, DbError>;
    async fn set_ttl_minutes(&self, user_id: &str, minutes: i64, at: TimestampMs) -> Result<(), DbError>;
}
