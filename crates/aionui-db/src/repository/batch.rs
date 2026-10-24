use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::{BatchItemRow, BatchJobRow};

/// Create a draft (or any initial-status) batch job. `id` is minted by the repo.
#[derive(Debug, Clone)]
pub struct CreateBatchJobParams<'a> {
    pub user_id: &'a str,
    pub title: &'a str,
    pub project_id: Option<&'a str>,
    pub template: &'a str,
    pub agent_type: Option<&'a str>,
    pub assistant_id: Option<&'a str>,
    pub mode: &'a str,
    pub concurrency: i64,
    pub item_timeout_sec: i64,
    pub checks: &'a str,
    pub status: &'a str,
    pub trial_size: i64,
    pub created_at: TimestampMs,
}

/// One item to insert under a job. `seq` must be unique per job.
#[derive(Debug, Clone)]
pub struct CreateBatchItemParams<'a> {
    pub seq: i64,
    pub target: &'a str,
    pub status: &'a str,
}

#[derive(Debug, Clone, Default)]
pub struct BatchJobListFilter {
    /// Exclusive `updated_at` cursor for paging backwards.
    pub before_updated_at: Option<TimestampMs>,
    pub limit: i64,
}

#[async_trait::async_trait]
pub trait IBatchRepository: Send + Sync {
    async fn create_job(&self, params: &CreateBatchJobParams<'_>) -> Result<BatchJobRow, DbError>;

    async fn get_job(&self, user_id: &str, id: &str) -> Result<Option<BatchJobRow>, DbError>;

    /// Newest `updated_at` first; owner-scoped.
    async fn list_jobs(&self, user_id: &str, filter: &BatchJobListFilter) -> Result<Vec<BatchJobRow>, DbError>;

    /// Updates status + `updated_at`. Returns false when missing / wrong owner.
    async fn set_job_status(
        &self,
        user_id: &str,
        id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError>;

    /// Inserts items for an owned job. Fails the whole batch on any conflict.
    async fn insert_items(
        &self,
        user_id: &str,
        batch_job_id: &str,
        items: &[CreateBatchItemParams<'_>],
    ) -> Result<Vec<BatchItemRow>, DbError>;

    async fn get_item(&self, user_id: &str, item_id: &str) -> Result<Option<BatchItemRow>, DbError>;

    /// Ordered by `seq` ascending.
    async fn list_items(&self, user_id: &str, batch_job_id: &str) -> Result<Vec<BatchItemRow>, DbError>;

    async fn set_item_status(
        &self,
        user_id: &str,
        item_id: &str,
        status: &str,
        error: Option<&str>,
        started_at: Option<TimestampMs>,
        finished_at: Option<TimestampMs>,
    ) -> Result<bool, DbError>;
}
