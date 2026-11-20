//! 3.0 C4: plan-first tasks. `start` binds the conversation and spawns
//! [`planning_job`] (strict planning → plan approval, needs you); a desktop
//! approval of that plan spawns [`execute_job`].
//!
//! The jobs are boxed `Send` futures in their own module: planning settles a
//! turn, which drains the queue, which starts tasks again — an async cycle the
//! compiler can only check for `Send` across a named boundary.

use std::future::Future;
use std::pin::Pin;

use aionui_api_types::TaskBoardChange;
use aionui_common::now_ms;

use super::item_response;
use crate::service::ConversationService;

pub(crate) type Job = Pin<Box<dyn Future<Output = ()> + Send>>;

pub(crate) fn planning_job(service: ConversationService, user_id: String, id: String, prompt: String) -> Job {
    Box::pin(async move { service.run_plan_first(&user_id, &id, prompt).await })
}

pub(crate) fn execute_job(
    service: ConversationService,
    user_id: String,
    approval: aionui_db::models::TaskApprovalRow,
) -> Job {
    Box::pin(async move {
        let request = aionui_api_types::ExecuteApprovedPlanRequest {
            approval_id: approval.id.clone(),
            artifact_id: approval.artifact_id.clone(),
            artifact_hash: approval.artifact_hash.clone(),
        };
        let id = approval.task_session_id;
        match service.execute_approved_plan(&user_id, &id, request).await {
            // Done → review (needs you), like an agent task's settled turn.
            Ok(run) if run.status == aionui_api_types::TaskRunStatus::Completed => {
                if let Err(error) = service.move_started_task_to_review(&user_id, &id).await {
                    tracing::warn!(%error, task_session_id = %id, "plan-first: in_review move failed");
                }
            }
            Ok(_) => {}
            Err(error) => {
                tracing::warn!(%error, task_session_id = %id, "plan-first: approved plan did not run");
            }
        }
    })
}

impl ConversationService {
    /// 3.0 C4: strict planning in the task's conversation → plan approval
    /// (needs you). When planning cannot start (the agent cannot guarantee
    /// non-mutating planning, …) the task is paused — never executed unplanned.
    pub(crate) async fn run_plan_first(&self, user_id: &str, id: &str, prompt: String) {
        let request = aionui_api_types::StartAutomaticPlanningRequest { prompt };
        match self.start_automatic_planning(user_id, id, request).await {
            Ok(_) => tracing::info!(task_session_id = %id, "plan-first: plan awaiting approval"),
            Err(error) => {
                tracing::warn!(%error, task_session_id = %id, "plan-first: planning did not produce a plan");
                if let Ok(board) = self.task_board_repo() {
                    let _ = board.pause_ready_task(user_id, id, now_ms()).await;
                }
                let _ = self
                    .append_event(
                        user_id,
                        id,
                        "plan_first_failed",
                        serde_json::json!({ "error": error.to_string() }),
                        now_ms(),
                    )
                    .await;
                if let Ok(row) = self.board_item(user_id, id).await
                    && let Ok(item) = item_response(row)
                {
                    self.publish_board_updated(user_id, TaskBoardChange::Board, item.board_status, &item);
                }
            }
        }
    }

    /// 3.0 C4: after the user approves a plan-first task's plan on the
    /// desktop, run the approved plan. Other approvals are left alone.
    pub(crate) async fn continue_plan_first_task(&self, user_id: &str, approval: &aionui_db::models::TaskApprovalRow) {
        if approval.approval_type != "plan" {
            return;
        }
        let Ok(board) = self.task_board_repo() else {
            return;
        };
        match board.get_task_flow(user_id, &approval.task_session_id).await {
            Ok(Some(flow)) if flow.plan_first != 0 => {}
            _ => return,
        }
        tokio::spawn(execute_job(self.clone(), user_id.to_owned(), approval.clone()));
    }
}
