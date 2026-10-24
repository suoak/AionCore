//! Squad template repository (M8 E7-1).

use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::SquadTemplateRow;

/// `DbError::Conflict` prefix for an optimistic-concurrency mismatch.
pub const SQUAD_STALE_CONFLICT_PREFIX: &str = "squad_stale";
/// `DbError::Conflict` prefix for a duplicate live template name.
pub const SQUAD_NAME_CONFLICT_PREFIX: &str = "squad_name_conflict";

#[derive(Debug, Clone)]
pub struct CreateSquadTemplateParams<'a> {
    pub id: &'a str,
    pub user_id: &'a str,
    pub name: &'a str,
    pub description: &'a str,
    pub leader_assistant_id: &'a str,
    pub leader_mode: &'a str,
    /// JSON array text.
    pub members: &'a str,
    pub visibility: &'a str,
    pub now: TimestampMs,
}

/// Full replacement of the editable fields (the service merges a PATCH onto
/// the current row first) guarded by `expected_updated_at`.
#[derive(Debug, Clone)]
pub struct UpdateSquadTemplateParams<'a> {
    pub user_id: &'a str,
    pub id: &'a str,
    pub expected_updated_at: TimestampMs,
    pub name: &'a str,
    pub description: &'a str,
    pub leader_assistant_id: &'a str,
    pub leader_mode: &'a str,
    pub members: &'a str,
    pub visibility: &'a str,
    pub now: TimestampMs,
}

#[async_trait::async_trait]
pub trait ISquadTemplateRepository: Send + Sync {
    async fn create(&self, p: CreateSquadTemplateParams<'_>) -> Result<SquadTemplateRow, DbError>;
    async fn get(&self, user_id: &str, id: &str, include_deleted: bool) -> Result<Option<SquadTemplateRow>, DbError>;
    async fn list(&self, user_id: &str, include_deleted: bool) -> Result<Vec<SquadTemplateRow>, DbError>;
    /// `NotFound` when missing/soft-deleted; `Conflict("squad_stale...")` when
    /// `expected_updated_at` does not match.
    async fn update(&self, p: UpdateSquadTemplateParams<'_>) -> Result<SquadTemplateRow, DbError>;
    /// Soft delete. Returns the new `deleted_at`, or `None` when the template
    /// is missing or already deleted.
    async fn soft_delete(&self, user_id: &str, id: &str, now: TimestampMs) -> Result<Option<TimestampMs>, DbError>;
    /// Ids of Teams instantiated from this template (teams are hard-deleted
    /// on removal, so every remaining row is a live instance).
    async fn instance_team_ids(&self, template_id: &str) -> Result<Vec<String>, DbError>;

    /// M8 E7-2: stamp `teams.squad_template_id` on a freshly created Team owned
    /// by `user_id`. Returns `DbError::NotFound` when no such team.
    async fn bind_team_instance(&self, user_id: &str, team_id: &str, template_id: &str) -> Result<(), DbError>;

    /// M8 E7-6: board assignment of a work item (`task_sessions` row owned by
    /// `user_id`), `None` when no such item.
    async fn work_item_assignment(
        &self,
        user_id: &str,
        work_item_id: &str,
    ) -> Result<Option<WorkItemAssignment>, DbError>;

    /// M8 E7-6: stamp `teams.work_item_id` on a Team owned by `user_id`.
    /// Returns `DbError::NotFound` when no such team.
    async fn bind_team_work_item(&self, user_id: &str, team_id: &str, work_item_id: &str) -> Result<(), DbError>;
}

/// Board fields of a work item that decide whether a Squad may start for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemAssignment {
    pub assignee_kind: Option<String>,
    pub assignee_id: Option<String>,
    pub board_status: String,
}
