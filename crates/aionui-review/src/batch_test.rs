use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::*;
use crate::resolver::AllowedRootsResolver;
use crate::types::{CreateReviewThreadRequest, DiffContext};

const FILE: &str = "src/lib.rs";

/// Records submissions; `running` drives `steer_available`, `reject_steer`
/// simulates a turn that ended before the steer landed.
#[derive(Default)]
struct FakeSink {
    running: bool,
    reject_steer: bool,
    fail: bool,
    sent: Mutex<Vec<(String, ReviewDeliveryMode, String, String)>>,
}

#[async_trait]
impl ReviewInputSink for FakeSink {
    async fn steer_available(&self, user_id: &str, conversation_id: &str) -> Result<bool, ReviewInputError> {
        if user_id != "u1" || conversation_id != "conv-u1" {
            return Err(ReviewInputError::NotFound);
        }
        Ok(self.running)
    }

    async fn submit(
        &self,
        _user_id: &str,
        conversation_id: &str,
        mode: ReviewDeliveryMode,
        content: &str,
        client_key: &str,
    ) -> Result<ReviewInputAccepted, ReviewInputError> {
        if self.fail {
            return Err(ReviewInputError::Rejected("agent offline".into()));
        }
        if mode == ReviewDeliveryMode::Steer && self.reject_steer {
            return Err(ReviewInputError::Unsupported);
        }
        self.sent.lock().unwrap().push((
            conversation_id.to_owned(),
            mode,
            content.to_owned(),
            client_key.to_owned(),
        ));
        Ok(ReviewInputAccepted {
            input_id: format!("input-{client_key}"),
            receipt: serde_json::json!({ "input": { "input_id": format!("input-{client_key}") } }),
        })
    }
}

struct Fixture {
    service: ReviewService,
    sink: Arc<FakeSink>,
    _db: aionui_db::Database,
    _tmp: tempfile::TempDir,
    repo: PathBuf,
    scope_id: String,
}

fn write(repo: &Path, lines: &[String]) {
    let path = repo.join(FILE);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

fn base_lines() -> Vec<String> {
    (1..=30).map(|i| format!("line {i}")).collect()
}

async fn fixture(sink: FakeSink) -> Fixture {
    let db = aionui_db::init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES ('u1', 'u1', 'x', 0, 0)",
    )
    .execute(db.pool())
    .await
    .unwrap();
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
    write(&repo, &base_lines());
    let sink = Arc::new(sink);
    let service = ReviewService::new(
        db.pool().clone(),
        Arc::new(AllowedRootsResolver::new(vec![tmp.path().to_path_buf()])),
    )
    .with_input_sink(sink.clone());
    let scope_id = repo.to_string_lossy().into_owned();
    Fixture {
        service,
        sink,
        _db: db,
        _tmp: tmp,
        repo,
        scope_id,
    }
}

async fn annotate(fx: &Fixture, line: u32, body: &str) -> ReviewThread {
    fx.service
        .create(
            "u1",
            CreateReviewThreadRequest {
                scope_kind: "scm".into(),
                scope_id: fx.scope_id.clone(),
                conversation_id: Some("conv-u1".into()),
                task_session_id: None,
                run_id: None,
                file_path: FILE.into(),
                side: "new".into(),
                start_line: line,
                end_line: line,
                body: body.into(),
                diff_context: DiffContext {
                    hunk_header: None,
                    base: None,
                    before: vec![],
                    lines: vec![format!("line {line}")],
                    after: vec![],
                },
            },
        )
        .await
        .unwrap()
}

fn scope_request(fx: &Fixture, delivery: Option<&str>) -> SendReviewBatchRequest {
    SendReviewBatchRequest {
        target_conversation_id: "conv-u1".into(),
        delivery: delivery.map(str::to_owned),
        scope_kind: Some("scm".into()),
        scope_id: Some(fx.scope_id.clone()),
        ..Default::default()
    }
}

async fn threads(fx: &Fixture) -> Vec<ReviewThread> {
    fx.service
        .list(
            "u1",
            ListReviewThreadsQuery {
                scope_kind: Some("scm".into()),
                scope_id: Some(fx.scope_id.clone()),
                file_path: None,
                status: None,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn five_annotations_go_out_in_one_batch_and_stay_anchored_after_the_agent_edits() {
    let fx = fixture(FakeSink::default()).await;
    for (line, body) in [(5, "a"), (9, "b"), (12, "c"), (20, "d"), (25, "e")] {
        annotate(&fx, line, body).await;
    }

    let sent = fx.service.send_batch("u1", scope_request(&fx, None)).await.unwrap();
    assert_eq!(sent.batch.status, "accepted");
    assert_eq!(sent.batch.delivery, "followup");
    assert_eq!(sent.batch.thread_ids.len(), 5);
    assert_eq!(
        sent.batch.input_id.as_deref(),
        Some(format!("input-{}", sent.batch.id).as_str())
    );
    {
        let log = fx.sink.sent.lock().unwrap();
        assert_eq!(log.len(), 1);
        let (_, mode, content, client_key) = &log[0];
        assert_eq!(*mode, ReviewDeliveryMode::Followup);
        assert_eq!(client_key, &sent.batch.id);
        assert_eq!(content, &sent.batch.content);
        assert!(content.starts_with(&format!("[[AION_REVIEW batch={} count=5]]", sent.batch.id)));
        for (n, line) in [(1, 5), (2, 9), (3, 12), (4, 20), (5, 25)] {
            assert!(content.contains(&format!("#{n} {FILE}  L{line}（新版本）\n```rust\nline {line}\n```")));
        }
    }
    assert!(threads(&fx).await.iter().all(|thread| {
        thread.status == ReviewThreadStatus::Sent && thread.last_batch_id.as_deref() == Some(sent.batch.id.as_str())
    }));

    // The agent inserts three lines above every annotation.
    let mut edited: Vec<String> = vec!["new a".into(), "new b".into(), "new c".into()];
    edited.extend(base_lines());
    write(&fx.repo, &edited);
    let moved: Vec<(u32, String)> = threads(&fx)
        .await
        .into_iter()
        .map(|thread| (thread.start_line, thread.anchor.lines[0].clone()))
        .collect();
    assert_eq!(
        moved,
        [5, 9, 12, 20, 25]
            .map(|line| (line + 3, format!("line {line}")))
            .to_vec()
    );
}

#[tokio::test]
async fn auto_steers_into_a_running_turn() {
    let fx = fixture(FakeSink {
        running: true,
        ..Default::default()
    })
    .await;
    annotate(&fx, 3, "x").await;
    let sent = fx.service.send_batch("u1", scope_request(&fx, None)).await.unwrap();
    assert_eq!(sent.batch.delivery, "steer");
    assert_eq!(fx.sink.sent.lock().unwrap()[0].1, ReviewDeliveryMode::Steer);
}

#[tokio::test]
async fn followup_is_kept_even_when_running() {
    let fx = fixture(FakeSink {
        running: true,
        ..Default::default()
    })
    .await;
    annotate(&fx, 3, "x").await;
    let sent = fx
        .service
        .send_batch("u1", scope_request(&fx, Some("followup")))
        .await
        .unwrap();
    assert_eq!(sent.batch.delivery, "followup");
}

#[tokio::test]
async fn steer_degrades_to_followup_when_idle_or_rejected() {
    let idle = fixture(FakeSink::default()).await;
    annotate(&idle, 3, "x").await;
    let sent = idle
        .service
        .send_batch("u1", scope_request(&idle, Some("steer")))
        .await
        .unwrap();
    assert_eq!(sent.batch.delivery, "followup");

    let ended = fixture(FakeSink {
        running: true,
        reject_steer: true,
        ..Default::default()
    })
    .await;
    annotate(&ended, 3, "x").await;
    let sent = ended
        .service
        .send_batch("u1", scope_request(&ended, Some("steer")))
        .await
        .unwrap();
    assert_eq!(sent.batch.delivery, "followup");
    assert_eq!(ended.sink.sent.lock().unwrap()[0].1, ReviewDeliveryMode::Followup);
}

#[tokio::test]
async fn resolved_threads_stay_out_and_explicit_ids_are_honoured() {
    let fx = fixture(FakeSink::default()).await;
    let a = annotate(&fx, 2, "a").await;
    let b = annotate(&fx, 4, "b").await;
    let c = annotate(&fx, 6, "c").await;
    fx.service.resolve("u1", &b.id).await.unwrap();

    let sent = fx.service.send_batch("u1", scope_request(&fx, None)).await.unwrap();
    assert_eq!(sent.batch.thread_ids, vec![a.id.clone(), c.id.clone()]);

    let only_c = SendReviewBatchRequest {
        target_conversation_id: "conv-u1".into(),
        thread_ids: vec![c.id.clone(), b.id.clone()],
        ..Default::default()
    };
    let sent = fx.service.send_batch("u1", only_c).await.unwrap();
    assert_eq!(sent.batch.thread_ids, vec![c.id.clone()]);
    // Unresolved threads ride along again in the next batch.
    let history = fx.service.list_batches("u1", "conv-u1").await.unwrap();
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].id, sent.batch.id);
}

#[tokio::test]
async fn failed_delivery_is_recorded_and_threads_stay_open() {
    let fx = fixture(FakeSink {
        fail: true,
        ..Default::default()
    })
    .await;
    annotate(&fx, 3, "x").await;
    let error = fx.service.send_batch("u1", scope_request(&fx, None)).await.unwrap_err();
    assert!(matches!(
        error,
        ReviewError::Conflict {
            code: "REVIEW_DELIVERY_REJECTED",
            ..
        }
    ));
    let history = fx.service.list_batches("u1", "conv-u1").await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, "failed");
    assert_eq!(history[0].error_message.as_deref(), Some("agent offline"));
    assert_eq!(threads(&fx).await[0].status, ReviewThreadStatus::Open);
}

#[tokio::test]
async fn foreign_targets_and_empty_batches_are_rejected_before_storing() {
    let fx = fixture(FakeSink::default()).await;
    let mut other = scope_request(&fx, None);
    other.target_conversation_id = "conv-u2".into();
    annotate(&fx, 3, "x").await;
    assert!(matches!(
        fx.service.send_batch("u1", other).await,
        Err(ReviewError::BadRequest(_))
    ));
    assert!(matches!(
        fx.service.send_batch("u1", scope_request(&fx, Some("later"))).await,
        Err(ReviewError::BadRequest(_))
    ));
    for thread in threads(&fx).await {
        fx.service.resolve("u1", &thread.id).await.unwrap();
    }
    assert!(matches!(
        fx.service.send_batch("u1", scope_request(&fx, None)).await,
        Err(ReviewError::BadRequest(_))
    ));
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM review_batches")
        .fetch_one(&fx.service.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
