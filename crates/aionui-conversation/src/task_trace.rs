use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::AgentStreamEvent;
use aionui_ai_agent::protocol::events::tool_call::{AcpToolCallContentItem, AcpToolCallStatus, ToolCallStatus};
use aionui_common::now_ms;
use aionui_db::{
    AppendTaskTraceEventParams, CreateTaskCheckpointParams, CreateTaskEvidenceParams, DbError, ITaskSessionRepository,
};
use sha2::{Digest, Sha256};

use crate::stream_persistence::OutputRetentionPolicy;
use crate::trace_redaction::sanitize_json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TaskTracePolicy {
    StrictPlanning,
    ApprovedExecution,
}

#[derive(Clone)]
pub(crate) struct TaskTraceContext {
    pub user_id: String,
    pub task_id: String,
    pub run_id: String,
    conversation_id: String,
    pub policy: TaskTracePolicy,
    repo: Arc<dyn ITaskSessionRepository>,
    output_retention: OutputRetentionPolicy,
    observed_calls: Arc<Mutex<HashMap<String, bool>>>,
}

impl TaskTraceContext {
    pub(crate) fn new(
        user_id: String,
        task_id: String,
        run_id: String,
        conversation_id: String,
        policy: TaskTracePolicy,
        repo: Arc<dyn ITaskSessionRepository>,
        output_retention: OutputRetentionPolicy,
    ) -> Self {
        Self {
            user_id,
            task_id,
            run_id,
            conversation_id,
            policy,
            repo,
            output_retention,
            observed_calls: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    async fn event(&self, event_type: &str, payload: serde_json::Value) -> Result<String, DbError> {
        let payload = sanitize_json(&payload).to_string();
        let event = self
            .repo
            .append_trace_event(&AppendTaskTraceEventParams {
                user_id: &self.user_id,
                task_session_id: &self.task_id,
                run_id: &self.run_id,
                event_type,
                timestamp: now_ms(),
                payload: &payload,
            })
            .await?;
        Ok(event.id)
    }

    async fn evidence(
        &self,
        trace_event_id: Option<&str>,
        kind: &str,
        summary: &str,
        reference: Option<&str>,
        metadata: serde_json::Value,
    ) -> Result<(), DbError> {
        let metadata = sanitize_json(&metadata).to_string();
        self.repo
            .create_evidence(&CreateTaskEvidenceParams {
                user_id: &self.user_id,
                task_session_id: &self.task_id,
                run_id: &self.run_id,
                trace_event_id,
                criterion_id: None,
                kind,
                summary,
                reference,
                metadata: &metadata,
                created_at: now_ms(),
            })
            .await?;
        Ok(())
    }

    fn observed_decision(&self, call_id: &str) -> Option<bool> {
        self.observed_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(call_id)
            .copied()
    }

    async fn record_start(&self, call_id: &str, tool: &str, capability: &str) -> Result<(bool, bool), DbError> {
        if let Some(allowed) = self.observed_decision(call_id) {
            return Ok((allowed, false));
        }
        let requested = serde_json::json!({
            "call_id": call_id,
            "tool": tool,
            "capability": capability
        });
        self.event("tool.requested", requested.clone()).await?;

        let (allowed, rule_id, reason) = match self.policy {
            TaskTracePolicy::ApprovedExecution => (
                true,
                "execution.approved_plan",
                "Tool execution is authorized by the approved immutable plan",
            ),
            TaskTracePolicy::StrictPlanning => {
                let allowed = matches!(tool, "Read" | "Grep" | "Glob" | "ViewImage");
                if allowed {
                    (
                        true,
                        "planning.strict.allow_only",
                        "Read-only tool is present in the strict planning allowlist",
                    )
                } else {
                    (
                        false,
                        "planning.strict.deny",
                        "Tool is absent from the strict planning allowlist",
                    )
                }
            }
        };
        let decision = serde_json::json!({
            "call_id": call_id,
            "tool": tool,
            "capability": capability,
            "decision": if allowed { "allow" } else { "deny" },
            "rule_id": rule_id,
            "reason": reason
        });
        let event_type = if allowed { "tool.allowed" } else { "tool.denied" };
        let event_id = self.event(event_type, decision.clone()).await?;
        self.evidence(Some(&event_id), "policy", reason, Some(rule_id), decision)
            .await?;
        self.observed_calls
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(call_id.to_owned(), allowed);
        Ok((allowed, true))
    }

    pub(crate) async fn observe(&self, event: &AgentStreamEvent) -> Result<(), DbError> {
        match event {
            AgentStreamEvent::ToolCall(data) => {
                let capability = capability_for_tool(&data.name);
                let (allowed, first) = self.record_start(&data.call_id, &data.name, capability).await?;
                match data.status {
                    ToolCallStatus::Running if allowed => {
                        self.event(
                            "tool.started",
                            serde_json::json!({ "call_id": data.call_id, "tool": data.name, "capability": capability }),
                        )
                        .await?;
                    }
                    ToolCallStatus::Completed => {
                        if allowed && first {
                            self.event(
                                "tool.started",
                                serde_json::json!({ "call_id": data.call_id, "tool": data.name, "capability": capability }),
                            )
                            .await?;
                        }
                        self.event(
                            "tool.completed",
                            serde_json::json!({ "call_id": data.call_id, "tool": data.name, "capability": capability }),
                        )
                        .await?;
                    }
                    ToolCallStatus::Error | ToolCallStatus::Canceled => {
                        self.event(
                            "tool.failed",
                            serde_json::json!({
                                "call_id": data.call_id,
                                "tool": data.name,
                                "capability": capability,
                                "result": data.output
                            }),
                        )
                        .await?;
                    }
                    ToolCallStatus::Running => {}
                }
            }
            AgentStreamEvent::AcpToolCall(data) => {
                let update = &data.update;
                let tool = update.title.as_deref().unwrap_or("ACP tool");
                let capability = update
                    .kind
                    .map(|kind| format!("{kind:?}").to_ascii_lowercase())
                    .unwrap_or_else(|| "unknown".into());
                let (allowed, first) = self.record_start(&update.tool_call_id, tool, &capability).await?;
                match update.status {
                    Some(AcpToolCallStatus::Pending | AcpToolCallStatus::InProgress) if allowed => {
                        self.event(
                            "tool.started",
                            serde_json::json!({ "call_id": update.tool_call_id, "tool": tool, "capability": capability }),
                        )
                        .await?;
                    }
                    Some(AcpToolCallStatus::Completed) => {
                        if allowed && first {
                            self.event(
                                "tool.started",
                                serde_json::json!({ "call_id": update.tool_call_id, "tool": tool, "capability": capability }),
                            )
                            .await?;
                        }
                        self.event(
                            "tool.completed",
                            serde_json::json!({ "call_id": update.tool_call_id, "tool": tool, "capability": capability }),
                        )
                        .await?;
                        if allowed && let Some(content) = &update.content {
                            for item in content {
                                if let AcpToolCallContentItem::Diff {
                                    path,
                                    old_text,
                                    new_text,
                                } = item
                                {
                                    self.record_file_change(path, old_text.as_deref(), new_text).await?;
                                }
                            }
                        }
                    }
                    Some(AcpToolCallStatus::Failed) => {
                        self.event(
                            "tool.failed",
                            serde_json::json!({ "call_id": update.tool_call_id, "tool": tool, "capability": capability }),
                        )
                        .await?;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn record_file_change(&self, path: &str, before: Option<&str>, after: &str) -> Result<(), DbError> {
        let change_type = match (before, after.is_empty()) {
            (None, false) => "added",
            (Some(_), true) => "deleted",
            _ => "modified",
        };
        let before_hash = before.map(sha256);
        let after_hash = (!after.is_empty()).then(|| sha256(after));
        let (added_lines, deleted_lines) = line_change_counts(before.unwrap_or_default(), after);
        let diff = crate::trace_redaction::redact_text(&format!(
            "--- a/{path}\n+++ b/{path}\n@@\n-{}\n+{}",
            before.unwrap_or_default(),
            after
        ));
        let payload = serde_json::json!({
            "path": path,
            "change_type": change_type,
            "before_hash": before_hash.clone(),
            "after_hash": after_hash.clone(),
            "added_lines": added_lines,
            "deleted_lines": deleted_lines
        });
        let event_id = self.event("file.changed", payload.clone()).await?;
        self.evidence(
            Some(&event_id),
            "file",
            &format!("{change_type}: {path}"),
            Some(path),
            payload,
        )
        .await?;
        let retained = self
            .output_retention
            .retain_evidence(&self.user_id, &self.conversation_id, &diff)
            .await
            .map_err(|error| DbError::Init(format!("Failed to retain task diff evidence: {error}")))?;
        let diff_reference = retained.reference.clone();
        self.evidence(
            Some(&event_id),
            "diff",
            &format!("Diff for {path}"),
            Some(&retained.reference),
            serde_json::json!({
                "path": path,
                "diff_preview": retained.preview,
                "sha256": retained.sha256,
                "size": retained.size
            }),
        )
        .await?;
        let checkpoint_state = serde_json::json!({
            "path": path,
            "change_type": change_type,
            "before_hash": before_hash,
            "after_hash": after_hash,
            "diff_reference": diff_reference
        })
        .to_string();
        self.repo
            .create_checkpoint(&CreateTaskCheckpointParams {
                user_id: &self.user_id,
                task_session_id: &self.task_id,
                run_id: &self.run_id,
                checkpoint_type: "after_mutation",
                artifact_id: None,
                state: &checkpoint_state,
                created_at: now_ms(),
            })
            .await?;
        Ok(())
    }
}

fn sha256(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn line_change_counts(before: &str, after: &str) -> (usize, usize) {
    let before = before.lines().collect::<Vec<_>>();
    let after = after.lines().collect::<Vec<_>>();
    let prefix = before
        .iter()
        .zip(after.iter())
        .take_while(|(left, right)| left == right)
        .count();
    let suffix = before[prefix..]
        .iter()
        .rev()
        .zip(after[prefix..].iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    (
        after.len().saturating_sub(prefix + suffix),
        before.len().saturating_sub(prefix + suffix),
    )
}

fn capability_for_tool(tool: &str) -> &'static str {
    match tool {
        "Read" | "Grep" | "Glob" | "ViewImage" => "filesystem.read",
        "Write" | "Edit" => "filesystem.write",
        "ExecCommand" => "shell.execute",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_ai_agent::protocol::events::tool_call::{
        AcpToolCallEventData, AcpToolCallKind, AcpToolCallSessionUpdateKind, AcpToolCallUpdateData, ToolCallEventData,
    };
    use aionui_db::{SqliteTaskSessionRepository, init_database_memory};

    #[tokio::test]
    async fn persists_real_policy_and_file_evidence_without_secrets() {
        let db = init_database_memory().await.unwrap();
        sqlx::raw_sql(
            "INSERT INTO conversations (id, user_id, name, type, created_at, updated_at)
             VALUES ('trace-conversation', 'system_default_user', 'Trace', 'aionrs', 1, 1);
             INSERT INTO task_sessions
             (id, user_id, title, conversation_id, mode, objective, acceptance_criteria, status, agent_type, created_at, updated_at)
             VALUES ('trace-task', 'system_default_user', 'Trace', 'trace-conversation', 'plan', '', '[]', 'running', 'aionrs', 1, 1);
             INSERT INTO task_artifacts
             (id, task_session_id, kind, version, content, content_hash, status, created_at, updated_at)
             VALUES ('trace-artifact', 'trace-task', 'plan', 1, 'plan', 'hash', 'approved', 1, 1);
             INSERT INTO task_approvals
             (id, task_session_id, approval_type, artifact_id, artifact_hash, status, requested_at, resolved_at, resolved_by)
             VALUES ('trace-approval', 'trace-task', 'plan', 'trace-artifact', 'hash', 'approved', 1, 2, 'system_default_user');
             INSERT INTO task_runs
             (id, task_session_id, conversation_id, run_kind, plan_artifact_id, approval_id, status, started_at)
             VALUES ('trace-run', 'trace-task', 'trace-conversation', 'execution', 'trace-artifact', 'trace-approval', 'running', 3);",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let repo = Arc::new(SqliteTaskSessionRepository::new(db.pool().clone()));
        let retained = tempfile::tempdir().unwrap();
        let strict = TaskTraceContext::new(
            "system_default_user".into(),
            "trace-task".into(),
            "trace-run".into(),
            "trace-conversation".into(),
            TaskTracePolicy::StrictPlanning,
            repo.clone(),
            OutputRetentionPolicy::new(retained.path().to_path_buf()),
        );
        strict
            .observe(&AgentStreamEvent::ToolCall(ToolCallEventData {
                call_id: "denied-write".into(),
                name: "Write".into(),
                args: serde_json::json!({ "password": "do-not-store" }),
                status: ToolCallStatus::Running,
                input: None,
                output: None,
                description: None,
                parent_call_id: None,
            }))
            .await
            .unwrap();

        let approved = TaskTraceContext::new(
            "system_default_user".into(),
            "trace-task".into(),
            "trace-run".into(),
            "trace-conversation".into(),
            TaskTracePolicy::ApprovedExecution,
            repo.clone(),
            OutputRetentionPolicy::new(retained.path().to_path_buf()),
        );
        approved
            .observe(&AgentStreamEvent::AcpToolCall(AcpToolCallEventData {
                session_id: "session".into(),
                update: AcpToolCallUpdateData {
                    session_update: AcpToolCallSessionUpdateKind::ToolCallUpdate,
                    tool_call_id: "edit-1".into(),
                    status: Some(AcpToolCallStatus::Completed),
                    title: Some("Edit".into()),
                    kind: Some(AcpToolCallKind::Edit),
                    raw_input: Some(serde_json::json!({ "authorization": "Bearer do-not-store" })),
                    raw_output: None,
                    content: Some(vec![AcpToolCallContentItem::Diff {
                        path: "src/auth.ts".into(),
                        old_text: Some("token=do-not-store".into()),
                        new_text: "token=safe-value".into(),
                    }]),
                    locations: None,
                },
                meta: None,
            }))
            .await
            .unwrap();

        let events = repo
            .list_trace_events("system_default_user", "trace-task", "trace-run")
            .await
            .unwrap();
        assert!(events.iter().any(|event| event.event_type == "tool.denied"));
        assert!(events.iter().any(|event| event.event_type == "tool.allowed"));
        assert!(events.iter().any(|event| event.event_type == "file.changed"));
        assert!(!events.iter().any(|event| event.payload.contains("do-not-store")));

        let evidence = repo
            .list_evidence("system_default_user", "trace-task", "trace-run")
            .await
            .unwrap();
        assert!(evidence.iter().any(|item| item.kind == "file"));
        assert!(evidence.iter().any(|item| item.kind == "diff"));
        let file_metadata = &evidence.iter().find(|item| item.kind == "file").unwrap().metadata;
        assert!(file_metadata.contains(r#""added_lines":1"#));
        assert!(file_metadata.contains(r#""deleted_lines":1"#));
        assert!(
            !evidence.iter().any(|item| item.metadata.contains("do-not-store")),
            "persisted evidence leaked a secret: {evidence:?}"
        );
        let diff_reference = evidence
            .iter()
            .find(|item| item.kind == "diff")
            .and_then(|item| item.reference.as_deref())
            .unwrap();
        let (_, full_diff) = OutputRetentionPolicy::new(retained.path().to_path_buf())
            .read("system_default_user", "trace-conversation", diff_reference)
            .await
            .unwrap();
        assert!(full_diff.contains("src/auth.ts"));
        assert!(!full_diff.contains("do-not-store"));
        let checkpoints = repo
            .list_checkpoints("system_default_user", "trace-task", "trace-run")
            .await
            .unwrap();
        assert!(
            checkpoints
                .iter()
                .any(|checkpoint| checkpoint.checkpoint_type == "after_mutation")
        );
    }

    #[test]
    fn line_counts_ignore_unchanged_prefix_and_suffix() {
        assert_eq!(line_change_counts("same\nold\ntail", "same\nnew\nextra\ntail"), (2, 1));
    }
}
