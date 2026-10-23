//! E3-2: conversation binding, unbind-on-delete, diff and TaskSession trace.

mod common;

use std::path::Path;
use std::sync::Arc;

use aionui_db::{
    CreatePlanningTaskRunParams, CreateTaskSessionParams, ITaskSessionRepository, SqliteTaskSessionRepository,
};
use aionui_worktree::types::{CreateWorktreeRequest, RemoveWorktreeQuery, WorktreeResponse};
use common::*;

async fn ready_worktree(h: &Harness, task_session_id: Option<&str>) -> WorktreeResponse {
    let wt = h
        .service
        .create(
            USER,
            CreateWorktreeRequest {
                repo_path: h.repo.to_string_lossy().into_owned(),
                title: Some("bind".into()),
                task_session_id: task_session_id.map(str::to_owned),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    h.wait_status(&wt.id, "ready").await
}

async fn insert_conversation(h: &Harness, id: &str) {
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES (?, ?, 'c', 'acp', '{}', 'pending', 0, 0)",
    )
    .bind(id)
    .bind(USER)
    .execute(h.db.pool())
    .await
    .unwrap();
}

async fn delete_conversation(h: &Harness, id: &str) {
    sqlx::query("DELETE FROM conversations WHERE id = ?")
        .bind(id)
        .execute(h.db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn bind_is_exclusive_and_delete_leaves_unbound_directory() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    insert_conversation(&h, "conv-a").await;
    insert_conversation(&h, "conv-b").await;

    assert!(h.service.bind_conversation(USER, &wt.id, "conv-a").await.unwrap());
    // Compare-and-set: a second conversation cannot steal it.
    assert!(!h.service.bind_conversation(USER, &wt.id, "conv-b").await.unwrap());
    // Nor can another user bind it.
    assert!(!h.service.bind_conversation(OTHER_USER, &wt.id, "conv-b").await.unwrap());
    let row = h.service.binding_candidate(USER, &wt.id).await.unwrap();
    assert_eq!(row.conversation_id.as_deref(), Some("conv-a"));

    // A bound, ready worktree cannot be removed out from under its agent.
    let err = h
        .service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap_err();
    assert_eq!(err.code(), "WORKTREE_INVALID_STATE");

    // Conversation delete: hook first, then the row delete (FK nulls the id).
    h.service.on_conversation_deleted(USER, "conv-a").await;
    delete_conversation(&h, "conv-a").await;
    let after = h.service.get(USER, &wt.id).await.unwrap();
    assert_eq!(after.status, "unbound");
    assert!(Path::new(&after.path).join("README.md").is_file(), "directory is kept");
    assert!(worktree_listed(&h.repo, &after.path));
    assert!(h.events.statuses(&wt.id).ends_with(&["unbound".to_owned()]));
    let row = h.service.binding_candidate(USER, &wt.id).await.unwrap();
    assert_eq!(row.conversation_id, None);

    // An unbound worktree can be picked up by a new conversation.
    assert!(h.service.bind_conversation(USER, &wt.id, "conv-b").await.unwrap());
    assert_eq!(h.service.get(USER, &wt.id).await.unwrap().status, "ready");
}

#[tokio::test]
async fn rolled_back_conversation_delete_rebinds_after_grace() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    insert_conversation(&h, "conv-a").await;
    assert!(h.service.bind_conversation(USER, &wt.id, "conv-a").await.unwrap());

    h.service.on_conversation_deleted(USER, "conv-a").await;
    // The row delete has not committed yet: inside the grace window the
    // worktree stays unbound even though the conversation still exists.
    assert_eq!(h.service.get(USER, &wt.id).await.unwrap().status, "unbound");

    // The delete failed (row still there) and the grace window has passed.
    sqlx::query("UPDATE task_worktrees SET updated_at = updated_at - 60000 WHERE id = ?")
        .bind(&wt.id)
        .execute(h.db.pool())
        .await
        .unwrap();
    let back = h.service.get(USER, &wt.id).await.unwrap();
    assert_eq!(back.status, "ready");
    let row = h.service.binding_candidate(USER, &wt.id).await.unwrap();
    assert_eq!(row.conversation_id.as_deref(), Some("conv-a"));
}

#[tokio::test]
async fn unbind_ignores_other_users_and_unrelated_conversations() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    insert_conversation(&h, "conv-a").await;
    assert!(h.service.bind_conversation(USER, &wt.id, "conv-a").await.unwrap());
    h.service.on_conversation_deleted(OTHER_USER, "conv-a").await;
    h.service.on_conversation_deleted(USER, "conv-zzz").await;
    assert_eq!(h.service.get(USER, &wt.id).await.unwrap().status, "ready");
}

#[tokio::test]
async fn diff_stat_covers_committed_staged_unstaged_and_untracked() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    let dir = Path::new(&wt.path);
    // committed on the worktree branch
    commit_file(dir, "committed.txt", "a\nb\n");
    // staged
    std::fs::write(dir.join("staged.txt"), "s\n").unwrap();
    git(dir, &["add", "staged.txt"]);
    // unstaged edit of a base file
    std::fs::write(dir.join("README.md"), "hello\nmore\n").unwrap();
    // untracked, nested
    std::fs::create_dir_all(dir.join("new/deep")).unwrap();
    std::fs::write(dir.join("new/deep/u.txt"), "1\n2\n3\n").unwrap();
    // ignored files stay out
    std::fs::write(dir.join(".gitignore"), "*.log\n").unwrap();
    std::fs::write(dir.join("noise.log"), "x\n").unwrap();
    // binary
    std::fs::write(dir.join("blob.bin"), [0u8, 1, 2, 0, 3]).unwrap();

    let stat = h.service.diff_stat(USER, &wt.id).await.unwrap();
    assert_eq!(stat.base_commit, wt.base_commit);
    let find = |p: &str| {
        stat.files
            .iter()
            .find(|f| f.path == p)
            .unwrap_or_else(|| panic!("{p} missing: {:?}", stat.files))
    };
    assert_eq!(
        (find("committed.txt").status.as_str(), find("committed.txt").additions),
        ("added", 2)
    );
    assert_eq!(find("staged.txt").status, "added");
    let readme = find("README.md");
    assert_eq!(
        (readme.status.as_str(), readme.additions, readme.deletions),
        ("modified", 1, 0)
    );
    let untracked = find("new/deep/u.txt");
    assert_eq!((untracked.status.as_str(), untracked.additions), ("untracked", 3));
    assert!(find("blob.bin").binary);
    assert!(
        stat.files.iter().all(|f| f.path != "noise.log"),
        "ignored files are excluded"
    );
    assert_eq!(stat.files_changed as usize, stat.files.len());
    assert!(!stat.truncated);

    let diff = h.service.diff_file(USER, &wt.id, "new/deep/u.txt").await.unwrap();
    assert_eq!(diff.status, "untracked");
    assert!(diff.patch.contains("+1\n+2\n+3"), "{}", diff.patch);
    let diff = h.service.diff_file(USER, &wt.id, "README.md").await.unwrap();
    assert!(diff.patch.contains("+more"));
    let diff = h.service.diff_file(USER, &wt.id, "blob.bin").await.unwrap();
    assert!(diff.binary && diff.patch.is_empty());
    let diff = h.service.diff_file(USER, &wt.id, "missing.txt").await.unwrap();
    assert_eq!(diff.status, "unchanged");
    // Pathspec magic is not interpreted: ":(glob)*" is a literal name.
    let diff = h.service.diff_file(USER, &wt.id, ":(glob)*").await.unwrap();
    assert_eq!(diff.status, "unchanged");
    assert_eq!(
        h.service
            .diff_file(USER, &wt.id, "../repo/README.md")
            .await
            .unwrap_err()
            .code(),
        "WORKTREE_PATH_ESCAPE"
    );
}

#[tokio::test]
async fn diff_requires_a_live_worktree() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    std::fs::remove_dir_all(&wt.path).unwrap();
    let err = h.service.diff_stat(USER, &wt.id).await.unwrap_err();
    assert_eq!(err.code(), "WORKTREE_INVALID_STATE");
}

#[tokio::test]
async fn managed_id_for_path_matches_live_worktrees_only() {
    require_git!();
    let h = harness().await;
    let wt = ready_worktree(&h, None).await;
    assert_eq!(
        h.service.managed_id_for_path(Path::new(&wt.path)).await,
        Some(wt.id.clone())
    );
    assert_eq!(h.service.managed_id_for_path(&h.repo).await, None);
    h.service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap();
    h.wait_status(&wt.id, "removed").await;
    assert_eq!(h.service.managed_id_for_path(Path::new(&wt.path)).await, None);
}

#[tokio::test]
async fn task_session_link_is_checked_and_traced() {
    require_git!();
    let h = harness().await;
    let repo = Arc::new(SqliteTaskSessionRepository::new(h.db.pool().clone()));
    h.service.set_task_sessions(repo.clone());
    insert_conversation(&h, "conv-task").await;
    let task = repo
        .create(&CreateTaskSessionParams {
            user_id: USER,
            title: "t",
            project_id: None,
            conversation_id: Some("conv-task"),
            mode: "plan",
            objective: "g",
            acceptance_criteria: "[]",
            status: "ready",
            agent_type: "codex",
            agent_session_id: None,
        })
        .await
        .unwrap();
    let run = repo
        .create_planning_run(&CreatePlanningTaskRunParams {
            user_id: USER,
            task_session_id: &task.id,
            conversation_id: "conv-task",
            agent_id: "codex",
            agent_runtime: None,
            model: None,
            planning_isolation: "read_only",
            started_at: 2,
        })
        .await
        .unwrap();

    // Someone else's (or a missing) TaskSession is refused up front.
    let err = h
        .service
        .create(
            OTHER_USER,
            CreateWorktreeRequest {
                repo_path: h.repo.to_string_lossy().into_owned(),
                task_session_id: Some(task.id.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "BAD_REQUEST");

    let wt = ready_worktree(&h, Some(&task.id)).await;
    insert_conversation(&h, "conv-b").await;
    assert!(h.service.bind_conversation(USER, &wt.id, "conv-b").await.unwrap());
    h.service.on_conversation_deleted(USER, "conv-b").await;

    let events = repo.list_trace_events(USER, &task.id, &run.id).await.unwrap();
    let ours: Vec<_> = events
        .iter()
        .filter(|e| e.event_type.starts_with("worktree."))
        .collect();
    let types: Vec<&str> = ours.iter().map(|e| e.event_type.as_str()).collect();
    assert_eq!(types, ["worktree.created", "worktree.bound", "worktree.unbound"]);
    let payload: serde_json::Value = serde_json::from_str(&ours[0].payload).unwrap();
    assert_eq!(payload["worktree_id"], wt.id.as_str());
    assert_eq!(payload["branch"], wt.branch.as_str());
}
