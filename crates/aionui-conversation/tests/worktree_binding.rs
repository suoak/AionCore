//! E3-2: conversations bound to a managed worktree (port faked here; the
//! real worktree service is exercised end-to-end in aionui-app).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use aionui_ai_agent::{AgentError, IWorkerTaskManager};
use aionui_api_types::{
    CloneConversationRequest, CreateConversationRequest, UpdateConversationRequest, WebSocketMessage,
};
use aionui_common::{AgentKillReason, OnConversationDelete, TimestampMs};
use aionui_conversation::skill_resolver::SkillResolver;
use aionui_conversation::{
    ConversationError, ConversationService, IWorktreeBinding, WorktreeBindingError, WorktreeBindingTarget,
};
use aionui_db::{SqliteConversationRepository, init_database_memory};
use aionui_realtime::EventBroadcaster;
use serde_json::json;

struct TestBroadcaster;

impl TestBroadcaster {
    fn new() -> Self {
        Self
    }
}

impl EventBroadcaster for TestBroadcaster {
    fn broadcast(&self, _event: WebSocketMessage<serde_json::Value>) {}
}

struct NoopTaskManager;

#[async_trait::async_trait]
impl IWorkerTaskManager for NoopTaskManager {
    fn get_task(&self, _: &str) -> Option<aionui_ai_agent::AgentInstance> {
        None
    }
    async fn get_or_build_task(
        &self,
        _: &str,
        _: aionui_ai_agent::types::BuildTaskOptions,
    ) -> Result<aionui_ai_agent::AgentInstance, AgentError> {
        Err(AgentError::internal("noop"))
    }
    fn kill(&self, _: &str, _: Option<AgentKillReason>) -> Result<(), AgentError> {
        Ok(())
    }
    fn kill_and_wait(
        &self,
        _: &str,
        _: Option<AgentKillReason>,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
        Box::pin(std::future::ready(()))
    }
    async fn clear(&self) {}
    fn active_count(&self) -> usize {
        0
    }
    fn collect_idle(&self, _: TimestampMs) -> Vec<String> {
        vec![]
    }
}

struct EmptySkillResolver;

#[async_trait::async_trait]
impl SkillResolver for EmptySkillResolver {
    async fn auto_inject_names(&self) -> Vec<String> {
        Vec::new()
    }

    async fn resolve_skills(&self, _names: &[String]) -> Vec<aionui_extension::ResolvedAgentSkill> {
        Vec::new()
    }
}

async fn setup() -> (ConversationService, Arc<TestBroadcaster>, Arc<dyn IWorkerTaskManager>) {
    let db = init_database_memory().await.unwrap();
    let repo = Arc::new(SqliteConversationRepository::new(db.pool().clone()));
    let broadcaster = Arc::new(TestBroadcaster::new());
    let agent_metadata_repo: Arc<dyn aionui_db::IAgentMetadataRepository> =
        Arc::new(aionui_db::SqliteAgentMetadataRepository::new(db.pool().clone()));
    let acp_session_repo: Arc<dyn aionui_db::IAcpSessionRepository> =
        Arc::new(aionui_db::SqliteAcpSessionRepository::new(db.pool().clone()));
    let task_mgr: Arc<dyn IWorkerTaskManager> = Arc::new(NoopTaskManager);
    let svc = ConversationService::new(
        std::env::temp_dir(),
        broadcaster.clone(),
        Arc::new(EmptySkillResolver),
        task_mgr.clone(),
        repo,
        agent_metadata_repo,
        acp_session_repo,
    );
    (svc, broadcaster, task_mgr)
}

const USER_ID: &str = "system_default_user";

/// In-memory stand-in for the worktree service.
#[derive(Default)]
struct FakeWorktrees {
    /// id → (path, status, bound conversation)
    rows: Mutex<HashMap<String, (String, String, Option<String>)>>,
    /// Ids whose `bind` loses a race after `resolve` succeeded.
    steal_on_bind: Mutex<Vec<String>>,
    unbound: Mutex<Vec<String>>,
}

impl FakeWorktrees {
    fn add(&self, id: &str, status: &str) -> String {
        let dir = std::env::temp_dir().join(format!("aionui-e3-2-{id}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.to_string_lossy().into_owned();
        self.rows
            .lock()
            .unwrap()
            .insert(id.to_owned(), (path.clone(), status.to_owned(), None));
        path
    }
    fn bound(&self, id: &str) -> Option<String> {
        self.rows.lock().unwrap().get(id).and_then(|r| r.2.clone())
    }
}

#[async_trait::async_trait]
impl IWorktreeBinding for FakeWorktrees {
    async fn resolve(&self, user_id: &str, worktree_id: &str) -> Result<WorktreeBindingTarget, WorktreeBindingError> {
        assert_eq!(user_id, USER_ID);
        let rows = self.rows.lock().unwrap();
        let (path, status, bound) = rows.get(worktree_id).ok_or(WorktreeBindingError::NotFound)?;
        if status != "ready" && status != "unbound" {
            return Err(WorktreeBindingError::NotReady { status: status.clone() });
        }
        if bound.is_some() {
            return Err(WorktreeBindingError::AlreadyBound);
        }
        Ok(WorktreeBindingTarget {
            worktree_id: worktree_id.to_owned(),
            path: path.clone(),
            fanout_group_id: Some("fg_1".into()),
            fanout_slot: Some("B".into()),
        })
    }

    async fn bind(&self, _user_id: &str, worktree_id: &str, conversation_id: &str) -> Result<(), WorktreeBindingError> {
        if self.steal_on_bind.lock().unwrap().iter().any(|id| id == worktree_id) {
            return Err(WorktreeBindingError::AlreadyBound);
        }
        let mut rows = self.rows.lock().unwrap();
        let row = rows.get_mut(worktree_id).ok_or(WorktreeBindingError::NotFound)?;
        if row.2.is_some() {
            return Err(WorktreeBindingError::AlreadyBound);
        }
        row.2 = Some(conversation_id.to_owned());
        row.1 = "ready".into();
        Ok(())
    }
}

#[async_trait::async_trait]
impl OnConversationDelete for FakeWorktrees {
    async fn on_conversation_deleted(&self, _user_id: &str, conversation_id: &str) {
        let mut rows = self.rows.lock().unwrap();
        for (id, row) in rows.iter_mut() {
            if row.2.as_deref() == Some(conversation_id) {
                row.1 = "unbound".into();
                row.2 = None;
                self.unbound.lock().unwrap().push(id.clone());
            }
        }
    }
}

async fn setup_bound() -> (ConversationService, Arc<FakeWorktrees>, Arc<dyn IWorkerTaskManager>) {
    let (svc, _b, task_mgr) = setup().await;
    let fake = Arc::new(FakeWorktrees::default());
    svc.with_worktree_binding(fake.clone());
    svc.with_delete_hook(fake.clone());
    (svc, fake, task_mgr)
}

fn create_req(body: serde_json::Value) -> CreateConversationRequest {
    serde_json::from_value(body).unwrap()
}

fn patch(extra: serde_json::Value) -> UpdateConversationRequest {
    serde_json::from_value(json!({ "extra": extra })).unwrap()
}

fn other_dir() -> String {
    let dir = std::env::temp_dir().join("aionui-e3-2-other-workspace");
    std::fs::create_dir_all(&dir).unwrap();
    dir.to_string_lossy().into_owned()
}

#[tokio::test]
async fn create_with_worktree_id_binds_and_sets_workspace() {
    let (svc, fake, _tm) = setup_bound().await;
    let path = fake.add("wt_1", "ready");
    let conv = svc
        .create(
            USER_ID,
            create_req(json!({
                "type": "acp",
                "worktree_id": "wt_1",
                // A client workspace is overridden by the worktree path.
                "extra": { "backend": "claude", "workspace": other_dir() }
            })),
        )
        .await
        .unwrap();
    assert_eq!(conv.extra["workspace"], path.as_str());
    assert_eq!(conv.extra["worktree_id"], "wt_1");
    assert_eq!(conv.extra["fanout_group_id"], "fg_1");
    assert_eq!(conv.extra["fanout_slot"], "B");
    assert_eq!(fake.bound("wt_1").as_deref(), Some(conv.id.as_str()));
    assert!(std::path::Path::new(&path).is_dir());
}

#[tokio::test]
async fn forged_worktree_keys_in_extra_are_stripped() {
    let (svc, fake, _tm) = setup_bound().await;
    let path = fake.add("wt_1", "ready");
    let conv = svc
        .create(
            USER_ID,
            create_req(json!({
                "type": "acp",
                "extra": {
                    "backend": "claude",
                    "workspace": path,
                    "worktree_id": "wt_1",
                    "fanout_group_id": "fg_x",
                    "fanout_slot": "Z"
                }
            })),
        )
        .await
        .unwrap();
    for key in ["worktree_id", "fanout_group_id", "fanout_slot"] {
        assert!(conv.extra.get(key).is_none(), "{key} must be stripped");
    }
    assert_eq!(fake.bound("wt_1"), None, "forged key never binds");
}

#[tokio::test]
async fn create_rejects_unbindable_worktrees() {
    let (svc, fake, _tm) = setup_bound().await;
    fake.add("wt_creating", "creating");
    fake.add("wt_taken", "ready");
    fake.rows.lock().unwrap().get_mut("wt_taken").unwrap().2 = Some("someone".into());
    let cases = [
        ("wt_missing", "WORKTREE_NOT_FOUND"),
        ("wt_creating", "WORKTREE_INVALID_STATE"),
        ("wt_taken", "WORKTREE_ALREADY_BOUND"),
    ];
    for (id, want) in cases {
        let err = svc
            .create(
                USER_ID,
                create_req(json!({ "type": "acp", "worktree_id": id, "extra": {} })),
            )
            .await
            .unwrap_err();
        let text = format!("{err:?}");
        assert!(text.contains(want), "{id}: {text}");
    }
    let listed = svc.list(USER_ID, Default::default()).await.unwrap();
    assert!(listed.items.is_empty(), "no conversation is left behind");
}

#[tokio::test]
async fn losing_the_bind_race_rolls_the_conversation_back() {
    let (svc, fake, _tm) = setup_bound().await;
    fake.add("wt_1", "ready");
    fake.steal_on_bind.lock().unwrap().push("wt_1".into());
    let err = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": {} })),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::Busy { .. }), "{err:?}");
    let listed = svc.list(USER_ID, Default::default()).await.unwrap();
    assert!(listed.items.is_empty(), "rolled back");
}

#[tokio::test]
async fn worktree_id_without_wired_port_is_rejected() {
    let (svc, _b, _tm) = setup().await;
    let err = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": {} })),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }), "{err:?}");
}

#[tokio::test]
async fn bound_workspace_and_keys_are_locked_on_patch() {
    let (svc, fake, tm) = setup_bound().await;
    let path = fake.add("wt_1", "ready");
    let conv = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": {"backend": "claude"} })),
        )
        .await
        .unwrap();

    for extra in [
        json!({ "workspace": other_dir() }),
        json!({ "workspace": null }),
        json!({ "worktree_id": "wt_2" }),
        json!({ "worktree_id": null }),
        json!({ "fanout_slot": "C" }),
    ] {
        let err = svc
            .update(USER_ID, &conv.id, patch(extra.clone()), &tm)
            .await
            .unwrap_err();
        assert!(matches!(err, ConversationError::BadRequest { .. }), "{extra}: {err:?}");
    }
    // Internal callers cannot move it either.
    let err = svc
        .update_extra(USER_ID, &conv.id, json!({ "workspace": other_dir() }))
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }), "{err:?}");

    // Echoing stored values back is fine, and so are unrelated keys.
    svc.update(
        USER_ID,
        &conv.id,
        patch(json!({ "workspace": path, "worktree_id": "wt_1", "note": "x" })),
        &tm,
    )
    .await
    .unwrap();
    let fetched = svc.get(USER_ID, &conv.id).await.unwrap();
    assert_eq!(fetched.extra["workspace"], path.as_str());
    assert_eq!(fetched.extra["note"], "x");
}

#[tokio::test]
async fn unbound_conversations_still_move_their_workspace_but_cannot_claim_keys() {
    let (svc, _fake, tm) = setup_bound().await;
    let conv = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "extra": {"backend": "claude"} })),
        )
        .await
        .unwrap();
    svc.update(USER_ID, &conv.id, patch(json!({ "workspace": other_dir() })), &tm)
        .await
        .unwrap();
    let err = svc
        .update(USER_ID, &conv.id, patch(json!({ "worktree_id": "wt_1" })), &tm)
        .await
        .unwrap_err();
    assert!(matches!(err, ConversationError::BadRequest { .. }), "{err:?}");
}

#[tokio::test]
async fn delete_unbinds_and_keeps_the_directory() {
    let (svc, fake, _tm) = setup_bound().await;
    let path = fake.add("wt_1", "ready");
    let conv = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": {} })),
        )
        .await
        .unwrap();
    svc.delete(USER_ID, &conv.id).await.unwrap();
    assert_eq!(*fake.unbound.lock().unwrap(), ["wt_1".to_owned()]);
    assert!(
        std::path::Path::new(&path).is_dir(),
        "worktree directory is never auto-deleted"
    );
}

#[tokio::test]
async fn clone_does_not_inherit_the_worktree() {
    let (svc, fake, _tm) = setup_bound().await;
    fake.add("wt_1", "ready");
    let source = svc
        .create(
            USER_ID,
            create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": {} })),
        )
        .await
        .unwrap();
    let cloned = svc
        .clone_create(
            USER_ID,
            CloneConversationRequest {
                conversation: create_req(json!({ "type": "acp", "worktree_id": "wt_1", "extra": source.extra })),
            },
        )
        .await
        .unwrap();
    assert!(cloned.extra.get("worktree_id").is_none());
    assert_eq!(fake.bound("wt_1").as_deref(), Some(source.id.as_str()));
}
