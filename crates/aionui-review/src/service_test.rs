use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::*;
use crate::resolver::AllowedRootsResolver;

struct Fixture {
    service: ReviewService,
    _db: aionui_db::Database,
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    scope_id: String,
}

const FILE: &str = "src/lib.rs";

fn base_lines() -> Vec<String> {
    (1..=20).map(|i| format!("line {i}")).collect()
}

fn write(repo: &Path, rel: &str, lines: &[String]) {
    let path = repo.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut text = lines.join("\n");
    text.push('\n');
    std::fs::write(path, text).unwrap();
}

async fn fixture() -> Fixture {
    let db = aionui_db::init_database_memory().await.unwrap();
    for user in ["u1", "u2"] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(db.pool())
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES ('conv-u1', 'u1', 'c', 'acp', '{}', 'pending', 1, 1)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let repo = std::fs::canonicalize(tmp.path()).unwrap().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    write(&repo, FILE, &base_lines());
    let resolver = Arc::new(AllowedRootsResolver::new(vec![tmp.path().to_path_buf()]));
    let service = ReviewService::new(db.pool().clone(), resolver);
    let scope_id = std::fs::canonicalize(&repo).unwrap().to_string_lossy().into_owned();
    Fixture {
        service,
        _db: db,
        _tmp: tmp,
        repo,
        scope_id,
    }
}

fn scm_request(fx: &Fixture, start: u32, end: u32, lines: &[&str]) -> CreateReviewThreadRequest {
    CreateReviewThreadRequest {
        scope_kind: "scm".into(),
        scope_id: fx.repo.to_string_lossy().into_owned(),
        conversation_id: Some("conv-u1".into()),
        task_session_id: None,
        run_id: None,
        file_path: FILE.into(),
        side: "new".into(),
        start_line: start,
        end_line: end,
        body: "please handle the **error** path".into(),
        diff_context: DiffContext {
            hunk_header: Some("@@ -1,20 +1,20 @@".into()),
            base: None,
            before: Vec::new(),
            lines: lines.iter().map(|l| (*l).to_owned()).collect(),
            after: Vec::new(),
        },
    }
}

fn list_query(fx: &Fixture) -> ListReviewThreadsQuery {
    ListReviewThreadsQuery {
        scope_kind: Some("scm".into()),
        scope_id: Some(fx.scope_id.clone()),
        file_path: None,
        status: None,
    }
}

async fn only(fx: &Fixture) -> ReviewThread {
    let mut threads = fx.service.list("u1", list_query(fx)).await.unwrap();
    assert_eq!(threads.len(), 1);
    threads.pop().unwrap()
}

#[tokio::test]
async fn create_builds_anchor_from_the_file_not_the_client() {
    let fx = fixture().await;
    let thread = fx
        .service
        .create("u1", scm_request(&fx, 5, 6, &["line 5", "line 6"]))
        .await
        .unwrap();
    assert_eq!(thread.scope_id, fx.scope_id);
    assert_eq!((thread.start_line, thread.end_line), (5, 6));
    assert_eq!(thread.status, ReviewThreadStatus::Open);
    assert_eq!(thread.anchor.lines, vec!["line 5", "line 6"]);
    assert_eq!(thread.anchor.before, vec!["line 2", "line 3", "line 4"]);
    assert_eq!(thread.anchor.after, vec!["line 7", "line 8", "line 9"]);
    assert_eq!(thread.anchor.base.as_deref(), Some("WORKTREE"));
    assert!(thread.anchor.content_sha.is_some());
    assert_eq!(thread.comments.len(), 1);
    assert_eq!(thread.comments[0].author, ReviewCommentAuthor::User);
    assert_eq!(thread.conversation_id.as_deref(), Some("conv-u1"));
}

#[tokio::test]
async fn create_follows_a_stale_diff_or_refuses_it() {
    let fx = fixture().await;
    // The UI's diff says "line 5" is at 3 (two lines were deleted since).
    let thread = fx
        .service
        .create("u1", scm_request(&fx, 3, 3, &["line 5"]))
        .await
        .unwrap();
    assert_eq!((thread.start_line, thread.end_line), (5, 5));

    let err = fx
        .service
        .create("u1", scm_request(&fx, 3, 3, &["not in the file"]))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: "ANCHOR_STALE",
                ..
            }
        ),
        "{err:?}"
    );

    let err = fx
        .service
        .create("u1", scm_request(&fx, 30, 31, &[]))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: "ANCHOR_STALE",
                ..
            }
        ),
        "{err:?}"
    );
}

#[tokio::test]
async fn lazy_reanchor_tracks_edits_and_persists() {
    let fx = fixture().await;
    let created = fx
        .service
        .create("u1", scm_request(&fx, 10, 11, &["line 10", "line 11"]))
        .await
        .unwrap();

    // Three lines inserted above → moves down by three.
    let mut lines = base_lines();
    for i in 0..3 {
        lines.insert(2, format!("inserted {i}"));
    }
    write(&fx.repo, FILE, &lines);
    let thread = only(&fx).await;
    assert_eq!((thread.start_line, thread.end_line), (13, 14));
    assert_eq!(thread.status, ReviewThreadStatus::Open);
    assert!(!thread.moved_fuzzy);

    // Persisted: a direct read sees the new position without re-reading.
    let row = fx.service.fetch_row("u1", &created.id).await.unwrap();
    assert_eq!((row.start_line, row.end_line), (13, 14));

    // Rewrite the selected lines → fuzzy, original kept.
    lines[12] = "rewritten 10".into();
    lines[13] = "rewritten 11".into();
    write(&fx.repo, FILE, &lines);
    let thread = only(&fx).await;
    assert_eq!((thread.start_line, thread.end_line), (13, 14));
    assert!(thread.moved_fuzzy);
    assert_eq!(
        thread.anchor.original_lines,
        Some(vec!["line 10".to_owned(), "line 11".to_owned()])
    );

    // Delete the whole block → outdated, snapshot retained.
    lines.drain(12..14);
    write(&fx.repo, FILE, &lines);
    let thread = only(&fx).await;
    assert_eq!(thread.status, ReviewThreadStatus::Outdated);
    assert_eq!(thread.anchor.lines, vec!["rewritten 10", "rewritten 11"]);

    // Status filter sees the fresh state.
    let mut query = list_query(&fx);
    query.status = Some("open".into());
    assert!(fx.service.list("u1", query).await.unwrap().is_empty());

    // Put the block back → found again and live.
    lines.insert(12, "rewritten 11".into());
    lines.insert(12, "rewritten 10".into());
    write(&fx.repo, FILE, &lines);
    let thread = only(&fx).await;
    assert_eq!(thread.status, ReviewThreadStatus::Open);
    assert_eq!((thread.start_line, thread.end_line), (13, 14));
}

#[tokio::test]
async fn crlf_rewrite_of_the_file_keeps_the_thread() {
    let fx = fixture().await;
    fx.service
        .create("u1", scm_request(&fx, 4, 4, &["line 4\r"]))
        .await
        .unwrap();
    let mut text = base_lines().join("\r\n");
    text.insert_str(0, "header\r\n");
    std::fs::write(fx.repo.join(FILE), text).unwrap();
    let thread = only(&fx).await;
    assert_eq!((thread.start_line, thread.end_line), (5, 5));
    assert_eq!(thread.status, ReviewThreadStatus::Open);
}

#[tokio::test]
async fn deleted_file_marks_threads_outdated() {
    let fx = fixture().await;
    fx.service
        .create("u1", scm_request(&fx, 1, 1, &["line 1"]))
        .await
        .unwrap();
    std::fs::remove_file(fx.repo.join(FILE)).unwrap();
    assert_eq!(only(&fx).await.status, ReviewThreadStatus::Outdated);
}

#[tokio::test]
async fn resolved_threads_are_frozen_until_reopened() {
    let fx = fixture().await;
    let created = fx
        .service
        .create("u1", scm_request(&fx, 2, 2, &["line 2"]))
        .await
        .unwrap();
    let resolved = fx.service.resolve("u1", &created.id).await.unwrap();
    assert_eq!(resolved.status, ReviewThreadStatus::Resolved);
    assert!(resolved.resolved_at.is_some());
    // Idempotent.
    assert_eq!(
        fx.service.resolve("u1", &created.id).await.unwrap().status,
        ReviewThreadStatus::Resolved
    );

    let mut lines = base_lines();
    lines.insert(0, "top".into());
    write(&fx.repo, FILE, &lines);
    let thread = only(&fx).await;
    assert_eq!(thread.start_line, 2, "resolved threads are not re-anchored");

    let reopened = fx.service.reopen("u1", &created.id).await.unwrap();
    assert_eq!(reopened.status, ReviewThreadStatus::Open);
    assert!(reopened.resolved_at.is_none());
    assert_eq!(reopened.start_line, 3, "reopen re-anchors right away");
}

#[tokio::test]
async fn update_body_range_and_comments() {
    let fx = fixture().await;
    let created = fx
        .service
        .create("u1", scm_request(&fx, 2, 2, &["line 2"]))
        .await
        .unwrap();

    let updated = fx
        .service
        .update(
            "u1",
            &created.id,
            UpdateReviewThreadRequest {
                body: Some("edited".into()),
                start_line: Some(7),
                end_line: Some(9),
                diff_context: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(updated.comments[0].body, "edited");
    assert_eq!((updated.start_line, updated.end_line), (7, 9));
    assert_eq!(updated.anchor.lines, vec!["line 7", "line 8", "line 9"]);

    let replied = fx
        .service
        .add_comment(
            "u1",
            &created.id,
            AddReviewCommentRequest {
                body: "one more".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(replied.comments.len(), 2);
    assert_eq!(replied.comments[1].body, "one more");
    // Editing the body later still targets the first comment.
    let again = fx
        .service
        .update(
            "u1",
            &created.id,
            UpdateReviewThreadRequest {
                body: Some("edited twice".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(again.comments[0].body, "edited twice");
    assert_eq!(again.comments[1].body, "one more");

    let err = fx
        .service
        .update("u1", &created.id, UpdateReviewThreadRequest::default())
        .await
        .unwrap_err();
    assert!(matches!(err, ReviewError::BadRequest(_)));
}

#[tokio::test]
async fn delete_cascades_comments() {
    let fx = fixture().await;
    let created = fx
        .service
        .create("u1", scm_request(&fx, 2, 2, &["line 2"]))
        .await
        .unwrap();
    fx.service.delete("u1", &created.id).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM review_comments WHERE thread_id = ?")
        .bind(&created.id)
        .fetch_one(&fx.service.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
    assert!(matches!(
        fx.service.delete("u1", &created.id).await,
        Err(ReviewError::NotFound)
    ));
}

#[tokio::test]
async fn other_users_cannot_see_or_touch_threads() {
    let fx = fixture().await;
    let created = fx
        .service
        .create("u1", scm_request(&fx, 2, 2, &["line 2"]))
        .await
        .unwrap();
    assert!(fx.service.list("u2", list_query(&fx)).await.unwrap().is_empty());
    assert!(matches!(
        fx.service.get("u2", &created.id).await,
        Err(ReviewError::NotFound)
    ));
    assert!(matches!(
        fx.service.resolve("u2", &created.id).await,
        Err(ReviewError::NotFound)
    ));
    assert!(matches!(
        fx.service.delete("u2", &created.id).await,
        Err(ReviewError::NotFound)
    ));
    assert!(matches!(
        fx.service
            .add_comment("u2", &created.id, AddReviewCommentRequest { body: "x".into() })
            .await,
        Err(ReviewError::NotFound)
    ));

    // u2 cannot target u1's conversation either.
    let mut req = scm_request(&fx, 2, 2, &["line 2"]);
    req.conversation_id = Some("conv-u1".into());
    assert!(matches!(
        fx.service.create("u2", req).await,
        Err(ReviewError::BadRequest(_))
    ));
}

#[tokio::test]
async fn task_run_scope_uses_context_and_is_not_reanchored() {
    let fx = fixture().await;
    let req = CreateReviewThreadRequest {
        scope_kind: "task_run".into(),
        scope_id: "run-1".into(),
        conversation_id: None,
        task_session_id: None,
        run_id: Some("run-1".into()),
        file_path: "a/b.rs".into(),
        side: "new".into(),
        start_line: 3,
        end_line: 4,
        body: "why?".into(),
        diff_context: DiffContext {
            hunk_header: None,
            base: None,
            before: vec!["x".into()],
            lines: vec!["y".into(), "z".into()],
            after: vec![],
        },
    };
    let thread = fx.service.create("u1", req.clone()).await.unwrap();
    assert!(thread.anchor.content_sha.is_none());
    assert_eq!(thread.anchor.lines, vec!["y", "z"]);
    let listed = fx
        .service
        .list(
            "u1",
            ListReviewThreadsQuery {
                scope_kind: Some("task_run".into()),
                scope_id: Some("run-1".into()),
                file_path: Some("a/b.rs".into()),
                status: Some("open".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].start_line, 3);

    let mut wrong = req;
    wrong.diff_context.lines = vec!["only one".into()];
    assert!(matches!(
        fx.service.create("u1", wrong).await,
        Err(ReviewError::BadRequest(_))
    ));
}

#[tokio::test]
async fn input_validation() {
    let fx = fixture().await;
    for path in ["../etc/passwd", "/abs/file", "C:/x", "a//b", "a/./b", ""] {
        let mut req = scm_request(&fx, 1, 1, &["line 1"]);
        req.file_path = path.into();
        assert!(
            matches!(fx.service.create("u1", req).await, Err(ReviewError::BadRequest(_))),
            "{path}"
        );
    }
    // Windows separators are normalized.
    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.file_path = "src\\lib.rs".into();
    assert_eq!(fx.service.create("u1", req).await.unwrap().file_path, FILE);

    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.scope_id = "/definitely/not/allowed".into();
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));

    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.body = "x".repeat(MAX_BODY_BYTES + 1);
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));

    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.body = "   ".into();
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));

    for (start, end) in [(0, 1), (5, 4)] {
        let req = scm_request(&fx, start, end, &[]);
        assert!(matches!(
            fx.service.create("u1", req).await,
            Err(ReviewError::BadRequest(_))
        ));
    }
    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.scope_kind = "bogus".into();
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));
    let mut req = scm_request(&fx, 1, 1, &["line 1"]);
    req.side = "both".into();
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));
}

#[tokio::test]
async fn binary_files_are_rejected() {
    let fx = fixture().await;
    std::fs::write(fx.repo.join("blob.bin"), b"abc\0def\n").unwrap();
    let mut req = scm_request(&fx, 1, 1, &[]);
    req.file_path = "blob.bin".into();
    assert!(matches!(
        fx.service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_escaping_the_repo_is_not_read() {
    let fx = fixture().await;
    let outside = fx._tmp.path().join("outside.txt");
    std::fs::write(&outside, "secret\n").unwrap();
    std::os::unix::fs::symlink(&outside, fx.repo.join("link.txt")).unwrap();
    // Not readable → falls back to the client context, which must match the range.
    let mut req = scm_request(&fx, 1, 1, &["whatever the diff showed"]);
    req.file_path = "link.txt".into();
    let thread = fx.service.create("u1", req).await.unwrap();
    assert!(thread.anchor.content_sha.is_none());
    assert_eq!(thread.anchor.lines, vec!["whatever the diff showed"]);
}

struct FakeProjects {
    owner: &'static str,
    pe_id: &'static str,
    dir: PathBuf,
}

#[async_trait::async_trait]
impl crate::resolver::ProjectDirResolver for FakeProjects {
    async fn resolve_dir(&self, user_id: &str, pe_id: &str, relative_path: &str) -> Option<PathBuf> {
        (user_id == self.owner && pe_id == self.pe_id).then(|| self.dir.join(relative_path))
    }
}

#[tokio::test]
async fn pe_scope_ids_resolve_through_the_users_project() {
    let fx = fixture().await;
    // The project folder is the parent of the repo, outside nothing in
    // particular: pe scopes do not need the allowed-roots sandbox.
    let projects = Arc::new(FakeProjects {
        owner: "u1",
        pe_id: "pe-1",
        dir: fx.repo.parent().unwrap().to_path_buf(),
    });
    let resolver = Arc::new(AllowedRootsResolver::new(Vec::new()).with_projects(projects));
    let service = ReviewService::new(fx.service.pool.clone(), resolver);

    let mut req = scm_request(&fx, 5, 5, &["line 5"]);
    req.scope_id = "pe:pe-1/repo/".into();
    let thread = service.create("u1", req.clone()).await.unwrap();
    assert_eq!(thread.scope_id, "pe:pe-1/repo");
    assert!(thread.anchor.content_sha.is_some(), "anchored from the real file");

    let mut lines = base_lines();
    lines.insert(0, "new top".into());
    write(&fx.repo, FILE, &lines);
    let listed = service
        .list(
            "u1",
            ListReviewThreadsQuery {
                scope_kind: Some("scm".into()),
                scope_id: Some("pe:pe-1/repo".into()),
                file_path: None,
                status: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].start_line, 6);

    // Another user cannot use u1's pe; traversal is rejected.
    assert!(matches!(
        service.create("u2", req.clone()).await,
        Err(ReviewError::BadRequest(_))
    ));
    req.scope_id = "pe:pe-1/../etc".into();
    assert!(matches!(
        service.create("u1", req).await,
        Err(ReviewError::BadRequest(_))
    ));
}
