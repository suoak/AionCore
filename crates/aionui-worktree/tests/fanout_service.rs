//! E3-4 FanoutService against real git, with a scripted conversation port.

mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_worktree::fanout_types::*;
use aionui_worktree::{
    FanoutPrompt, FanoutService, FanoutStore, IFanoutConversations, SlotConversationSpec, SlotRuntime, WorktreeError,
    WorktreeService,
};
use async_trait::async_trait;
use common::*;

#[derive(Clone)]
enum Script {
    /// Write files, leave them uncommitted.
    Write(Vec<(&'static str, &'static str)>),
    /// Write files and commit them on the slot branch.
    Commit(Vec<(&'static str, &'static str)>),
    /// Stay busy until cancelled.
    Hold,
    /// Conversation creation fails.
    FailCreate,
}

#[derive(Default)]
struct FakeState {
    next: usize,
    paths: HashMap<String, PathBuf>,
    busy: HashMap<String, bool>,
    cancelled: Vec<String>,
    created: Vec<(String, String)>,
}

struct FakePort {
    worktrees: WorktreeService,
    pool: aionui_db::SqlitePool,
    scripts: HashMap<&'static str, Script>,
    state: Mutex<FakeState>,
}

#[async_trait]
impl IFanoutConversations for FakePort {
    async fn check_assistant(&self, _user_id: &str, assistant_id: &str) -> Result<(), String> {
        if self.scripts.contains_key(assistant_id) {
            Ok(())
        } else {
            Err("assistant not found".into())
        }
    }

    async fn create_slot_conversation(&self, user_id: &str, spec: SlotConversationSpec) -> Result<String, String> {
        if matches!(self.scripts.get(spec.assistant_id.as_str()), Some(Script::FailCreate)) {
            return Err("backend unavailable".into());
        }
        let id = {
            let mut s = self.state.lock().unwrap();
            s.next += 1;
            format!("conv-{}", s.next)
        };
        sqlx::query(
            "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
             VALUES (?, ?, ?, 'acp', '{}', 'pending', 1, 1)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(&spec.title)
        .execute(&self.pool)
        .await
        .map_err(|e| e.to_string())?;
        let row = self
            .worktrees
            .get(user_id, &spec.worktree_id)
            .await
            .map_err(|e| e.to_string())?;
        assert!(
            self.worktrees
                .bind_conversation(user_id, &spec.worktree_id, &id)
                .await
                .unwrap()
        );
        let mut s = self.state.lock().unwrap();
        s.paths.insert(id.clone(), PathBuf::from(row.path));
        s.created.push((id.clone(), spec.assistant_id));
        s.busy.insert(id.clone(), false);
        Ok(id)
    }

    async fn send_prompt(&self, _user_id: &str, conversation_id: &str, prompt: &FanoutPrompt) -> Result<(), String> {
        assert!(!prompt.content.is_empty());
        let (path, assistant) = {
            let s = self.state.lock().unwrap();
            let assistant = s
                .created
                .iter()
                .find(|(c, _)| c == conversation_id)
                .map(|(_, a)| a.clone())
                .unwrap();
            (s.paths[conversation_id].clone(), assistant)
        };
        match self.scripts[assistant.as_str()].clone() {
            Script::Write(files) => {
                for (name, body) in files {
                    std::fs::write(path.join(name), body).unwrap();
                }
            }
            Script::Commit(files) => {
                for (name, body) in files {
                    commit_file(&path, name, body);
                }
            }
            Script::Hold => {
                self.state.lock().unwrap().busy.insert(conversation_id.to_owned(), true);
            }
            Script::FailCreate => unreachable!(),
        }
        Ok(())
    }

    async fn runtime(&self, _user_id: &str, conversation_id: &str) -> SlotRuntime {
        SlotRuntime {
            busy: *self.state.lock().unwrap().busy.get(conversation_id).unwrap_or(&false),
        }
    }

    async fn cancel_turn(&self, _user_id: &str, conversation_id: &str) -> Result<(), String> {
        let mut s = self.state.lock().unwrap();
        s.busy.insert(conversation_id.to_owned(), false);
        s.cancelled.push(conversation_id.to_owned());
        Ok(())
    }

    async fn last_reply(&self, _user_id: &str, conversation_id: &str) -> Option<String> {
        Some(format!("done in {conversation_id}"))
    }
}

struct Fx {
    h: Harness,
    fanout: FanoutService,
    port: Arc<FakePort>,
}

async fn fixture(scripts: &[(&'static str, Script)]) -> Fx {
    let h = harness().await;
    let fanout = FanoutService::with_watch_interval(
        h.service.clone(),
        FanoutStore::new(h.db.pool().clone()),
        Duration::from_millis(30),
    );
    let port = Arc::new(FakePort {
        worktrees: h.service.clone(),
        pool: h.db.pool().clone(),
        scripts: scripts.iter().cloned().collect(),
        state: Mutex::default(),
    });
    fanout.set_conversations(port.clone());
    Fx { h, fanout, port }
}

fn slot(assistant_id: &str) -> FanoutSlotRequest {
    FanoutSlotRequest {
        assistant_id: assistant_id.into(),
        model: None,
        permission: None,
    }
}

fn group_req(repo: &Path, slots: &[&str]) -> CreateFanoutGroupRequest {
    CreateFanoutGroupRequest {
        repo_path: repo.to_string_lossy().into_owned(),
        base_ref: None,
        prompt: "Implement the feature".into(),
        files: Vec::new(),
        sessions: Vec::new(),
        task_session_id: None,
        max_parallel: None,
        title: Some("Feature".into()),
        slots: slots.iter().map(|a| slot(a)).collect(),
    }
}

async fn wait_group(fx: &Fx, id: &str, want: &str) -> FanoutGroupResponse {
    for _ in 0..400 {
        let g = fx.fanout.get(USER, id).await.unwrap();
        if g.status == want {
            return g;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("group {id} never reached {want}");
}

fn adopt(slot: &str, strategy: AdoptStrategy) -> AdoptFanoutRequest {
    AdoptFanoutRequest {
        slot: slot.into(),
        strategy,
        target_branch: None,
        snapshot_message: None,
        cleanup: AdoptCleanup::default(),
    }
}

#[tokio::test]
async fn three_slots_are_isolated_then_squash_adopt_keeps_only_the_winner() {
    require_git!();
    let fx = fixture(&[
        ("a1", Script::Write(vec![("a.txt", "from A\n")])),
        ("a2", Script::Commit(vec![("b.txt", "from B\n")])),
        ("a3", Script::Write(vec![("README.md", "rewritten by C\n")])),
    ])
    .await;
    let repo = &fx.h.repo;
    let base = git(repo, &["rev-parse", "HEAD"]);
    let g = fx
        .fanout
        .create(USER, group_req(repo, &["a1", "a2", "a3"]))
        .await
        .unwrap();
    assert_eq!(g.status, "preparing");
    assert_eq!(g.max_parallel, 3);
    assert_eq!(g.base_commit, base);
    assert_eq!(
        g.slots.iter().map(|s| s.slot.as_str()).collect::<Vec<_>>(),
        ["A", "B", "C"]
    );

    fx.fanout.wait_settled(&g.id).await;
    let g = wait_group(&fx, &g.id, "comparing").await;
    let paths: Vec<_> = g.slots.iter().map(|s| PathBuf::from(&s.path)).collect();
    let branches: Vec<_> = g.slots.iter().map(|s| s.branch.clone()).collect();
    assert!(g.slots.iter().all(|s| s.status == "finished"), "{:?}", g.slots);
    assert_eq!(
        branches.iter().collect::<std::collections::HashSet<_>>().len(),
        3,
        "distinct branches"
    );
    // Isolation: each slot only sees its own work; the source checkout is untouched.
    assert!(paths[0].join("a.txt").is_file() && !paths[1].join("a.txt").exists());
    assert!(paths[1].join("b.txt").is_file() && !paths[2].join("b.txt").exists());
    assert_eq!(std::fs::read_to_string(paths[0].join("README.md")).unwrap(), "hello\n");
    assert_eq!(git(repo, &["status", "--porcelain"]), "");

    let cmp = fx.fanout.compare(USER, &g.id).await.unwrap();
    let files: HashMap<_, _> = cmp.files.iter().map(|f| (f.path.as_str(), f)).collect();
    assert_eq!(files["a.txt"].slots.keys().collect::<Vec<_>>(), ["A"]);
    assert_eq!(files["b.txt"].slots.keys().collect::<Vec<_>>(), ["B"]);
    assert_eq!(files["README.md"].slots.keys().collect::<Vec<_>>(), ["C"]);
    assert!(
        cmp.slots
            .iter()
            .all(|s| s.summary.as_deref().is_some_and(|t| t.starts_with("done in")))
    );

    let done = fx
        .fanout
        .adopt(USER, &g.id, adopt("a", AdoptStrategy::Squash))
        .await
        .unwrap();
    assert_eq!(done.status, "adopted");
    assert_eq!(done.winner_slot.as_deref(), Some("A"));
    let result = done.adopt_result.clone().unwrap();
    assert!(result.snapshot_commit.is_some(), "dirty winner is snapshotted");
    assert_eq!(result.target_branch.as_deref(), Some("main"));
    assert_eq!(
        result.result_commit.as_deref(),
        Some(git(repo, &["rev-parse", "HEAD"]).as_str())
    );
    assert_eq!(git(repo, &["rev-list", "--count", &format!("{base}..HEAD")]), "1");
    assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "from A\n");
    assert_eq!(git(repo, &["status", "--porcelain"]), "");
    // Losers: worktrees removed; B's unmerged commit keeps its branch, C's
    // uncommitted edit goes with its directory.
    assert_eq!(result.removed_worktrees.len(), 2);
    assert_eq!(result.preserved_branches, [branches[1].clone()]);
    assert!(result.cleanup_errors.is_empty(), "{:?}", result.cleanup_errors);
    assert!(!paths[1].exists() && !paths[2].exists());
    assert!(branch_exists(repo, &branches[1]));
    assert!(!branch_exists(repo, &branches[2]));
    // The winner's worktree and branch stay.
    assert!(paths[0].is_dir() && branch_exists(repo, &branches[0]));
    let statuses: Vec<_> = done.slots.iter().map(|s| s.status.as_str()).collect();
    assert_eq!(statuses, ["adopted", "removed", "removed"]);

    // Terminal: no second adopt / discard / cancel.
    for err in [
        fx.fanout
            .adopt(USER, &g.id, adopt("B", AdoptStrategy::BranchOnly))
            .await
            .unwrap_err(),
        fx.fanout.discard(USER, &g.id).await.unwrap_err(),
        fx.fanout.cancel(USER, &g.id).await.unwrap_err(),
    ] {
        assert!(matches!(err, WorktreeError::FanoutInvalidState { .. }), "{err:?}");
    }
}

#[tokio::test]
async fn conflicting_adopt_is_aborted_and_target_left_clean() {
    require_git!();
    let fx = fixture(&[
        ("a1", Script::Commit(vec![("README.md", "slot A\n")])),
        ("a2", Script::Commit(vec![("README.md", "slot B\n")])),
    ])
    .await;
    let repo = &fx.h.repo;
    let g = fx.fanout.create(USER, group_req(repo, &["a1", "a2"])).await.unwrap();
    fx.fanout.wait_settled(&g.id).await;
    wait_group(&fx, &g.id, "comparing").await;
    commit_file(repo, "README.md", "main moved on\n");
    let head = git(repo, &["rev-parse", "HEAD"]);

    for strategy in [AdoptStrategy::Squash, AdoptStrategy::NoFf, AdoptStrategy::CherryPick] {
        let err = fx.fanout.adopt(USER, &g.id, adopt("A", strategy)).await.unwrap_err();
        assert!(
            matches!(&err, WorktreeError::AdoptConflict(m) if m.contains("README.md")),
            "{strategy:?}: {err:?}"
        );
        assert_eq!(err.code(), "ADOPT_CONFLICT");
        let git_dir = PathBuf::from(git(repo, &["rev-parse", "--absolute-git-dir"]));
        for marker in ["MERGE_HEAD", "CHERRY_PICK_HEAD"] {
            assert!(!git_dir.join(marker).exists(), "{strategy:?} left {marker}");
        }
        assert_eq!(git(repo, &["status", "--porcelain"]), "", "{strategy:?}");
        assert_eq!(git(repo, &["rev-parse", "HEAD"]), head);
    }
    assert_eq!(fx.fanout.get(USER, &g.id).await.unwrap().status, "comparing");

    // Target checks: dirty tracked file / another branch / detached HEAD.
    std::fs::write(repo.join("README.md"), "local edit\n").unwrap();
    let err = fx
        .fanout
        .adopt(USER, &g.id, adopt("A", AdoptStrategy::Squash))
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::TargetNotClean(_)), "{err:?}");
    git(repo, &["checkout", "--", "README.md"]);
    let mut other = adopt("A", AdoptStrategy::NoFf);
    other.target_branch = Some("develop".into());
    let err = fx.fanout.adopt(USER, &g.id, other).await.unwrap_err();
    assert!(matches!(err, WorktreeError::TargetNotClean(_)), "{err:?}");

    // branch_only never touches the target.
    let mut keep = adopt("B", AdoptStrategy::BranchOnly);
    keep.cleanup.keep_branches = true;
    let done = fx.fanout.adopt(USER, &g.id, keep).await.unwrap();
    let result = done.adopt_result.unwrap();
    assert_eq!(result.result_commit, None);
    assert_eq!(git(repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(result.preserved_branches.len(), 1);
}

#[tokio::test]
async fn no_ff_and_cherry_pick_adopt() {
    require_git!();
    let fx = fixture(&[
        ("a1", Script::Commit(vec![("one.txt", "1\n"), ("two.txt", "2\n")])),
        ("a2", Script::Commit(vec![("other.txt", "x\n")])),
    ])
    .await;
    let repo = &fx.h.repo;
    let base = git(repo, &["rev-parse", "HEAD"]);

    let g = fx.fanout.create(USER, group_req(repo, &["a1", "a2"])).await.unwrap();
    fx.fanout.wait_settled(&g.id).await;
    wait_group(&fx, &g.id, "comparing").await;
    let done = fx
        .fanout
        .adopt(USER, &g.id, adopt("A", AdoptStrategy::NoFf))
        .await
        .unwrap();
    let head = done.adopt_result.unwrap().result_commit.unwrap();
    assert_eq!(
        git(repo, &["rev-list", "--parents", "-n1", &head]).split(' ').count(),
        3,
        "merge commit"
    );

    let g = fx.fanout.create(USER, group_req(repo, &["a1", "a2"])).await.unwrap();
    fx.fanout.wait_settled(&g.id).await;
    wait_group(&fx, &g.id, "comparing").await;
    let before = git(repo, &["rev-parse", "HEAD"]);
    let done = fx
        .fanout
        .adopt(USER, &g.id, adopt("B", AdoptStrategy::CherryPick))
        .await
        .unwrap();
    assert_eq!(
        done.adopt_result.unwrap().result_commit.unwrap(),
        git(repo, &["rev-parse", "HEAD"])
    );
    assert_eq!(git(repo, &["rev-list", "--count", &format!("{before}..HEAD")]), "1");
    assert!(repo.join("other.txt").is_file());
    assert_ne!(base, before);
}

#[tokio::test]
async fn limits_and_validation() {
    require_git!();
    let fx = fixture(&[("a1", Script::Hold)]).await;
    let repo = &fx.h.repo;
    for n in [0usize, 1, 6] {
        let ids = vec!["a1"; n];
        let err = fx.fanout.create(USER, group_req(repo, &ids)).await.unwrap_err();
        assert!(matches!(err, WorktreeError::FanoutLimit(_)), "{n}: {err:?}");
        assert_eq!(err.code(), "FANOUT_LIMIT");
    }
    for p in [0u8, 6] {
        let mut r = group_req(repo, &["a1", "a1"]);
        r.max_parallel = Some(p);
        assert!(matches!(
            fx.fanout.create(USER, r).await.unwrap_err(),
            WorktreeError::FanoutLimit(_)
        ));
    }
    let err = fx
        .fanout
        .create(USER, group_req(repo, &["a1", "nope"]))
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::BadRequest(_)), "{err:?}");
    let mut r = group_req(repo, &["a1", "a1"]);
    r.prompt = "  ".into();
    assert!(matches!(
        fx.fanout.create(USER, r).await.unwrap_err(),
        WorktreeError::BadRequest(_)
    ));
    // Nothing was created by any rejected request.
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_worktrees")
        .fetch_one(fx.h.db.pool())
        .await
        .unwrap();
    assert_eq!(rows, 0);

    // Unknown slot fields are rejected (no backend / inline MCP override).
    assert!(
        serde_json::from_value::<FanoutSlotRequest>(serde_json::json!({"assistant_id": "a", "backend": "claude"}))
            .is_err()
    );

    // Without a wired conversation port the service is unavailable.
    let bare = FanoutService::new(fx.h.service.clone(), FanoutStore::new(fx.h.db.pool().clone()));
    let err = bare.create(USER, group_req(repo, &["a1", "a1"])).await.unwrap_err();
    assert_eq!(err.code(), "FANOUT_UNAVAILABLE");
    assert!(matches!(
        fx.fanout.get(USER, "fg_missing").await.unwrap_err(),
        WorktreeError::FanoutNotFound(_)
    ));
}

#[tokio::test]
async fn permission_override_is_ignored_and_max_parallel_defaults() {
    require_git!();
    let fx = fixture(&[("a1", Script::Write(vec![("x.txt", "x\n")]))]).await;
    let mut r = group_req(&fx.h.repo, &["a1", "a1"]);
    r.slots[0].permission = Some("yolo".into());
    let g = fx.fanout.create(USER, r).await.unwrap();
    assert_eq!(g.max_parallel, 2, "min(default 3, slots)");
    let spec: String =
        sqlx::query_scalar("SELECT slot_spec FROM task_worktrees WHERE fanout_group_id = ? AND slot = 'A'")
            .bind(&g.id)
            .fetch_one(fx.h.db.pool())
            .await
            .unwrap();
    assert!(!spec.contains("yolo") && !spec.contains("permission"), "{spec}");
    fx.fanout.wait_settled(&g.id).await;
    // Other users cannot see it.
    assert!(matches!(
        fx.fanout.get(OTHER_USER, &g.id).await.unwrap_err(),
        WorktreeError::FanoutNotFound(_)
    ));
    assert!(
        fx.fanout
            .list(OTHER_USER, ListFanoutGroupsQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        fx.fanout
            .list(USER, ListFanoutGroupsQuery::default())
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn busy_slot_blocks_adopt_then_cancel_and_discard_clean_up() {
    require_git!();
    let fx = fixture(&[("hold", Script::Hold), ("broken", Script::FailCreate)]).await;
    let repo = &fx.h.repo;
    let g = fx
        .fanout
        .create(USER, group_req(repo, &["hold", "hold", "broken"]))
        .await
        .unwrap();
    let g = wait_group(&fx, &g.id, "running").await;
    let mut g = fx.fanout.get(USER, &g.id).await.unwrap();
    for _ in 0..200 {
        if g.slots.iter().filter(|s| s.status == "running").count() == 2 && g.slots[2].status == "failed" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
        g = fx.fanout.get(USER, &g.id).await.unwrap();
    }
    assert_eq!(
        g.slots.iter().map(|s| s.status.as_str()).collect::<Vec<_>>(),
        ["running", "running", "failed"]
    );
    assert_eq!(g.slots[2].error.as_ref().unwrap().code, SLOT_START_FAILED);

    let err = fx
        .fanout
        .adopt(USER, &g.id, adopt("A", AdoptStrategy::BranchOnly))
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::SlotBusy(_)), "{err:?}");

    let c = fx.fanout.cancel(USER, &g.id).await.unwrap();
    assert_eq!(c.status, "cancelled");
    assert_eq!(fx.port.state.lock().unwrap().cancelled.len(), 2);
    assert!(c.slots[..2].iter().all(|s| s.status == "finished"));

    let paths: Vec<_> = c.slots.iter().map(|s| PathBuf::from(&s.path)).collect();
    let d = fx.fanout.discard(USER, &g.id).await.unwrap();
    assert_eq!(d.status, "discarded");
    assert!(
        d.slots
            .iter()
            .all(|s| s.status == "removed" && s.worktree_status == "removed"),
        "{:?}",
        d.slots
    );
    assert!(paths.iter().all(|p| !p.exists()));
    assert!(d.slots.iter().all(|s| !branch_exists(repo, &s.branch)));
    assert!(matches!(
        fx.fanout.discard(USER, &g.id).await.unwrap_err(),
        WorktreeError::FanoutInvalidState { .. }
    ));
    let changed = fx
        .h
        .events
        .events
        .lock()
        .unwrap()
        .iter()
        .filter(|e| e.name == "fanout.changed" && e.data["group_id"] == g.id.as_str() && e.data.get("slot").is_none())
        .map(|e| e.data["status"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(changed, ["preparing", "running", "cancelled", "discarded"]);
}

#[tokio::test]
async fn group_fails_when_no_slot_starts() {
    require_git!();
    let fx = fixture(&[("broken", Script::FailCreate)]).await;
    let g = fx
        .fanout
        .create(USER, group_req(&fx.h.repo, &["broken", "broken"]))
        .await
        .unwrap();
    fx.fanout.wait_settled(&g.id).await;
    let g = fx.fanout.get(USER, &g.id).await.unwrap();
    assert_eq!(g.status, "failed");
    assert!(g.slots.iter().all(|s| s.status == "failed"));
    // A failed group can still be discarded.
    assert_eq!(fx.fanout.discard(USER, &g.id).await.unwrap().status, "discarded");
}
