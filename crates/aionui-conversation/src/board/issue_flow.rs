//! 3.0 C8 (plan B §1.7, §3): Issue tasks.
//!
//! - **Start options.** `POST /api/task-sessions/{id}/start` takes an optional
//!   `{approach, assistant_id}` before the task first runs. `approach` is for
//!   Issue tasks (`source = github`): `fix` (agent run in the task's worktree),
//!   `plan_first` (C4: plan, approval, then implement) or `review_only`
//!   (analyse and report; the task can never be adopted or delivered as a PR —
//!   enforced by Core, not by the prompt alone).
//! - **Issue comment write-back.** Nothing is ever posted on its own.
//!   `GET …/issue-comment/draft` suggests a comment (posts nothing);
//!   `POST …/issue-comment` posts exactly the user-edited `body` and only with
//!   `confirm: true`. Desktop only (device and agent scopes deny it), audited
//!   by the app's audit layer, at most one comment per task; the link is kept
//!   in the task's `delivery.issue_comment`.

use std::sync::Arc;

use aionui_api_types::{
    IssueApproach, IssueCommentDraftResponse, PostIssueCommentRequest, StartTaskRequest, StartTaskResponse,
    TaskBoardItemResponse, TaskBoardStatus, TaskSource,
};
use aionui_common::now_ms;
use aionui_db::{SetTaskFlowParams, UpdateTaskSessionParams};
use serde_json::{Map, Value, json};

use super::item_response;
use crate::error::ConversationError;
use crate::service::ConversationService;
use crate::task_flow::TASK_ASSISTANT_ID_MAX_CHARS;

/// GitHub's comment limit is 65 536 characters.
pub const ISSUE_COMMENT_MAX_CHARS: usize = 65_536;
/// Code prefix of every review-only refusal.
pub const REVIEW_ONLY_CODE: &str = "review_only_task";

static ISSUE_COMMENT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Prepended to a review-only task's prompt.
pub const REVIEW_ONLY_INSTRUCTION: &str = "Review only: analyse this issue and report your findings \
(likely cause, affected code, a suggested fix and its risks). Do not modify, commit or push any files; \
this task will not be merged.";

/// Posts a comment under a GitHub Issue with the user's own `gh` login.
#[async_trait::async_trait]
pub trait IssueCommentPort: Send + Sync {
    /// Returns the comment URL when the CLI reports one.
    async fn post_issue_comment(
        &self,
        user_id: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<Option<String>, ConversationError>;
}

/// `github:owner/name#N` → (`owner/name`, N).
pub fn parse_github_ref(external_ref: &str) -> Option<(String, u64)> {
    let rest = external_ref.strip_prefix("github:")?;
    let (repo, number) = rest.rsplit_once('#')?;
    let (owner, name) = repo.split_once('/')?;
    let valid = |part: &str| {
        !part.is_empty()
            && !part.starts_with('-')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    if !valid(owner) || !valid(name) || name.contains('/') {
        return None;
    }
    let number = number.parse::<u64>().ok().filter(|n| *n > 0)?;
    Some((repo.to_owned(), number))
}

fn approach_of(prompt_json: Option<&str>) -> Option<IssueApproach> {
    prompt_json
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
        .and_then(|value| {
            value
                .get("approach")
                .and_then(Value::as_str)
                .and_then(IssueApproach::parse)
        })
}

fn review_only_error() -> ConversationError {
    ConversationError::Busy {
        reason: format!("{REVIEW_ONLY_CODE}: a review-only task cannot be adopted or delivered"),
    }
}

impl ConversationService {
    pub fn with_issue_comment_port(&self, port: Arc<dyn IssueCommentPort>) {
        if let Ok(mut guard) = self.issue_comment.write() {
            *guard = Some(port);
        }
    }

    /// The approach chosen when the task started, if any.
    pub async fn task_approach(&self, user_id: &str, id: &str) -> Result<Option<IssueApproach>, ConversationError> {
        let flow = self
            .task_board_repo()?
            .get_task_flow(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        Ok(approach_of(flow.prompt_json.as_deref()))
    }

    /// Refuses adopts and pull requests of review-only tasks.
    pub async fn ensure_task_deliverable(&self, user_id: &str, id: &str) -> Result<(), ConversationError> {
        match self.task_approach(user_id, id).await? {
            Some(IssueApproach::ReviewOnly) => Err(review_only_error()),
            _ => Ok(()),
        }
    }

    /// `POST /api/task-sessions/{id}/start` with an optional body.
    pub async fn start_task_with(
        &self,
        user_id: &str,
        id: &str,
        request: StartTaskRequest,
    ) -> Result<StartTaskResponse, ConversationError> {
        if request != StartTaskRequest::default() {
            self.apply_start_options(user_id, id, &request).await?;
        }
        self.start_task(user_id, id).await
    }

    async fn apply_start_options(
        &self,
        user_id: &str,
        id: &str,
        request: &StartTaskRequest,
    ) -> Result<(), ConversationError> {
        let _guard = self.task_start_lock.lock().await;
        let task = self
            .task_session_repo()?
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        let row = self.board_item(user_id, id).await?;
        let flow = self
            .task_board_repo()?
            .get_task_flow(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Task session '{id}' not found")))?;
        let fresh = matches!(task.status.as_str(), "draft" | "ready")
            && matches!(row.board_status.as_str(), "backlog" | "todo")
            && task.conversation_id.is_none()
            && flow.fanout_group_id.is_none()
            && flow.queued_at.is_none();
        if !fresh {
            return Err(ConversationError::bad_request(
                "Start options can only be set before the task first runs",
            ));
        }
        let source = super::item_response(row.clone())?.source;
        if request.approach.is_some() && source != TaskSource::Github {
            return Err(ConversationError::bad_request(
                "`approach` is for tasks imported from a GitHub Issue",
            ));
        }
        let assistant_id = match request.assistant_id.as_deref().map(str::trim) {
            None => flow.assistant_id.clone(),
            Some(value)
                if !value.is_empty()
                    && value.chars().count() <= TASK_ASSISTANT_ID_MAX_CHARS
                    && !value.chars().any(char::is_control) =>
            {
                Some(value.to_owned())
            }
            Some(_) => {
                return Err(ConversationError::bad_request(format!(
                    "`assistant_id` must contain 1 to {TASK_ASSISTANT_ID_MAX_CHARS} characters"
                )));
            }
        };
        let mut prompt: Map<String, Value> = flow
            .prompt_json
            .as_deref()
            .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
            .and_then(|value| match value {
                Value::Object(map) => Some(map),
                _ => None,
            })
            .unwrap_or_default();
        let mut plan_first = flow.plan_first != 0;
        let mut mode = None;
        if let Some(approach) = request.approach {
            prompt.insert("approach".into(), json!(approach.as_str()));
            let base = prompt
                .get("text")
                .and_then(Value::as_str)
                .filter(|text| !text.trim().is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| super::start::prompt_text(None, &task.objective, &task.title));
            let base = base
                .strip_prefix(REVIEW_ONLY_INSTRUCTION)
                .map(|rest| rest.trim_start().to_owned())
                .unwrap_or(base);
            match approach {
                IssueApproach::Fix => {
                    plan_first = false;
                    mode = Some("agent");
                    prompt.insert("text".into(), json!(base));
                }
                IssueApproach::PlanFirst => {
                    plan_first = true;
                    mode = Some("plan");
                    prompt.insert("text".into(), json!(base));
                }
                IssueApproach::ReviewOnly => {
                    plan_first = false;
                    mode = Some("agent");
                    prompt.insert("text".into(), json!(format!("{REVIEW_ONLY_INSTRUCTION}\n\n{base}")));
                }
            }
        }
        let prompt_json = (!prompt.is_empty()).then(|| Value::Object(prompt).to_string());
        if let Some(mode) = mode
            && task.mode != mode
        {
            self.task_session_repo()?
                .update(
                    user_id,
                    id,
                    &UpdateTaskSessionParams {
                        mode: Some(mode),
                        ..Default::default()
                    },
                )
                .await?;
        }
        self.task_board_repo()?
            .set_task_flow(
                user_id,
                id,
                &SetTaskFlowParams {
                    assistant_id: assistant_id.as_deref(),
                    prompt_json: prompt_json.as_deref(),
                    repo_path: flow.repo_path.as_deref(),
                    base_ref: flow.base_ref.as_deref(),
                    plan_first,
                    source: Some(source.as_str()),
                    source_ref: flow.source_ref.as_deref(),
                },
            )
            .await?;
        tracing::info!(
            task_session_id = %id,
            approach = request.approach.map(IssueApproach::as_str),
            assistant = assistant_id.is_some(),
            "task start options set"
        );
        Ok(())
    }

    async fn issue_of(&self, user_id: &str, id: &str) -> Result<(String, u64), ConversationError> {
        let row = self.board_item(user_id, id).await?;
        row.external_ref
            .as_deref()
            .and_then(parse_github_ref)
            .ok_or_else(|| ConversationError::bad_request("This task did not come from a GitHub Issue"))
    }

    /// `GET /api/task-sessions/{id}/issue-comment/draft`. Posts nothing.
    pub async fn issue_comment_draft(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<IssueCommentDraftResponse, ConversationError> {
        let (repo, number) = self.issue_of(user_id, id).await?;
        let row = self.board_item(user_id, id).await?;
        let delivery = self.delivery_object(user_id, id).await?;
        let approach = self.task_approach(user_id, id).await?;
        let status = match TaskBoardStatus::parse(&row.board_status) {
            Some(TaskBoardStatus::Done) => "done",
            Some(TaskBoardStatus::InReview) => "ready for review",
            Some(TaskBoardStatus::Cancelled) => "cancelled",
            _ => "in progress",
        };
        let mut lines = vec![
            format!(
                "Update from CSBU WorkMate on #{number}: **{}** is {status}.",
                row.title.trim()
            ),
            String::new(),
        ];
        if approach == Some(IssueApproach::ReviewOnly) {
            lines.push("- Approach: review only (no code changes).".into());
        }
        if let Some(url) = delivery
            .get("pull_request")
            .and_then(|pr| pr.get("url"))
            .and_then(Value::as_str)
        {
            lines.push(format!("- Pull request: {url}"));
        }
        if let Some(adopt) = delivery.get("adopt") {
            let commit = adopt.get("result_commit").and_then(Value::as_str).unwrap_or_default();
            let target = adopt.get("target_branch").and_then(Value::as_str).unwrap_or_default();
            if !commit.is_empty() {
                let short: String = commit.chars().take(12).collect();
                lines.push(if target.is_empty() {
                    format!("- Merged as {short}.")
                } else {
                    format!("- Merged into `{target}` as {short}.")
                });
            }
        }
        let posted_url = delivery
            .get("issue_comment")
            .and_then(|comment| comment.get("url"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(IssueCommentDraftResponse {
            repo,
            number,
            body: lines.join("\n").trim_end().to_owned(),
            posted_url,
        })
    }

    /// `POST /api/task-sessions/{id}/issue-comment`: posts exactly the
    /// user-edited body, only with `confirm: true`, once per task.
    pub async fn post_issue_comment(
        &self,
        user_id: &str,
        id: &str,
        request: PostIssueCommentRequest,
    ) -> Result<TaskBoardItemResponse, ConversationError> {
        if !request.confirm {
            return Err(ConversationError::bad_request(
                "issue_comment_unconfirmed: confirm the comment before it is posted",
            ));
        }
        let body = request.body.trim();
        if body.is_empty() || body.chars().count() > ISSUE_COMMENT_MAX_CHARS {
            return Err(ConversationError::bad_request(format!(
                "An Issue comment needs 1 to {ISSUE_COMMENT_MAX_CHARS} characters"
            )));
        }
        let (repo, number) = self.issue_of(user_id, id).await?;
        let port = self
            .issue_comment
            .read()
            .ok()
            .and_then(|guard| guard.clone())
            .ok_or_else(|| ConversationError::internal("Issue comments are not configured"))?;
        // Serializes posts so a double click cannot comment twice; separate
        // from the start lock so a slow `gh` never blocks task starts.
        let _guard = ISSUE_COMMENT_LOCK.lock().await;
        let mut delivery = self.delivery_object(user_id, id).await?;
        if delivery.contains_key("issue_comment") {
            return Err(ConversationError::Busy {
                reason: "issue_comment_already_posted: this task already commented on its Issue".into(),
            });
        }
        let url = port.post_issue_comment(user_id, &repo, number, body).await?;
        delivery.insert(
            "issue_comment".into(),
            json!({ "url": url, "repo": repo, "number": number, "at": now_ms() }),
        );
        self.write_delivery(user_id, id, delivery).await?;
        let row = self.board_item(user_id, id).await?;
        let from = TaskBoardStatus::parse(&row.board_status).unwrap_or(TaskBoardStatus::Backlog);
        self.publish_item(user_id, from, row.clone());
        tracing::info!(task_session_id = %id, repo = %repo, number, "issue comment posted after confirmation");
        item_response(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn github_refs_parse_strictly() {
        assert_eq!(
            parse_github_ref("github:suoak/AionCore#42"),
            Some(("suoak/AionCore".into(), 42))
        );
        for bad in [
            "github:suoak#1",
            "github:/x#1",
            "github:a/b#0",
            "github:a/b#x",
            "github:a/b/c#1",
            "github:-a/b#1",
            "github:a/b c#1",
            "gitlab:a/b#1",
            "a/b#1",
        ] {
            assert_eq!(parse_github_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_approach_is_read_from_the_prompt_state() {
        assert_eq!(
            approach_of(Some(r#"{"approach":"review_only"}"#)),
            Some(IssueApproach::ReviewOnly)
        );
        assert_eq!(approach_of(Some(r#"{"approach":"yolo"}"#)), None);
        assert_eq!(approach_of(Some("not json")), None);
        assert_eq!(approach_of(None), None);
    }
}
