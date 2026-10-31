//! BatchRunner / BatchService against the real SQLite repo and a fake
//! executor (no agents, worktrees or conversations).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_api_types::{BatchRetryRequest, CreateBatchJobRequest};
use aionui_batch::{
    AttemptOutcome, BatchError, BatchEvent, BatchRunnerConfig, BatchService, IBatchExecutor, ItemSpec, PreparedItem,
    TargetLocation,
};
use aionui_db::{IBatchRepository, SqliteBatchRepository, init_database_memory};
use serde_json::{Value, json};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Behave {
    /// Busy for a while, then writes the artifact.
    Pass,
    /// Busy for a while, writes nothing.
    NoArtifact,
    /// Busy until cancelled.
    Hang,
}

#[derive(Default)]
struct FakeState {
    behave: HashMap<i64, Behave>,
    /// conversation -> (seq, busy until, cancelled)
    convs: HashMap<String, (i64, Instant, bool)>,
    sessions: Vec<(String, AttemptOutcome)>,
    events: Vec<BatchEvent>,
    prompts: Vec<String>,
    cancels: usize,
    ended: std::collections::HashSet<String>,
    disk_checks: Vec<(usize, u32)>,
}

struct Fake {
    root: PathBuf,
    turn: Duration,
    state: Mutex<FakeState>,
    in_turn: AtomicUsize,
    max_in_turn: AtomicUsize,
    next: AtomicUsize,
    disk_ok: std::sync::atomic::AtomicBool,
}

impl Fake {
    fn new(root: &Path) -> Arc<Self> {
        Arc::new(Self {
            root: root.to_path_buf(),
            turn: Duration::from_millis(120),
            state: Mutex::new(FakeState::default()),
            in_turn: AtomicUsize::new(0),
            max_in_turn: AtomicUsize::new(0),
            next: AtomicUsize::new(0),
            disk_ok: std::sync::atomic::AtomicBool::new(true),
        })
    }
    fn set(&self, seq: i64, b: Behave) {
        self.state.lock().unwrap().behave.insert(seq, b);
    }
    fn workspace(&self, seq: i64) -> PathBuf {
        self.root.join(format!("w{seq}"))
    }
}

#[async_trait::async_trait]
impl IBatchExecutor for Fake {
    async fn check_assistant(&self, _user: &str, assistant_id: &str) -> Result<(), BatchError> {
        if assistant_id == "missing" {
            return Err(BatchError::Unavailable("assistant 'missing' not found".into()));
        }
        Ok(())
    }
    async fn check_target(&self, location: &TargetLocation) -> Result<(), BatchError> {
        if location.path().exists() {
            Ok(())
        } else {
            Err(BatchError::BadRequest(format!(
                "{} does not exist",
                location.path().display()
            )))
        }
    }
    async fn disk_precheck(&self, repos: &[PathBuf], concurrency: u32) -> Result<(), BatchError> {
        self.state.lock().unwrap().disk_checks.push((repos.len(), concurrency));
        if self.disk_ok.load(Ordering::SeqCst) {
            Ok(())
        } else {
            Err(BatchError::InsufficientDisk("need more space".into()))
        }
    }
    async fn create_session(&self, _user: &str, spec: &ItemSpec) -> Result<String, BatchError> {
        Ok(format!("ts_{}_{}", spec.seq, self.next.fetch_add(1, Ordering::SeqCst)))
    }
    async fn prepare(&self, _user: &str, ts: &str, spec: &ItemSpec) -> Result<PreparedItem, BatchError> {
        let conv = format!("conv_{ts}");
        self.state
            .lock()
            .unwrap()
            .convs
            .insert(conv.clone(), (spec.seq, Instant::now(), false));
        Ok(PreparedItem {
            conversation_id: conv,
            workspace: spec.location.path().to_path_buf(),
        })
    }
    async fn send_prompt(&self, _user: &str, conv: &str, prompt: &str) -> Result<(), BatchError> {
        let mut st = self.state.lock().unwrap();
        st.prompts.push(prompt.to_string());
        let entry = st.convs.get_mut(conv).unwrap();
        entry.1 = Instant::now() + self.turn;
        let n = self.in_turn.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_turn.fetch_max(n, Ordering::SeqCst);
        Ok(())
    }
    async fn is_busy(&self, conv: &str) -> bool {
        let mut st = self.state.lock().unwrap();
        let (seq, until, cancelled) = *st.convs.get(conv).unwrap();
        let behave = st.behave.get(&seq).copied().unwrap_or(Behave::Pass);
        let busy = !cancelled && (behave == Behave::Hang || Instant::now() < until);
        if !busy && st.ended.insert(conv.to_string()) {
            // Turn ended: produce the artifact once.
            if behave == Behave::Pass {
                std::fs::write(self.workspace(seq).join("result.json"), r#"{"ok":true}"#).unwrap();
            }
            self.in_turn.fetch_sub(1, Ordering::SeqCst);
        }
        busy
    }
    async fn cancel_turn(&self, _user: &str, conv: &str) {
        let mut st = self.state.lock().unwrap();
        st.cancels += 1;
        if let Some(c) = st.convs.get_mut(conv) {
            c.2 = true;
        }
        if st.ended.insert(conv.to_string()) {
            self.in_turn.fetch_sub(1, Ordering::SeqCst);
        }
    }
    async fn finish_session(&self, _user: &str, ts: &str, outcome: AttemptOutcome, _summary: &str) {
        self.state.lock().unwrap().sessions.push((ts.to_string(), outcome));
    }
    fn notify(&self, _user: &str, event: BatchEvent) {
        self.state.lock().unwrap().events.push(event);
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    fake: Arc<Fake>,
    svc: Arc<BatchService>,
    repo: Arc<dyn IBatchRepository>,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IBatchRepository> = Arc::new(SqliteBatchRepository::new(db.pool().clone()));
    std::mem::forget(db);
    let fake = Fake::new(dir.path());
    let cfg = BatchRunnerConfig {
        poll_interval: Duration::from_millis(10),
        start_grace: Duration::from_millis(500),
        timeout_unit: Duration::from_millis(5),
    };
    let svc = Arc::new(BatchService::new(repo.clone(), fake.clone(), cfg));
    Harness {
        _dir: dir,
        fake,
        svc,
        repo,
    }
}

fn request(h: &Harness, n: i64, extra: Value) -> CreateBatchJobRequest {
    serde_json::from_value(request_json(h, n, extra)).unwrap()
}

fn request_json(h: &Harness, n: i64, extra: Value) -> Value {
    let targets: Vec<Value> = (1..=n)
        .map(|i| {
            let ws = h.fake.workspace(i);
            std::fs::create_dir_all(&ws).unwrap();
            json!({"workspace": ws.to_str().unwrap(), "name": format!("t{i}")})
        })
        .collect();
    let mut req = json!({
        "title": "Release check",
        "template": {"prompt": "Check {{target.name}} (#{{seq}})"},
        "targets": targets,
        "checks": [
            {"kind": "file_exists", "path": "result.json"},
            {"kind": "json_schema", "path": "result.json",
             "schema": {"type": "object", "required": ["ok"], "properties": {"ok": {"const": true}}}}
        ],
        "assistant_id": "asst",
        "item_timeout_sec": 60
    });
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    req
}

async fn wait_idle(h: &Harness, job_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while h.svc.runner().is_driving(job_id).await {
        assert!(Instant::now() < deadline, "driver did not settle");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn trial_gate_then_scale_with_bounded_concurrency() {
    let h = harness().await;
    let job = h
        .svc
        .create("u1", request(&h, 6, json!({"concurrency": 2, "trial_size": 1})))
        .await
        .unwrap();
    assert_eq!((job.status.as_str(), job.counts.pending), ("draft", 6));
    assert!(!job.trial_settled);

    let err = h.svc.scale("u1", &job.id).await.unwrap_err();
    assert!(matches!(err, BatchError::InvalidState(_)), "no scale before the trial");

    let started = h.svc.start_trial("u1", &job.id).await.unwrap();
    assert_eq!(started.status, "trial");
    wait_idle(&h, &job.id).await;
    let after_trial = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!(after_trial.status, "trial", "a trial never scales itself");
    assert!(after_trial.trial_settled);
    assert_eq!((after_trial.counts.passed, after_trial.counts.pending), (1, 5));
    assert_eq!(h.fake.state.lock().unwrap().prompts, vec!["Check t1 (#1)".to_string()]);

    let scaled = h.svc.scale("u1", &job.id).await.unwrap();
    assert_eq!(scaled.status, "running");
    wait_idle(&h, &job.id).await;
    let done = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!((done.status.as_str(), done.counts.passed), ("completed", 6));
    assert_eq!(h.fake.max_in_turn.load(Ordering::SeqCst), 2, "concurrency bound");

    let items = h.svc.items("u1", &job.id).await.unwrap();
    assert!(items[0].trial && !items[1].trial);
    assert!(
        items
            .iter()
            .all(|i| i.attempt == 1 && i.check_results.len() == 2 && i.task_session_id.is_some())
    );
    let sessions = h.fake.state.lock().unwrap().sessions.clone();
    assert_eq!(sessions.len(), 6);
    assert!(sessions.iter().all(|(_, o)| *o == AttemptOutcome::Passed));
    assert!(h.fake.state.lock().unwrap().events.contains(&BatchEvent::JobUpdated {
        job_id: job.id.clone(),
        status: "completed".into()
    }));

    let csv = h.svc.export_csv("u1", &job.id).await.unwrap();
    assert_eq!(csv.lines().count(), 7);
    assert!(csv.lines().nth(1).unwrap().starts_with("1,passed,1,true,"));

    // Owner scope.
    assert!(matches!(h.svc.get("u2", &job.id).await, Err(BatchError::NotFound(_))));
}

#[tokio::test]
async fn failed_trial_blocks_scale_and_retry_reruns_it() {
    let h = harness().await;
    h.fake.set(1, Behave::NoArtifact);
    let job = h.svc.create("u1", request(&h, 3, json!({}))).await.unwrap();
    h.svc.start_trial("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    let items = h.svc.items("u1", &job.id).await.unwrap();
    assert_eq!(items[0].status, "failed");
    assert_eq!(items[0].error.as_deref(), Some("2 of 2 checks failed"));
    assert!(items[0].check_results.iter().all(|c| !c.ok));
    assert!(matches!(
        h.svc.scale("u1", &job.id).await,
        Err(BatchError::InvalidState(_))
    ));

    // Only trial items may be retried before scaling.
    let req = BatchRetryRequest {
        item_ids: Some(vec![items[1].id.clone()]),
        filter: None,
    };
    assert!(h.svc.retry("u1", &job.id, &req).await.is_err());

    h.fake.set(1, Behave::Pass);
    let job_after = h.svc.retry("u1", &job.id, &BatchRetryRequest::default()).await.unwrap();
    assert_eq!(job_after.status, "trial");
    wait_idle(&h, &job.id).await;
    let items = h.svc.items("u1", &job.id).await.unwrap();
    assert_eq!((items[0].status.as_str(), items[0].attempt), ("passed", 2));
    assert!(items[0].error.is_none());
    h.svc.scale("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    assert_eq!(h.svc.get("u1", &job.id).await.unwrap().status, "completed");
}

#[tokio::test]
async fn failing_items_fail_the_job_and_retry_completes_it() {
    let h = harness().await;
    h.fake.set(3, Behave::NoArtifact);
    let job = h.svc.create("u1", request(&h, 3, json!({}))).await.unwrap();
    h.svc.start_trial("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    h.svc.scale("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    let failed = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!(
        (failed.status.as_str(), failed.counts.failed, failed.counts.passed),
        ("failed", 1, 2)
    );

    h.fake.set(3, Behave::Pass);
    let rerun = h.svc.retry("u1", &job.id, &BatchRetryRequest::default()).await.unwrap();
    assert_eq!(rerun.status, "running");
    wait_idle(&h, &job.id).await;
    let done = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!((done.status.as_str(), done.counts.passed), ("completed", 3));
    assert!(matches!(
        h.svc.retry("u1", &job.id, &BatchRetryRequest::default()).await,
        Err(BatchError::InvalidState(_))
    ));
}

#[tokio::test]
async fn hung_item_times_out_and_stops_its_turn() {
    let h = harness().await;
    h.fake.set(1, Behave::Hang);
    let job = h.svc.create("u1", request(&h, 1, json!({}))).await.unwrap();
    h.svc.start_trial("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    let items = h.svc.items("u1", &job.id).await.unwrap();
    assert_eq!(items[0].status, "timeout");
    assert!(items[0].error.as_deref().unwrap().starts_with("no result within"));
    let st = h.fake.state.lock().unwrap();
    assert_eq!(st.cancels, 1);
    assert_eq!(st.sessions[0].1, AttemptOutcome::Timeout);
}

#[tokio::test]
async fn cancel_stops_in_flight_and_pending_items() {
    let h = harness().await;
    for seq in 1..=4 {
        h.fake.set(seq, Behave::Hang);
    }
    h.fake.set(1, Behave::Pass);
    let job = h
        .svc
        .create(
            "u1",
            request(&h, 4, json!({"concurrency": 2, "item_timeout_sec": 3600})),
        )
        .await
        .unwrap();
    h.svc.start_trial("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    h.svc.scale("u1", &job.id).await.unwrap();
    // Wait until both slots are busy.
    let deadline = Instant::now() + Duration::from_secs(10);
    while h.svc.get("u1", &job.id).await.unwrap().counts.running < 2 {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let cancelled = h.svc.cancel("u1", &job.id).await.unwrap();
    assert_eq!(cancelled.status, "cancelled");
    wait_idle(&h, &job.id).await;
    let done = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!(
        (done.status.as_str(), done.counts.passed, done.counts.cancelled),
        ("cancelled", 1, 3)
    );
    assert_eq!(h.fake.state.lock().unwrap().cancels, 2);
    assert!(matches!(
        h.svc.cancel("u1", &job.id).await,
        Err(BatchError::InvalidState(_))
    ));
}

#[tokio::test]
async fn create_validates_assistant_targets_and_disk() {
    let h = harness().await;
    assert!(matches!(
        h.svc
            .create("u1", request(&h, 1, json!({"assistant_id": "missing"})))
            .await,
        Err(BatchError::Unavailable(_))
    ));
    let mut req = request(&h, 1, json!({}));
    req.targets.push(json!({"workspace": "/definitely/not/here"}));
    assert!(matches!(h.svc.create("u1", req).await, Err(BatchError::BadRequest(_))));
    let mut req = request(&h, 1, json!({}));
    req.targets.push(req.targets[0].clone());
    assert!(
        matches!(h.svc.create("u1", req).await, Err(BatchError::BadRequest(_))),
        "shared workspace"
    );

    // Repo targets go through the disk precheck before anything runs.
    let repo = h.fake.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut req = request(&h, 0, json!({}));
    req.targets = vec![
        json!({"repo": repo.to_str().unwrap(), "name": "a"}),
        json!({"repo": repo.to_str().unwrap(), "name": "b"}),
    ];
    let job = h.svc.create("u1", req).await.unwrap();
    h.fake.disk_ok.store(false, Ordering::SeqCst);
    assert!(matches!(
        h.svc.start_trial("u1", &job.id).await,
        Err(BatchError::InsufficientDisk(_))
    ));
    assert_eq!(h.svc.get("u1", &job.id).await.unwrap().status, "draft");
    assert_eq!(h.fake.state.lock().unwrap().disk_checks.last(), Some(&(1, 1)));
}

#[tokio::test]
async fn boot_recovery_pauses_running_jobs_and_scale_resumes() {
    let h = harness().await;
    let job = h.svc.create("u1", request(&h, 2, json!({}))).await.unwrap();
    // Simulate a crash mid-run: job running, item 2 running.
    h.repo
        .transition_job_status("u1", &job.id, &["draft"], "running", 1)
        .await
        .unwrap();
    let items = h.svc.items("u1", &job.id).await.unwrap();
    h.repo.start_item("u1", &items[1].id, "ts_old", 1).await.unwrap();

    assert_eq!(h.svc.recover_on_boot().await.unwrap(), (1, 1));
    let paused = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!((paused.status.as_str(), paused.counts.failed), ("paused", 1));

    h.svc.scale("u1", &job.id).await.unwrap();
    wait_idle(&h, &job.id).await;
    let resumed = h.svc.get("u1", &job.id).await.unwrap();
    assert_eq!(
        (resumed.status.as_str(), resumed.counts.passed),
        ("failed", 1),
        "interrupted item needs a retry"
    );
    h.svc.retry("u1", &job.id, &BatchRetryRequest::default()).await.unwrap();
    wait_idle(&h, &job.id).await;
    assert_eq!(h.svc.get("u1", &job.id).await.unwrap().status, "completed");
}

// ---- HTTP routes -------------------------------------------------------

fn app(h: &Harness, user: &str) -> axum::Router {
    aionui_batch::batch_routes(aionui_batch::BatchRouterState { service: h.svc.clone() }).layer(axum::Extension(
        aionui_auth::CurrentUser {
            id: user.to_owned(),
            username: user.to_owned(),
            user_type: aionui_db::UserType::Local,
            status: aionui_db::UserStatus::Active,
        },
    ))
}

async fn call(router: axum::Router, method: &str, uri: &str, body: Option<Value>) -> (u16, String, Value) {
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let mut req = axum::http::Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            axum::body::Body::from(v.to_string())
        }
        None => axum::body::Body::empty(),
    };
    let resp = router.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    let json = serde_json::from_str(&text).unwrap_or(Value::String(text));
    (status, ctype, json)
}

#[tokio::test]
async fn http_routes_drive_the_full_lifecycle() {
    let h = harness().await;
    let req = request_json(&h, 3, json!({"concurrency": 2}));
    let (st, _, body) = call(app(&h, "u1"), "POST", "/api/batch", Some(req)).await;
    assert_eq!(st, 201, "{body}");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["status"], "draft");

    let (st, _, body) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/scale"), None).await;
    assert_eq!(st, 409, "scale before the trial: {body}");

    let (st, _, body) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/trial"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("trial")));
    wait_idle(&h, &id).await;
    let (st, _, body) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/scale"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("running")));
    wait_idle(&h, &id).await;

    let (st, _, body) = call(app(&h, "u1"), "GET", &format!("/api/batch/{id}"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("completed")));
    let (st, _, body) = call(app(&h, "u1"), "GET", &format!("/api/batch/{id}/items"), None).await;
    assert_eq!((st, body["data"].as_array().unwrap().len()), (200, 3));
    let (st, _, body) = call(app(&h, "u1"), "GET", "/api/batch?limit=10", None).await;
    assert_eq!((st, body["data"].as_array().unwrap().len()), (200, 1));

    let (st, ctype, body) = call(app(&h, "u1"), "GET", &format!("/api/batch/{id}/export.csv"), None).await;
    assert_eq!(st, 200);
    assert!(ctype.starts_with("text/csv"), "{ctype}");
    assert_eq!(body.as_str().unwrap().lines().count(), 4);

    // Nothing failed: retry with the default filter has nothing to do.
    let (st, _, _) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/retry"), None).await;
    assert_eq!(st, 409);
    // Terminal: cancel is refused.
    let (st, _, _) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/cancel"), None).await;
    assert_eq!(st, 409);

    // Other users see nothing.
    let (st, _, _) = call(app(&h, "u2"), "GET", &format!("/api/batch/{id}"), None).await;
    assert_eq!(st, 404);
    let (st, _, body) = call(app(&h, "u2"), "GET", "/api/batch", None).await;
    assert_eq!((st, body["data"].as_array().unwrap().len()), (200, 0));
}

#[tokio::test]
async fn http_routes_map_validation_and_disk_errors() {
    let h = harness().await;
    let (st, _, _) = call(app(&h, "u1"), "POST", "/api/batch", Some(json!({"title": "x"}))).await;
    assert_eq!(st, 400, "malformed body");
    let mut req = request_json(&h, 1, json!({}));
    req["targets"] = json!([]);
    let (st, _, body) = call(app(&h, "u1"), "POST", "/api/batch", Some(req)).await;
    assert_eq!(st, 400, "{body}");

    // The disk precheck covers repo targets (each gets a worktree).
    let repo = h.fake.root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let mut req = request_json(&h, 0, json!({}));
    req["targets"] = json!([{"repo": repo.to_str().unwrap(), "name": "a"}]);
    let (st, _, body) = call(app(&h, "u1"), "POST", "/api/batch", Some(req)).await;
    assert_eq!(st, 201, "{body}");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    h.fake.disk_ok.store(false, Ordering::SeqCst);
    let (st, _, body) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/trial"), None).await;
    assert_eq!(st, 507, "{body}");
    h.fake.disk_ok.store(true, Ordering::SeqCst);

    let (st, _, body) = call(app(&h, "u1"), "POST", &format!("/api/batch/{id}/cancel"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("cancelled")));
    let (st, _, _) = call(app(&h, "u1"), "GET", "/api/batch/bj_missing/items", None).await;
    assert_eq!(st, 404);
}
