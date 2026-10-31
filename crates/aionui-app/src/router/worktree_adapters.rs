//! M7 E3-2 glue between managed worktrees and the conversation / SCM crates,
//! which must not depend on `aionui-worktree` directly.

use std::path::Path;
use std::sync::Arc;

use aionui_common::OnConversationDelete;
use aionui_conversation::{IWorktreeBinding, WorktreeBindingError, WorktreeBindingTarget};
use aionui_project::scm::IManagedWorktreeLookup;
use aionui_worktree::{WorktreeError, WorktreeService};
use async_trait::async_trait;

/// Statuses a conversation may bind from.
const BINDABLE: [&str; 2] = ["ready", "unbound"];

pub(crate) struct WorktreeConversationBinding {
    pub(crate) service: WorktreeService,
}

fn internal(error: WorktreeError) -> WorktreeBindingError {
    match error {
        WorktreeError::NotFound(_) => WorktreeBindingError::NotFound,
        other => WorktreeBindingError::Internal(other.to_string()),
    }
}

#[async_trait]
impl IWorktreeBinding for WorktreeConversationBinding {
    async fn resolve(&self, user_id: &str, worktree_id: &str) -> Result<WorktreeBindingTarget, WorktreeBindingError> {
        let row = self
            .service
            .binding_candidate(user_id, worktree_id)
            .await
            .map_err(internal)?;
        if !BINDABLE.contains(&row.status.as_str()) {
            return Err(WorktreeBindingError::NotReady { status: row.status });
        }
        if row.conversation_id.is_some() {
            return Err(WorktreeBindingError::AlreadyBound);
        }
        if !Path::new(&row.path).is_dir() {
            return Err(WorktreeBindingError::NotReady {
                status: "orphaned".into(),
            });
        }
        Ok(WorktreeBindingTarget {
            worktree_id: row.id,
            path: row.path,
            fanout_group_id: row.fanout_group_id,
            fanout_slot: row.slot,
        })
    }

    async fn bind(&self, user_id: &str, worktree_id: &str, conversation_id: &str) -> Result<(), WorktreeBindingError> {
        if self
            .service
            .bind_conversation(user_id, worktree_id, conversation_id)
            .await
            .map_err(internal)?
        {
            Ok(())
        } else {
            Err(WorktreeBindingError::AlreadyBound)
        }
    }

    async fn on_work_item_closed(
        &self,
        user_id: &str,
        work_item_id: &str,
        board_status: &str,
    ) -> Vec<aionui_conversation::KeptWorkItemWorktree> {
        self.service
            .mark_work_item_closed(user_id, work_item_id, board_status)
            .await
            .into_iter()
            .map(|row| aionui_conversation::KeptWorkItemWorktree {
                worktree_id: row.id,
                path: row.path,
                branch: row.branch,
            })
            .collect()
    }
}

/// Conversation delete → bound worktrees become `unbound`; directories stay.
pub(crate) struct WorktreeUnbindOnDelete {
    pub(crate) service: WorktreeService,
}

#[async_trait]
impl OnConversationDelete for WorktreeUnbindOnDelete {
    async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        self.service.on_conversation_deleted(user_id, conversation_id).await;
    }
}

/// SCM discover → `ScmRepository.managed_worktree_id`.
pub(crate) struct ScmManagedWorktreeLookup {
    pub(crate) service: WorktreeService,
}

#[async_trait]
impl IManagedWorktreeLookup for ScmManagedWorktreeLookup {
    async fn managed_worktree_id(&self, workdir: &Path) -> Option<String> {
        self.service.managed_id_for_path(workdir).await
    }
}

/// Wire the worktree service into the shared conversation service and the
/// TaskSession trace. Called once from `build_module_states`.
pub(crate) fn wire_worktree_bindings(services: &crate::services::AppServices, service: &WorktreeService) {
    service.set_task_sessions(Arc::new(aionui_db::SqliteTaskSessionRepository::new(
        services.database.pool().clone(),
    )));
    services
        .conversation_service
        .with_worktree_binding(Arc::new(WorktreeConversationBinding {
            service: service.clone(),
        }));
    services
        .conversation_service
        .with_delete_hook(Arc::new(WorktreeUnbindOnDelete {
            service: service.clone(),
        }));
}

/// E3-4 fan-out → conversation service. Slot conversations are ordinary
/// assistant conversations bound to the slot worktree: the assistant's own
/// backend, permission mode, MCP and skills apply (no override, never YOLO).
pub(crate) struct FanoutConversationsAdapter {
    pub(crate) conversations: aionui_conversation::ConversationService,
    pub(crate) task_manager: Arc<dyn aionui_ai_agent::IWorkerTaskManager>,
    pub(crate) assistants: Arc<aionui_assistant::AssistantService>,
}

fn reply_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(s) => Some(s.clone()),
        other => other.get("content").and_then(|v| v.as_str()).map(str::to_owned),
    }
}

#[async_trait]
impl aionui_worktree::IFanoutConversations for FanoutConversationsAdapter {
    async fn check_assistant(&self, user_id: &str, assistant_id: &str) -> Result<(), String> {
        let assistant = self
            .assistants
            .get_for_user(user_id, assistant_id)
            .await
            .map_err(|e| e.to_string())?;
        if !assistant.enabled {
            return Err("assistant is disabled".into());
        }
        Ok(())
    }

    async fn create_slot_conversation(
        &self,
        user_id: &str,
        spec: aionui_worktree::SlotConversationSpec,
    ) -> Result<String, String> {
        let model = match spec.model {
            Some(value) if !value.is_null() => Some(serde_json::from_value(value).map_err(|e| format!("model: {e}"))?),
            _ => None,
        };
        let req = aionui_api_types::CreateConversationRequest {
            r#type: None,
            name: Some(spec.title),
            model,
            assistant: Some(aionui_api_types::AssistantConversationRequest {
                id: spec.assistant_id,
                locale: None,
                conversation_overrides: None,
            }),
            source: None,
            channel_chat_id: None,
            extra: serde_json::json!({}),
            worktree_id: Some(spec.worktree_id),
        };
        self.conversations
            .create(user_id, req)
            .await
            .map(|c| c.id)
            .map_err(|e| e.to_string())
    }

    async fn send_prompt(
        &self,
        user_id: &str,
        conversation_id: &str,
        prompt: &aionui_worktree::FanoutPrompt,
    ) -> Result<(), String> {
        let files = prompt
            .files
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("files: {e}"))?;
        let sessions = prompt
            .sessions
            .iter()
            .map(|v| serde_json::from_value(v.clone()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| format!("sessions: {e}"))?;
        let req = aionui_api_types::SendMessageRequest {
            content: prompt.content.clone(),
            files,
            sessions,
            inject_skills: Vec::new(),
            hidden: false,
        };
        self.conversations
            .send_message(user_id, conversation_id, req, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn runtime(&self, _user_id: &str, conversation_id: &str) -> aionui_worktree::SlotRuntime {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        aionui_worktree::SlotRuntime {
            busy: summary.is_processing || summary.state != aionui_api_types::ConversationRuntimeStateKind::Idle,
        }
    }

    async fn cancel_turn(&self, user_id: &str, conversation_id: &str) -> Result<(), String> {
        let summary = self.conversations.runtime_summary_for(conversation_id).await;
        let Some(turn_id) = summary.turn_id else {
            return Ok(());
        };
        self.conversations
            .cancel(user_id, conversation_id, &turn_id, &self.task_manager)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    async fn last_reply(&self, user_id: &str, conversation_id: &str) -> Option<String> {
        let message = self
            .conversations
            .latest_message_of_type(user_id, conversation_id, "text")
            .await
            .ok()
            .flatten()?;
        if message.position == Some(aionui_common::MessagePosition::Right) {
            return None;
        }
        reply_text(&message.content)
    }
}

/// Build the fan-out state on the worktree service and wire its conversation
/// port. Called once from `build_module_states`.
pub(crate) fn build_fanout_state(
    services: &crate::services::AppServices,
    worktree: &WorktreeService,
    assistants: Arc<aionui_assistant::AssistantService>,
) -> aionui_worktree::FanoutRouterState {
    let service = aionui_worktree::FanoutService::new(
        worktree.clone(),
        aionui_worktree::FanoutStore::new(services.database.pool().clone()),
    );
    service.set_conversations(Arc::new(FanoutConversationsAdapter {
        conversations: services.conversation_service.clone(),
        task_manager: services.worker_task_manager.clone(),
        assistants,
    }));
    aionui_worktree::FanoutRouterState { service }
}

/// M8 Q12 / D7-3: one shared managed worktree per work item for Squad
/// instances. Source repository, used only for the first instance:
/// explicit `repo_path` (must be git) → instantiate `workspace` (if git) →
/// the work item's project workspace folder (if git); otherwise `None` and
/// the Team keeps its plain shared workspace.
pub(crate) struct SquadWorkItemWorktreeAdapter {
    pub(crate) service: WorktreeService,
    pub(crate) task_sessions: Arc<dyn aionui_db::ITaskSessionRepository>,
    pub(crate) projects: aionui_project::ProjectService,
}

impl SquadWorkItemWorktreeAdapter {
    async fn project_workspace(&self, user_id: &str, project_id: &str) -> Option<String> {
        let detail = self.projects.get_project(user_id, project_id).await.ok()?;
        let entry = detail
            .explorer
            .entries
            .iter()
            .find(|e| e.pe_id == detail.explorer.workspace_pe_id)
            .or_else(|| detail.explorer.entries.iter().find(|e| e.role == "workspace"))?;
        let path = aionui_project::canonical::uri_to_path(&entry.folder.resource_uri).ok()?;
        Some(path.to_string_lossy().into_owned())
    }
}

fn squad_worktree_error(error: WorktreeError) -> aionui_team::SquadWorktreeError {
    match error {
        WorktreeError::NotARepository(_) | WorktreeError::BadRequest(_) | WorktreeError::InvalidRef(_) => {
            aionui_team::SquadWorktreeError::InvalidSource(error.to_string())
        }
        other => aionui_team::SquadWorktreeError::Failed(other.to_string()),
    }
}

#[async_trait]
impl aionui_team::SquadWorkItemWorktreePort for SquadWorkItemWorktreeAdapter {
    async fn ensure(
        &self,
        user_id: &str,
        work_item_id: &str,
        repo_path: Option<&str>,
        workspace: Option<&str>,
    ) -> Result<Option<aionui_api_types::SquadWorkItemWorktree>, aionui_team::SquadWorktreeError> {
        let item = self
            .task_sessions
            .get(user_id, work_item_id)
            .await
            .map_err(|e| aionui_team::SquadWorktreeError::Failed(e.to_string()))?;
        let mut sources = Vec::new();
        if let Some(repo) = repo_path {
            sources.push(aionui_worktree::WorkItemWorktreeSource {
                repo_path: repo.to_owned(),
                explicit: true,
            });
        } else if let Some(workspace) = workspace {
            // A caller-chosen non-git workspace keeps the plain shared
            // workspace rather than silently switching to the project.
            sources.push(aionui_worktree::WorkItemWorktreeSource {
                repo_path: workspace.to_owned(),
                explicit: false,
            });
        } else if let Some(project_id) = item.as_ref().and_then(|i| i.project_id.as_deref())
            && let Some(path) = self.project_workspace(user_id, project_id).await
        {
            sources.push(aionui_worktree::WorkItemWorktreeSource {
                repo_path: path,
                explicit: false,
            });
        }
        let title = item.as_ref().map(|i| i.title.as_str());
        let found = self
            .service
            .ensure_for_work_item(user_id, work_item_id, title, &sources)
            .await
            .map_err(squad_worktree_error)?;
        Ok(found.map(|wt| aionui_api_types::SquadWorkItemWorktree {
            id: wt.row.id,
            path: wt.row.path,
            branch: wt.row.branch,
            source_repo_path: wt.row.source_repo_path,
            reused: wt.reused,
        }))
    }
}

#[cfg(test)]
mod q12_tests {
    use super::*;
    use aionui_db::{CreateTaskSessionParams, ITaskSessionRepository, SqliteTaskSessionRepository};
    use aionui_team::{SquadWorkItemWorktreePort, SquadWorktreeError};

    const USER: &str = "system_default_user";

    fn git(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn init_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        assert!(git(dir, &["init", "-q", "-b", "main"]));
        std::fs::write(dir.join("README.md"), "hi\n").unwrap();
        assert!(git(dir, &["add", "."]));
        assert!(git(dir, &["commit", "-q", "-m", "init"]));
    }

    struct Fixture {
        _tmp: tempfile::TempDir,
        repo: std::path::PathBuf,
        plain: std::path::PathBuf,
        project_id: String,
        tasks: Arc<SqliteTaskSessionRepository>,
        adapter: SquadWorkItemWorktreeAdapter,
    }

    async fn fixture() -> Option<Fixture> {
        if !git(Path::new("."), &["--version"]) {
            eprintln!("git not available; skipping");
            return None;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = dunce::canonicalize(tmp.path()).unwrap().join("repo");
        init_repo(&repo);
        let plain = repo.parent().unwrap().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let db = aionui_db::init_database_memory().await.unwrap();
        let service = WorktreeService::new(
            aionui_worktree::WorktreeStore::new(db.pool().clone()),
            aionui_worktree::WorktreeServiceConfig {
                root_dir: repo.parent().unwrap().join("worktrees"),
                git: aionui_worktree::GitCli::default(),
                events: aionui_worktree::WorktreeEvents::disabled(),
            },
        );
        let tasks = Arc::new(SqliteTaskSessionRepository::new(db.pool().clone()));
        service.set_task_sessions(tasks.clone());
        let projects = aionui_project::ProjectService::new(
            Arc::new(aionui_db::SqliteProjectStore::new(db.pool().clone())),
            repo.parent().unwrap().join("tmp-projects"),
        );
        let uri = aionui_project::canonical::to_file_uri(&repo).unwrap();
        let project_id = projects.create_standard(USER, uri).await.unwrap().project.project_id;
        Some(Fixture {
            _tmp: tmp,
            repo,
            plain,
            project_id,
            tasks: tasks.clone(),
            adapter: SquadWorkItemWorktreeAdapter {
                service,
                task_sessions: tasks,
                projects,
            },
        })
    }

    async fn item(f: &Fixture, project_id: Option<&str>) -> String {
        f.tasks
            .create(&CreateTaskSessionParams {
                user_id: USER,
                title: "Squad item",
                project_id,
                conversation_id: None,
                mode: "agent",
                objective: "g",
                acceptance_criteria: "[]",
                status: "ready",
                agent_type: "codex",
                agent_session_id: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn project_workspace_is_the_default_source_and_is_reused() {
        let Some(f) = fixture().await else { return };
        let id = item(&f, Some(&f.project_id)).await;
        let first = f.adapter.ensure(USER, &id, None, None).await.unwrap().expect("created");
        assert!(!first.reused);
        assert_eq!(Path::new(&first.source_repo_path), f.repo.as_path());
        assert!(first.branch.ends_with("-squad-item"), "{}", first.branch);
        assert!(Path::new(&first.path).join("README.md").is_file());
        // A later instance reuses it, whatever it passes.
        let plain = f.plain.to_string_lossy().into_owned();
        let again = f.adapter.ensure(USER, &id, None, Some(&plain)).await.unwrap().unwrap();
        assert!(again.reused);
        assert_eq!(
            (again.id.as_str(), again.path.as_str()),
            (first.id.as_str(), first.path.as_str())
        );
    }

    #[tokio::test]
    async fn explicit_and_workspace_sources() {
        let Some(f) = fixture().await else { return };
        let plain = f.plain.to_string_lossy().into_owned();
        let repo = f.repo.to_string_lossy().into_owned();

        // A non-git workspace keeps the plain shared workspace (no silent
        // switch to the project repository).
        let id = item(&f, Some(&f.project_id)).await;
        assert!(f.adapter.ensure(USER, &id, None, Some(&plain)).await.unwrap().is_none());
        // An explicit non-git repo_path is a caller error.
        let err = f.adapter.ensure(USER, &id, Some(&plain), None).await.unwrap_err();
        assert!(matches!(err, SquadWorktreeError::InvalidSource(_)), "{err:?}");
        // A git workspace is used directly.
        let wt = f.adapter.ensure(USER, &id, None, Some(&repo)).await.unwrap().unwrap();
        assert_eq!(Path::new(&wt.source_repo_path), f.repo.as_path());

        // No project, no source: nothing to do.
        let bare = item(&f, None).await;
        assert!(f.adapter.ensure(USER, &bare, None, None).await.unwrap().is_none());
    }
}
