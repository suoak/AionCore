//! 3.0 unified task flow (plan B C1): the optional fields a task carries so it
//! can start on its own (assistant, composer prompt, repository, base ref,
//! plan-first, source). C1 validates and stores them; `start` (C2) uses them.

use std::path::Path;

use aionui_api_types::{CreateTaskFlowFields, TaskSource};
use aionui_db::SetTaskFlowParams;

use crate::error::ConversationError;
use crate::service::ConversationService;

pub const TASK_ASSISTANT_ID_MAX_CHARS: usize = 128;
pub const TASK_PROMPT_MAX_BYTES: usize = 64 * 1024;
pub const TASK_REPO_PATH_MAX_CHARS: usize = 4096;
pub const TASK_BASE_REF_MAX_CHARS: usize = 255;
pub const TASK_SOURCE_REF_MAX_CHARS: usize = 256;

/// Task-flow fields after validation, ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedTaskFlow {
    pub assistant_id: Option<String>,
    pub prompt_json: Option<String>,
    pub repo_path: Option<String>,
    pub base_ref: Option<String>,
    pub plan_first: bool,
    pub source: TaskSource,
    pub source_ref: Option<String>,
}

impl ValidatedTaskFlow {
    /// True when nothing needs storing: readers infer manual / conversation
    /// from `conversation_id` on their own.
    pub fn is_inferable(&self) -> bool {
        self.assistant_id.is_none()
            && self.prompt_json.is_none()
            && self.repo_path.is_none()
            && self.base_ref.is_none()
            && !self.plan_first
            && self.source_ref.is_none()
            && matches!(self.source, TaskSource::Manual | TaskSource::Conversation)
    }
}

fn bad(message: impl Into<String>) -> ConversationError {
    ConversationError::bad_request(message)
}

fn trimmed(value: Option<&String>, field: &str, max_chars: usize) -> Result<Option<String>, ConversationError> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Err(bad(format!("`{field}` must not be empty")));
    }
    if value.chars().count() > max_chars {
        return Err(bad(format!("`{field}` must be at most {max_chars} characters")));
    }
    if value.chars().any(char::is_control) {
        return Err(bad(format!("`{field}` must not contain control characters")));
    }
    Ok(Some(value.to_owned()))
}

/// A conservative subset of `git check-ref-format`: refuses anything git would
/// reject, and anything that could be read as an option.
pub fn is_safe_git_ref(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.starts_with('/')
        && !value.ends_with('/')
        && !value.ends_with('.')
        && !value.ends_with(".lock")
        && !value.contains("..")
        && !value.contains("//")
        && !value.contains("@{")
        && value != "@"
        && !value
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
        && value.split('/').all(|part| !part.starts_with('.'))
}

/// Validate the task-flow fields of a create request.
///
/// `core_source` is set when Core itself creates the task (GitHub import, an
/// agent proposal, cron); clients may only claim `manual` or `conversation`.
pub fn validate_task_flow(
    flow: &CreateTaskFlowFields,
    conversation_bound: bool,
    core_source: Option<TaskSource>,
) -> Result<ValidatedTaskFlow, ConversationError> {
    let source = match (core_source, flow.source) {
        (Some(source), _) => source,
        (None, Some(source @ (TaskSource::Manual | TaskSource::Conversation))) => source,
        (None, Some(other)) => {
            return Err(bad(format!(
                "Task source `{}` is set by Core and cannot be requested",
                other.as_str()
            )));
        }
        (None, None) if conversation_bound => TaskSource::Conversation,
        (None, None) => TaskSource::Manual,
    };
    let assistant_id = trimmed(flow.assistant_id.as_ref(), "assistant_id", TASK_ASSISTANT_ID_MAX_CHARS)?;
    let prompt_json = match &flow.prompt {
        None => None,
        Some(value) if value.is_object() => {
            let text = value.to_string();
            if text.len() > TASK_PROMPT_MAX_BYTES {
                return Err(bad(format!("`prompt` must be at most {TASK_PROMPT_MAX_BYTES} bytes")));
            }
            Some(text)
        }
        Some(_) => return Err(bad("`prompt` must be a JSON object")),
    };
    let repo_path = trimmed(flow.repo_path.as_ref(), "repo_path", TASK_REPO_PATH_MAX_CHARS)?;
    if let Some(path) = &repo_path
        && !Path::new(path).is_absolute()
    {
        return Err(bad("`repo_path` must be an absolute path"));
    }
    let base_ref = trimmed(flow.base_ref.as_ref(), "base_ref", TASK_BASE_REF_MAX_CHARS)?;
    if let Some(reference) = &base_ref
        && !is_safe_git_ref(reference)
    {
        return Err(bad("`base_ref` is not a valid git ref"));
    }
    let source_ref = trimmed(flow.source_ref.as_ref(), "source_ref", TASK_SOURCE_REF_MAX_CHARS)?;
    Ok(ValidatedTaskFlow {
        assistant_id,
        prompt_json,
        repo_path,
        base_ref,
        plan_first: flow.plan_first.unwrap_or(false),
        source,
        source_ref,
    })
}

impl ConversationService {
    /// Store validated task-flow fields on a freshly created task.
    pub(crate) async fn store_task_flow(
        &self,
        user_id: &str,
        task_session_id: &str,
        flow: &ValidatedTaskFlow,
    ) -> Result<(), ConversationError> {
        let repo = match self.task_board_repo() {
            Ok(repo) => repo,
            // Embeddings without a board (some tests, headless tools) lose nothing.
            Err(_) if flow.is_inferable() => return Ok(()),
            Err(error) => return Err(error),
        };
        repo.set_task_flow(
            user_id,
            task_session_id,
            &SetTaskFlowParams {
                assistant_id: flow.assistant_id.as_deref(),
                prompt_json: flow.prompt_json.as_deref(),
                repo_path: flow.repo_path.as_deref(),
                base_ref: flow.base_ref.as_deref(),
                plan_first: flow.plan_first,
                source: Some(flow.source.as_str()),
                source_ref: flow.source_ref.as_deref(),
            },
        )
        .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "task_flow_tests.rs"]
mod tests;
