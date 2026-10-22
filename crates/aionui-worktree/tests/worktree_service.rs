//! WorktreeService against real git in temporary repositories.

mod common;

use std::path::Path;
use std::time::Duration;

use aionui_worktree::WorktreeError;
use aionui_worktree::store::{NewWorktree, WorktreeStore};
use aionui_worktree::types::{BranchPolicy, CreateWorktreeRequest, ListWorktreesQuery, RemoveWorktreeQuery};
use common::*;

fn req(repo: &Path) -> CreateWorktreeRequest {
    CreateWorktreeRequest {
        repo_path: repo.to_string_lossy().into_owned(),
        title: Some("Fix login".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn create_reaches_ready_with_isolated_branch_and_path() {
    require_git!();
    let h = harness().await;
    let base = git(&h.repo, &["rev-parse", "HEAD"]);
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    assert_eq!(wt.status, "creating");
    assert!(
        wt.branch.starts_with("wm/") && wt.branch.ends_with("-fix-login"),
        "{}",
        wt.branch
    );
    assert_eq!(wt.base_ref, "HEAD");
    assert_eq!(wt.base_commit, base);

    let wt = h.wait_status(&wt.id, "ready").await;
    let path = Path::new(&wt.path);
    assert!(path.join("README.md").is_file());
    assert!(path.starts_with(dunce::canonicalize(&h.root).unwrap()));
    assert!(!path.starts_with(&h.repo), "worktree must not live inside the repo");
    assert!(branch_exists(&h.repo, &wt.branch));
    assert!(worktree_listed(&h.repo, &wt.path));
    assert_eq!((wt.ahead, wt.behind), (Some(0), Some(0)));
    assert_eq!(git(&h.repo, &["status", "--porcelain"]), "", "main checkout untouched");
    assert_eq!(h.events.statuses(&wt.id), ["creating", "ready"]);

    let listed = h.service.list(USER, ListWorktreesQuery::default()).await.unwrap();
    assert_eq!(listed.len(), 1);
    let by_repo = h
        .service
        .list(
            USER,
            ListWorktreesQuery {
                repo_path: Some(h.repo.to_string_lossy().into_owned()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(by_repo.len(), 1);
}

#[tokio::test]
async fn create_validates_inputs() {
    require_git!();
    let h = harness().await;
    let not_repo = h.tmp.path().join("plain");
    std::fs::create_dir_all(&not_repo).unwrap();
    let err = h.service.create(USER, req(&not_repo)).await.unwrap_err();
    assert!(matches!(err, WorktreeError::NotARepository(_)), "{err:?}");

    for bad in ["--upload-pack=touch /tmp/pwn", "main;id", "-b"] {
        let mut r = req(&h.repo);
        r.base_ref = Some(bad.into());
        let err = h.service.create(USER, r).await.unwrap_err();
        assert!(matches!(err, WorktreeError::InvalidRef(_)), "{bad}: {err:?}");
    }
    let mut r = req(&h.repo);
    r.base_ref = Some("no-such-branch".into());
    assert!(matches!(
        h.service.create(USER, r).await.unwrap_err(),
        WorktreeError::RefNotFound(_)
    ));

    for bad in ["-x", "a b", "a..b", "x.lock"] {
        let mut r = req(&h.repo);
        r.branch = Some(bad.into());
        let err = h.service.create(USER, r).await.unwrap_err();
        assert!(matches!(err, WorktreeError::InvalidRef(_)), "{bad}: {err:?}");
    }
    let mut r = req(&h.repo);
    r.branch = Some("main".into());
    assert!(matches!(
        h.service.create(USER, r).await.unwrap_err(),
        WorktreeError::BranchExists(_)
    ));
    assert!(
        h.service
            .list(USER, ListWorktreesQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn explicit_base_ref_and_branch() {
    require_git!();
    let h = harness().await;
    let first = git(&h.repo, &["rev-parse", "HEAD"]);
    commit_file(&h.repo, "second.txt", "2\n");
    let mut r = req(&h.repo);
    r.base_ref = Some("HEAD~1".into());
    r.branch = Some("feature/e3-test".into());
    let wt = h.service.create(USER, r).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    assert_eq!(wt.base_commit, first);
    assert_eq!(wt.branch, "feature/e3-test");
    assert!(!Path::new(&wt.path).join("second.txt").exists());
}

#[tokio::test]
async fn remove_auto_deletes_unchanged_branch() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    let removing = h
        .service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap();
    assert_eq!(removing.status, "removing");
    h.service.wait_idle(&wt.id).await;
    let done = h
        .service
        .list(
            USER,
            ListWorktreesQuery {
                include_removed: Some(true),
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .remove(0);
    assert_eq!(done.status, "removed");
    assert!(!done.branch_preserved);
    assert!(done.removed_at.is_some());
    assert!(!Path::new(&wt.path).exists());
    assert!(!branch_exists(&h.repo, &wt.branch));
    assert!(!worktree_listed(&h.repo, &wt.path));
    assert_eq!(h.events.statuses(&wt.id), ["creating", "ready", "removing", "removed"]);
    // Removed rows drop out of the default list.
    assert!(
        h.service
            .list(USER, ListWorktreesQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn remove_auto_preserves_branch_with_unmerged_commits() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    commit_file(Path::new(&wt.path), "work.txt", "work\n");
    let detail = h.service.get(USER, &wt.id).await.unwrap();
    assert_eq!(detail.ahead, Some(1));

    h.service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap();
    let done = h.wait_status(&wt.id, "removed").await;
    assert!(done.branch_preserved);
    assert!(
        branch_exists(&h.repo, &wt.branch),
        "unmerged branch must survive auto removal"
    );
    assert!(!Path::new(&wt.path).exists());
}

#[tokio::test]
async fn remove_auto_deletes_branch_merged_into_head() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    commit_file(Path::new(&wt.path), "work.txt", "work\n");
    git(&h.repo, &["merge", "-q", "--ff-only", &wt.branch]);
    h.service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap();
    let done = h.wait_status(&wt.id, "removed").await;
    assert!(!done.branch_preserved);
    assert!(!branch_exists(&h.repo, &wt.branch));
}

#[tokio::test]
async fn remove_keep_and_force_branch_policies() {
    require_git!();
    let h = harness().await;
    let keep = h.service.create(USER, req(&h.repo)).await.unwrap();
    let keep = h.wait_status(&keep.id, "ready").await;
    h.service
        .remove(
            USER,
            &keep.id,
            RemoveWorktreeQuery {
                branch: BranchPolicy::Keep,
                force_dir: false,
            },
        )
        .await
        .unwrap();
    let done = h.wait_status(&keep.id, "removed").await;
    assert!(done.branch_preserved);
    assert!(
        branch_exists(&h.repo, &keep.branch),
        "keep retains even an unchanged branch"
    );

    let force = h.service.create(USER, req(&h.repo)).await.unwrap();
    let force = h.wait_status(&force.id, "ready").await;
    commit_file(Path::new(&force.path), "work.txt", "work\n");
    h.service
        .remove(
            USER,
            &force.id,
            RemoveWorktreeQuery {
                branch: BranchPolicy::Force,
                force_dir: false,
            },
        )
        .await
        .unwrap();
    let done = h.wait_status(&force.id, "removed").await;
    assert!(!done.branch_preserved);
    assert!(!branch_exists(&h.repo, &force.branch), "force deletes unmerged branch");
}

#[tokio::test]
async fn dirty_worktree_needs_force_dir() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    std::fs::write(Path::new(&wt.path).join("untracked.txt"), "x").unwrap();
    let err = h
        .service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::Dirty(_)), "{err:?}");
    assert_eq!(h.service.get(USER, &wt.id).await.unwrap().status, "ready");
    assert!(Path::new(&wt.path).join("untracked.txt").exists());

    h.service
        .remove(
            USER,
            &wt.id,
            RemoveWorktreeQuery {
                branch: BranchPolicy::Auto,
                force_dir: true,
            },
        )
        .await
        .unwrap();
    h.wait_status(&wt.id, "removed").await;
    assert!(!Path::new(&wt.path).exists());
}

#[tokio::test]
async fn cancel_rolls_back_then_retry_succeeds() {
    require_git!();
    let h = harness_with_git(slow_git).await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    // Wait until the (slow) add has produced the checkout.
    for _ in 0..400 {
        if Path::new(&wt.path).join("README.md").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let cancelled = h.service.cancel(USER, &wt.id).await.unwrap();
    assert_eq!(cancelled.status, "failed");
    assert_eq!(cancelled.error.as_ref().unwrap().code, "CANCELLED");
    assert!(!Path::new(&wt.path).exists(), "partial checkout removed");
    assert!(
        !branch_exists(&h.repo, &wt.branch),
        "branch created by the cancelled attempt removed"
    );
    assert!(!worktree_listed(&h.repo, &wt.path));

    // Cancel only applies while creating.
    assert!(matches!(
        h.service.cancel(USER, &wt.id).await.unwrap_err(),
        WorktreeError::InvalidState { .. }
    ));

    std::fs::remove_file(h.tmp.path().join("slow")).unwrap();
    let retried = h.service.retry(USER, &wt.id).await.unwrap();
    assert_eq!(retried.status, "creating");
    assert!(retried.error.is_none());
    let ready = h.wait_status(&wt.id, "ready").await;
    assert!(Path::new(&ready.path).join("README.md").is_file());
    assert!(branch_exists(&h.repo, &ready.branch));
    assert!(matches!(
        h.service.retry(USER, &wt.id).await.unwrap_err(),
        WorktreeError::InvalidState { .. }
    ));
}

#[tokio::test]
async fn removing_while_creating_is_rejected() {
    require_git!();
    let h = harness_with_git(slow_git).await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let err = h
        .service
        .remove(USER, &wt.id, RemoveWorktreeQuery::default())
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::InvalidState { .. }), "{err:?}");
    h.service.cancel(USER, &wt.id).await.unwrap();
}

async fn insert_creating(h: &Harness, id: &str, path: &Path, branch: &str) {
    let base = git(&h.repo, &["rev-parse", "HEAD"]);
    let common = dunce::canonicalize(h.repo.join(".git")).unwrap();
    WorktreeStore::new(h.db.pool().clone())
        .insert(&NewWorktree {
            id: id.into(),
            user_id: USER.into(),
            source_repo_path: h.repo.to_string_lossy().into_owned(),
            repo_common_dir: common.to_string_lossy().into_owned(),
            path: path.to_string_lossy().into_owned(),
            branch: branch.into(),
            base_ref: "HEAD".into(),
            base_commit: base,
            task_session_id: None,
            now: 1,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn recover_on_boot_then_retry_adopts_or_rebuilds() {
    require_git!();
    let h = harness().await;
    let parent = h.root.join("deadbeef");
    std::fs::create_dir_all(&parent).unwrap();
    let parent = dunce::canonicalize(parent).unwrap();

    // 1) The previous process finished `worktree add` but died before
    //    marking the row ready; the user already has work in it.
    let healthy = parent.join("aaaa1111");
    git(
        &h.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "wm/aaaa1111",
            &healthy.to_string_lossy(),
            "HEAD",
        ],
    );
    std::fs::write(healthy.join("keep-me.txt"), "user work").unwrap();
    insert_creating(&h, "wt_healthy", &healthy, "wm/aaaa1111").await;

    // 2) Died before anything was created.
    let empty = parent.join("bbbb2222");
    insert_creating(&h, "wt_empty", &empty, "wm/bbbb2222").await;

    let report = h.second_service().recover_on_boot().await.unwrap();
    assert_eq!(report.interrupted_creates, 2);
    for id in ["wt_healthy", "wt_empty"] {
        let wt = h.service.get(USER, id).await.unwrap();
        assert_eq!(wt.status, "failed");
        assert_eq!(wt.error.unwrap().code, "INTERRUPTED");
    }

    h.service.retry(USER, "wt_healthy").await.unwrap();
    h.wait_status("wt_healthy", "ready").await;
    assert_eq!(
        std::fs::read_to_string(healthy.join("keep-me.txt")).unwrap(),
        "user work",
        "retry must not rebuild a healthy worktree"
    );

    h.service.retry(USER, "wt_empty").await.unwrap();
    h.wait_status("wt_empty", "ready").await;
    assert!(empty.join("README.md").is_file());
}

#[tokio::test]
async fn externally_deleted_worktree_is_orphaned_then_pruned() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    let wt = h.wait_status(&wt.id, "ready").await;
    std::fs::remove_dir_all(&wt.path).unwrap();

    let orphan = h.service.get(USER, &wt.id).await.unwrap();
    assert_eq!(orphan.status, "orphaned");
    assert!(worktree_listed(&h.repo, &wt.path), "git still has the stale entry");

    let pruned = h.service.prune(USER).await.unwrap();
    assert_eq!(pruned.pruned, vec![wt.id.clone()]);
    assert!(!worktree_listed(&h.repo, &wt.path), "stale admin entry dropped");
    let removed = h
        .service
        .list(
            USER,
            ListWorktreesQuery {
                status: Some("removed".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(removed.len(), 1);
    assert!(removed[0].branch_preserved, "prune never deletes branches");
    assert!(branch_exists(&h.repo, &wt.branch));
}

#[tokio::test]
async fn prune_leaves_unmanaged_worktrees_alone() {
    require_git!();
    let h = harness().await;
    let outside = h.tmp.path().join("user-wt");
    git(
        &h.repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "user/own",
            &outside.to_string_lossy(),
            "HEAD",
        ],
    );
    std::fs::remove_dir_all(&outside).unwrap();
    h.service.prune(USER).await.unwrap();
    h.second_service().recover_on_boot().await.unwrap();
    let list = git(&h.repo, &["worktree", "list", "--porcelain"]);
    assert!(
        list.contains("user-wt"),
        "unmanaged stale worktree must be left to the user: {list}"
    );
}

#[tokio::test]
async fn records_are_scoped_per_user() {
    require_git!();
    let h = harness().await;
    let wt = h.service.create(USER, req(&h.repo)).await.unwrap();
    h.wait_status(&wt.id, "ready").await;
    assert!(matches!(
        h.service.get(OTHER_USER, &wt.id).await.unwrap_err(),
        WorktreeError::NotFound(_)
    ));
    assert!(matches!(
        h.service
            .remove(OTHER_USER, &wt.id, RemoveWorktreeQuery::default())
            .await
            .unwrap_err(),
        WorktreeError::NotFound(_)
    ));
    assert!(
        h.service
            .list(OTHER_USER, ListWorktreesQuery::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(h.service.prune(OTHER_USER).await.unwrap().pruned.is_empty());
}

#[tokio::test]
async fn live_path_is_unique() {
    require_git!();
    let h = harness().await;
    let p = h.root.join("x").join("same");
    insert_creating(&h, "wt_a", &p, "wm/a").await;
    let base = git(&h.repo, &["rev-parse", "HEAD"]);
    let err = WorktreeStore::new(h.db.pool().clone())
        .insert(&NewWorktree {
            id: "wt_b".into(),
            user_id: USER.into(),
            source_repo_path: h.repo.to_string_lossy().into_owned(),
            repo_common_dir: "c".into(),
            path: p.to_string_lossy().into_owned(),
            branch: "wm/b".into(),
            base_ref: "HEAD".into(),
            base_commit: base,
            task_session_id: None,
            now: 1,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, WorktreeError::PathConflict(_)), "{err:?}");
}

#[tokio::test]
async fn capabilities_report_git() {
    require_git!();
    let h = harness().await;
    let caps = h.service.capabilities().await;
    assert!(caps.available);
    assert_eq!(caps.min_version, "2.31.0");

    let missing = harness_with_git(|_| aionui_worktree::GitCli::new("/no/such/git-binary")).await;
    let caps = missing.service.capabilities().await;
    assert!(!caps.available);
    assert_eq!(caps.error.unwrap().code, "GIT_NOT_AVAILABLE");
    let err = missing.service.create(USER, req(&missing.repo)).await.unwrap_err();
    assert_eq!(err.code(), "GIT_NOT_AVAILABLE");
}
