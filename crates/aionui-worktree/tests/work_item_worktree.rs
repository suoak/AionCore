//! M8 Q12 / D7-3: one shared worktree per E6 work item.

mod common;

use std::path::Path;
use std::sync::Arc;

use aionui_db::{
    CreatePlanningTaskRunParams, CreateTaskSessionParams, ITaskSessionRepository, SqliteTaskSessionRepository,
};
use aionui_worktree::types::{CreateWorktreeRequest, ListWorktreesQuery};
use aionui_worktree::{WorkItemWorktreeSource, WorktreeError};
use common::*;

async fn work_item(h: &Harness, repo: &SqliteTaskSessionRepository) -> String {
    work_item_in(h, repo, None).await
}

async fn work_item_in(h: &Harness, repo: &SqliteTaskSessionRepository, conversation_id: Option<&str>) -> String {
    repo.create(&CreateTaskSessionParams {
        user_id: USER,
        title: "Fix login",
        project_id: None,
        conversation_id,
        mode: "plan",
        objective: "g",
        acceptance_criteria: "[]",
        status: "ready",
        agent_type: "codex",
        agent_session_id: None,
    })
    .await
    .unwrap_or_else(|e| panic!("create work item ({}): {e}", h.repo.display()))
    .id
}

fn source(path: &Path, explicit: bool) -> WorkItemWorktreeSource {
    WorkItemWorktreeSource {
        repo_path: path.to_string_lossy().into_owned(),
        explicit,
    }
}

fn tasks(h: &Harness) -> Arc<SqliteTaskSessionRepository> {
    let repo = Arc::new(SqliteTaskSessionRepository::new(h.db.pool().clone()));
    h.service.set_task_sessions(repo.clone());
    repo
}

#[tokio::test]
async fn first_start_creates_and_later_starts_reuse() {
    require_git!();
    let h = harness().await;
    let ts = tasks(&h);
    let item = work_item(&h, &ts).await;

    let first = h
        .service
        .ensure_for_work_item(USER, &item, Some("Fix login"), &[source(&h.repo, true)])
        .await
        .unwrap()
        .expect("git source creates a worktree");
    assert!(!first.reused);
    assert_eq!(first.row.status, "ready");
    assert_eq!(first.row.task_session_id.as_deref(), Some(item.as_str()));
    assert!(first.row.fanout_group_id.is_none());
    assert!(first.row.branch.ends_with("-fix-login"), "{}", first.row.branch);
    assert!(Path::new(&first.row.path).join("README.md").is_file());

    // Later starts reuse it even without (or with a different) source.
    let other = tempfile::tempdir().unwrap();
    let other_repo = init_repo(other.path());
    for sources in [vec![], vec![source(&other_repo, true)]] {
        let again = h
            .service
            .ensure_for_work_item(USER, &item, None, &sources)
            .await
            .unwrap()
            .expect("reused");
        assert!(again.reused);
        assert_eq!(again.row.id, first.row.id);
        assert_eq!(again.row.path, first.row.path);
    }

    let linked = h
        .service
        .list(
            USER,
            ListWorktreesQuery {
                task_session_id: Some(item.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(linked.len(), 1, "exactly one worktree per work item");
    assert_eq!(linked[0].id, first.row.id);

    // Another work item gets its own worktree; another user sees nothing.
    let item2 = work_item(&h, &ts).await;
    let second = h
        .service
        .ensure_for_work_item(USER, &item2, None, &[source(&h.repo, true)])
        .await
        .unwrap()
        .unwrap();
    assert_ne!(second.row.id, first.row.id);
    assert!(h.service.work_item_worktree(OTHER_USER, &item).await.unwrap().is_none());
}

#[tokio::test]
async fn concurrent_starts_share_one_worktree() {
    require_git!();
    let h = harness().await;
    let ts = tasks(&h);
    let item = work_item(&h, &ts).await;
    let sources = [source(&h.repo, true)];
    let (a, b, c) = tokio::join!(
        h.service.ensure_for_work_item(USER, &item, None, &sources),
        h.service.ensure_for_work_item(USER, &item, None, &sources),
        h.service.ensure_for_work_item(USER, &item, None, &sources),
    );
    let rows = [a.unwrap().unwrap(), b.unwrap().unwrap(), c.unwrap().unwrap()];
    assert!(rows.iter().all(|r| r.row.id == rows[0].row.id));
    assert_eq!(rows.iter().filter(|r| !r.reused).count(), 1);
}

#[tokio::test]
async fn sources_fall_back_or_fail() {
    require_git!();
    let h = harness().await;
    let ts = tasks(&h);
    let item = work_item(&h, &ts).await;
    let plain = h.tmp.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();

    // No source / only a non-git guess: keep the plain workspace.
    assert!(
        h.service
            .ensure_for_work_item(USER, &item, None, &[])
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        h.service
            .ensure_for_work_item(USER, &item, None, &[source(&plain, false)])
            .await
            .unwrap()
            .is_none()
    );
    // An explicit non-git source is an error.
    let err = h
        .service
        .ensure_for_work_item(USER, &item, None, &[source(&plain, true)])
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::NotARepository(_)), "{err:?}");
    // A non-git guess falls through to the next candidate.
    let wt = h
        .service
        .ensure_for_work_item(USER, &item, None, &[source(&plain, false), source(&h.repo, false)])
        .await
        .unwrap()
        .unwrap();
    assert!(!wt.reused);
    assert!(matches!(
        h.service.ensure_for_work_item(USER, " ", None, &[]).await.unwrap_err(),
        WorktreeError::BadRequest(_)
    ));
}

#[tokio::test]
async fn fanout_and_gone_worktrees_are_not_reused() {
    require_git!();
    let h = harness().await;
    let ts = tasks(&h);
    let item = work_item(&h, &ts).await;
    // A worktree created by hand for the item counts as its shared one.
    let manual = h
        .service
        .create(
            USER,
            CreateWorktreeRequest {
                repo_path: h.repo.to_string_lossy().into_owned(),
                task_session_id: Some(item.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let reused = h
        .service
        .ensure_for_work_item(USER, &item, None, &[])
        .await
        .unwrap()
        .expect("creating row is awaited and reused");
    assert!(reused.reused);
    assert_eq!(reused.row.id, manual.id);

    // Directory gone (outside our control) → orphaned, a fresh one is made.
    std::fs::remove_dir_all(&reused.row.path).unwrap();
    let fresh = h
        .service
        .ensure_for_work_item(USER, &item, None, &[source(&h.repo, true)])
        .await
        .unwrap()
        .unwrap();
    assert!(!fresh.reused);
    assert_ne!(fresh.row.id, manual.id);
    assert_eq!(h.service.get(USER, &manual.id).await.unwrap().status, "orphaned");
}

#[tokio::test]
async fn closing_the_work_item_only_marks_the_worktree() {
    require_git!();
    let h = harness().await;
    let ts = tasks(&h);
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES ('conv-q12', ?, 'c', 'acp', '{}', 'pending', 0, 0)",
    )
    .bind(USER)
    .execute(h.db.pool())
    .await
    .unwrap();
    let item = work_item_in(&h, &ts, Some("conv-q12")).await;
    let run = ts
        .create_planning_run(&CreatePlanningTaskRunParams {
            user_id: USER,
            task_session_id: &item,
            conversation_id: "conv-q12",
            agent_id: "codex",
            agent_runtime: None,
            model: None,
            planning_isolation: "read_only",
            started_at: 1,
        })
        .await
        .unwrap();
    let wt = h
        .service
        .ensure_for_work_item(USER, &item, None, &[source(&h.repo, true)])
        .await
        .unwrap()
        .unwrap();
    let before = h.events.statuses(&wt.row.id).len();

    let marked = h.service.mark_work_item_closed(USER, &item, "done").await;
    assert_eq!(marked.len(), 1);
    assert_eq!(marked[0].id, wt.row.id);

    let after = h.service.get(USER, &wt.row.id).await.unwrap();
    assert_eq!(after.status, "ready", "status untouched");
    assert!(Path::new(&after.path).is_dir(), "directory kept");
    assert!(branch_exists(&h.repo, &after.branch), "branch kept");
    assert!(worktree_listed(&h.repo, &after.path));
    assert_eq!(
        h.events.statuses(&wt.row.id).len(),
        before + 1,
        "worktree.changed emitted"
    );

    let events = ts.list_trace_events(USER, &item, &run.id).await.unwrap();
    let closed: Vec<_> = events
        .iter()
        .filter(|e| e.event_type == "worktree.work_item_closed")
        .collect();
    assert_eq!(closed.len(), 1);
    let payload: serde_json::Value = serde_json::from_str(&closed[0].payload).unwrap();
    assert_eq!(payload["board_status"], "done");
    assert_eq!(payload["kept"], true);

    // A closed item can be reopened and still reuses the same worktree.
    let again = h
        .service
        .ensure_for_work_item(USER, &item, None, &[])
        .await
        .unwrap()
        .unwrap();
    assert!(again.reused);
    assert_eq!(again.row.id, wt.row.id);
    // Work item without worktrees: nothing to mark.
    let empty = work_item(&h, &ts).await;
    assert!(
        h.service
            .mark_work_item_closed(USER, &empty, "cancelled")
            .await
            .is_empty()
    );
}
