//! M8 Q12 / D7-3: one shared worktree per E6 work item.
//!
//! Every Squad instance started for a work item (`task_sessions.id`) runs in
//! the same managed worktree. The link is the existing
//! `task_worktrees.task_session_id` column (no new migration):
//!
//! - first start: create a worktree from the source repository with
//!   `task_session_id = work_item_id` and wait until it is `ready`;
//! - later starts: reuse the newest live (`ready` / `unbound`) non fan-out
//!   row of that work item (a `creating` one is awaited);
//! - closing the work item (`done` / `cancelled`) only marks it (trace event
//!   + `worktree.changed`); the directory and branch are never removed here.

use std::path::Path;

use crate::error::WorktreeError;
use crate::service::WorktreeService;
use crate::store::WorktreeRow;
use crate::types::{CreateWorktreeRequest, STATUS_CREATING, STATUS_READY, STATUS_UNBOUND};

/// A candidate source repository for a work item's first worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemWorktreeSource {
    pub repo_path: String,
    /// `true`: the caller named this repository explicitly, so "not a git
    /// repository" is an error. `false`: a best-effort guess (shared
    /// workspace, project folder) that is skipped when it is not a repo.
    pub explicit: bool,
}

/// The work item's shared worktree and whether it already existed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkItemWorktree {
    pub row: WorktreeRow,
    pub reused: bool,
}

fn usable(row: &WorktreeRow) -> bool {
    [STATUS_READY, STATUS_UNBOUND].contains(&row.status.as_str()) && Path::new(&row.path).is_dir()
}

impl WorktreeService {
    /// The newest usable shared worktree of `work_item_id`, reconciled.
    /// A `creating` row is awaited first. Never creates anything.
    pub async fn work_item_worktree(
        &self,
        user_id: &str,
        work_item_id: &str,
    ) -> Result<Option<WorktreeRow>, WorktreeError> {
        let rows = self.store().list_live_for_work_item(user_id, work_item_id).await?;
        for row in rows {
            let row = if row.status == STATUS_CREATING {
                self.wait_idle(&row.id).await;
                match self.store().get(user_id, &row.id).await {
                    Ok(row) => row,
                    Err(WorktreeError::NotFound(_)) => continue,
                    Err(error) => return Err(error),
                }
            } else {
                row
            };
            let row = self.refresh_orphan(row).await?;
            if usable(&row) {
                return Ok(Some(row));
            }
        }
        Ok(None)
    }

    /// Find or create the shared worktree of `work_item_id`.
    ///
    /// `Ok(None)`: nothing to reuse and no candidate source is a git
    /// repository (callers fall back to a plain shared workspace). A new
    /// worktree is awaited until `ready`; a failed create is an error and its
    /// row is kept (`failed`) for the worktree manager.
    pub async fn ensure_for_work_item(
        &self,
        user_id: &str,
        work_item_id: &str,
        title: Option<&str>,
        sources: &[WorkItemWorktreeSource],
    ) -> Result<Option<WorkItemWorktree>, WorktreeError> {
        let work_item_id = work_item_id.trim();
        if work_item_id.is_empty() {
            return Err(WorktreeError::BadRequest("work_item_id is required".into()));
        }
        let lock = self.work_item_lock(user_id, work_item_id);
        let _guard = lock.lock().await;

        if let Some(row) = self.work_item_worktree(user_id, work_item_id).await? {
            tracing::info!(
                event = "worktree.work_item_reused",
                worktree_id = %row.id,
                user_id,
                work_item_id,
                path = %row.path,
                "reusing the work item's shared worktree"
            );
            return Ok(Some(WorkItemWorktree { row, reused: true }));
        }

        let mut repo_path = None;
        for source in sources {
            let candidate = source.repo_path.trim();
            if candidate.is_empty() {
                continue;
            }
            match self.resolve_repo(candidate).await {
                Ok(_) => {
                    repo_path = Some(candidate.to_owned());
                    break;
                }
                Err(WorktreeError::NotARepository(_)) if !source.explicit => continue,
                Err(error) => return Err(error),
            }
        }
        let Some(repo_path) = repo_path else {
            return Ok(None);
        };

        let created = self
            .create(
                user_id,
                CreateWorktreeRequest {
                    repo_path,
                    base_ref: None,
                    branch: None,
                    title: title.map(str::to_owned),
                    task_session_id: Some(work_item_id.to_owned()),
                },
            )
            .await?;
        self.wait_idle(&created.id).await;
        let row = self.store().get(user_id, &created.id).await?;
        if row.status != STATUS_READY {
            let message = row
                .error_message
                .clone()
                .unwrap_or_else(|| format!("worktree ended in status {}", row.status));
            return Err(WorktreeError::Git(message));
        }
        tracing::info!(
            event = "worktree.work_item_created",
            worktree_id = %row.id,
            user_id,
            work_item_id,
            path = %row.path,
            branch = %row.branch,
            "created the work item's shared worktree"
        );
        Ok(Some(WorkItemWorktree { row, reused: false }))
    }

    /// The work item reached `done` / `cancelled`: mark (never remove) its
    /// live shared worktrees. Returns the rows that were marked.
    pub async fn mark_work_item_closed(
        &self,
        user_id: &str,
        work_item_id: &str,
        board_status: &str,
    ) -> Vec<WorktreeRow> {
        let rows = match self.store().list_live_for_work_item(user_id, work_item_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(user_id, work_item_id, error = %error, "work item worktree lookup failed");
                return Vec::new();
            }
        };
        let mut marked = Vec::new();
        for row in rows {
            let row = match self.refresh_orphan(row).await {
                Ok(row) => row,
                Err(error) => {
                    tracing::warn!(work_item_id, error = %error, "work item worktree reconcile failed");
                    continue;
                }
            };
            if !usable(&row) {
                continue;
            }
            tracing::info!(
                event = "worktree.work_item_closed",
                worktree_id = %row.id,
                user_id,
                work_item_id,
                board_status,
                path = %row.path,
                "work item closed; shared worktree kept"
            );
            self.trace(
                &row,
                "worktree.work_item_closed",
                serde_json::json!({ "board_status": board_status, "kept": true }),
            )
            .await;
            self.events().changed(user_id, &row.id, &row.status);
            marked.push(row);
        }
        marked
    }
}
