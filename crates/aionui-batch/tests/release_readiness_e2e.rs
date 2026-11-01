//! M8 E8-8: the "release readiness check" batch over 20 fixture git
//! repositories, end to end through the HTTP routes.
//!
//! Fixture repos are generated locally with the git CLI (no network). Each
//! attempt gets its own `git worktree` (as the production adapter does with
//! managed worktrees), and a scripted stand-in agent runs a real readiness
//! check inside it and writes `release-check.json`. The batch judges every
//! item only by that artifact:
//!
//! - trial 1 → confirm → scale the other 19 at concurrency 4;
//! - scale before the trial is refused (409);
//! - an agent that claims it finished but wrote nothing → failed;
//! - an artifact that breaks the schema → failed;
//! - a hung agent → cancelled and marked timeout;
//! - a repo that is not ready still passes (the artifact says `ready:false`);
//! - single-item retry: a new TaskSession and worktree, the old ones kept,
//!   and the `batch:{id}` summary refreshed with the new counts.
//!
//! What this does not cover: a real agent CLI and the app's
//! `BatchExecutorAdapter` (conversation/worktree services); those need an
//! agent binary and are exercised by the adapter's own tests.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_batch::{
    AttemptOutcome, BatchError, BatchEvent, BatchRunnerConfig, BatchService, BatchSummary, IBatchExecutor, ItemSpec,
    PreparedItem, TargetLocation,
};
use aionui_db::{IBatchRepository, SqliteBatchRepository, init_database_memory};
use serde_json::{Value, json};

const REPOS: i64 = 20;
const CONCURRENCY: usize = 4;
const ARTIFACT: &str = "release-check.json";

/// What the stand-in agent does for one repo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Agent {
    /// Runs the readiness check and writes the artifact.
    Check,
    /// Says it is done but writes nothing.
    ClaimsDone,
    /// Writes an artifact that breaks the schema.
    BadArtifact,
    /// Never finishes its turn.
    Hang,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=E8-8", "-c", "user.email=e8-8@example.invalid"])
        .args(["-c", "commit.gpgsign=false", "-c", "init.defaultBranch=main"])
        .args(args)
        .output()
        .expect("git CLI");
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `repo-NN`: VERSION + CHANGELOG + a tiny crate. `ready = false` leaves the
/// version out of the changelog.
fn fixture_repo(root: &Path, n: i64, ready: bool) -> PathBuf {
    let dir = root.join(format!("repo-{n:02}"));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    let version = format!("1.{n}.0");
    std::fs::write(dir.join("VERSION"), format!("{version}\n")).unwrap();
    let entry = if ready {
        format!("## {version}\n- Release notes for repo-{n:02}\n\n")
    } else {
        String::new()
    };
    std::fs::write(
        dir.join("CHANGELOG.md"),
        format!("# Changelog\n\n{entry}## 1.0.0\n- First release\n"),
    )
    .unwrap();
    std::fs::write(dir.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n").unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "fixture"]);
    dir
}

/// The readiness check the stand-in agent performs inside its worktree.
fn readiness_report(worktree: &Path, repo_name: &str) -> Value {
    let version = std::fs::read_to_string(worktree.join("VERSION"))
        .unwrap_or_default()
        .trim()
        .to_owned();
    let changelog = std::fs::read_to_string(worktree.join("CHANGELOG.md")).unwrap_or_default();
    let status = Command::new("git")
        .arg("-C")
        .arg(worktree)
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .unwrap();
    let checks = vec![
        json!({"name": "version_file", "ok": !version.is_empty()}),
        json!({"name": "changelog_entry", "ok": changelog.contains(&format!("## {version}"))}),
        json!({"name": "clean_tree", "ok": status.status.success() && status.stdout.is_empty()}),
    ];
    let ready = checks.iter().all(|c| c["ok"] == true);
    json!({"repo": repo_name, "version": version, "ready": ready, "checks": checks})
}

#[derive(Default)]
struct World {
    agent: HashMap<i64, Agent>,
    /// conversation → (seq, worktree, repo name)
    convs: HashMap<String, (i64, PathBuf, String)>,
    /// conversations whose turn has ended
    done: HashSet<String>,
    started: HashSet<String>,
    cancelled: Vec<String>,
    /// task session → (seq, worktree, outcome)
    sessions: HashMap<String, (i64, PathBuf, Option<AttemptOutcome>)>,
    summaries: Vec<BatchSummary>,
    disk_checks: Vec<(usize, u32)>,
}

struct Executor {
    root: PathBuf,
    world: Arc<Mutex<World>>,
    in_turn: Arc<AtomicUsize>,
    max_in_turn: Arc<AtomicUsize>,
    next: AtomicUsize,
}

impl Executor {
    fn set(&self, seq: i64, agent: Agent) {
        self.world.lock().unwrap().agent.insert(seq, agent);
    }
}

#[async_trait::async_trait]
impl IBatchExecutor for Executor {
    async fn check_assistant(&self, _user: &str, _assistant_id: &str) -> Result<(), BatchError> {
        Ok(())
    }

    async fn check_target(&self, location: &TargetLocation) -> Result<(), BatchError> {
        match location {
            TargetLocation::Repo(p) if p.join(".git").exists() => Ok(()),
            other => Err(BatchError::BadRequest(format!(
                "{} is not a git repository",
                other.path().display()
            ))),
        }
    }

    async fn disk_precheck(&self, repos: &[PathBuf], concurrency: u32) -> Result<(), BatchError> {
        self.world.lock().unwrap().disk_checks.push((repos.len(), concurrency));
        Ok(())
    }

    async fn create_session(&self, _user: &str, spec: &ItemSpec) -> Result<String, BatchError> {
        Ok(format!(
            "ts_{:02}_{}",
            spec.seq,
            self.next.fetch_add(1, Ordering::SeqCst)
        ))
    }

    async fn prepare(&self, _user: &str, ts: &str, spec: &ItemSpec) -> Result<PreparedItem, BatchError> {
        let TargetLocation::Repo(repo) = &spec.location else {
            return Err(BatchError::BadRequest("repo targets only".into()));
        };
        // One worktree per attempt, keyed by the TaskSession like production.
        let worktree = self.root.join("worktrees").join(ts);
        let branch = format!("batch/{ts}");
        let (repo, wt) = (repo.clone(), worktree.clone());
        tokio::task::spawn_blocking(move || {
            git(&repo, &["worktree", "add", "-q", "-b", &branch, wt.to_str().unwrap()])
        })
        .await
        .unwrap();
        let conv = format!("conv_{ts}");
        let name = spec.location.path().file_name().unwrap().to_string_lossy().into_owned();
        let mut w = self.world.lock().unwrap();
        w.convs.insert(conv.clone(), (spec.seq, worktree.clone(), name));
        w.sessions.insert(ts.to_owned(), (spec.seq, worktree.clone(), None));
        Ok(PreparedItem {
            conversation_id: conv,
            workspace: worktree,
        })
    }

    async fn send_prompt(&self, _user: &str, conv: &str, prompt: &str) -> Result<(), BatchError> {
        let (seq, worktree, name, agent) = {
            let mut w = self.world.lock().unwrap();
            let (seq, wt, name) = w.convs.get(conv).cloned().unwrap();
            assert!(prompt.contains(&name), "prompt names its target: {prompt}");
            w.started.insert(conv.to_owned());
            let agent = w.agent.get(&seq).copied().unwrap_or(Agent::Check);
            (seq, wt, name, agent)
        };
        let n = self.in_turn.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_turn.fetch_max(n, Ordering::SeqCst);
        if agent == Agent::Hang {
            return Ok(());
        }
        let world = self.world.clone();
        let in_turn = self.in_turn.clone();
        let conv = conv.to_owned();
        tokio::spawn(async move {
            // Long enough that the 4 slots overlap.
            tokio::time::sleep(Duration::from_millis(60)).await;
            let report = tokio::task::spawn_blocking({
                let wt = worktree.clone();
                move || readiness_report(&wt, &name)
            })
            .await
            .unwrap();
            match agent {
                Agent::Check => {
                    std::fs::write(worktree.join(ARTIFACT), serde_json::to_vec_pretty(&report).unwrap()).unwrap()
                }
                Agent::BadArtifact => std::fs::write(
                    worktree.join(ARTIFACT),
                    json!({"repo": format!("repo-{seq:02}"), "version": "next", "ready": "yes"}).to_string(),
                )
                .unwrap(),
                Agent::ClaimsDone | Agent::Hang => {}
            }
            if world.lock().unwrap().done.insert(conv) {
                in_turn.fetch_sub(1, Ordering::SeqCst);
            }
        });
        Ok(())
    }

    async fn is_busy(&self, conv: &str) -> bool {
        let w = self.world.lock().unwrap();
        w.started.contains(conv) && !w.done.contains(conv)
    }

    async fn cancel_turn(&self, _user: &str, conv: &str) {
        let mut w = self.world.lock().unwrap();
        w.cancelled.push(conv.to_owned());
        if w.done.insert(conv.to_owned()) {
            self.in_turn.fetch_sub(1, Ordering::SeqCst);
        }
    }

    async fn finish_session(&self, _user: &str, ts: &str, outcome: AttemptOutcome, _summary: &str) {
        if let Some(s) = self.world.lock().unwrap().sessions.get_mut(ts) {
            s.2 = Some(outcome);
        }
    }

    fn notify(&self, _user: &str, _event: BatchEvent) {}

    async fn job_finished(&self, _user: &str, summary: &BatchSummary) {
        self.world.lock().unwrap().summaries.push(summary.clone());
    }
}

struct Harness {
    _dir: tempfile::TempDir,
    exec: Arc<Executor>,
    svc: Arc<BatchService>,
    repos: Vec<PathBuf>,
}

async fn harness() -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let fixtures = dir.path().join("fixtures");
    // repo-18 is not release-ready (its version is missing from the changelog).
    let repos: Vec<PathBuf> = (1..=REPOS).map(|n| fixture_repo(&fixtures, n, n != 18)).collect();
    let db = init_database_memory().await.unwrap();
    let repo: Arc<dyn IBatchRepository> = Arc::new(SqliteBatchRepository::new(db.pool().clone()));
    std::mem::forget(db);
    let exec = Arc::new(Executor {
        root: dir.path().to_path_buf(),
        world: Arc::new(Mutex::new(World::default())),
        in_turn: Arc::new(AtomicUsize::new(0)),
        max_in_turn: Arc::new(AtomicUsize::new(0)),
        next: AtomicUsize::new(0),
    });
    let cfg = BatchRunnerConfig {
        poll_interval: Duration::from_millis(10),
        start_grace: Duration::from_millis(500),
        // item_timeout_sec 60 → 1.2 s of wall clock.
        timeout_unit: Duration::from_millis(20),
    };
    let svc = Arc::new(BatchService::new(repo, exec.clone(), cfg));
    Harness {
        _dir: dir,
        exec,
        svc,
        repos,
    }
}

fn app(h: &Harness) -> axum::Router {
    aionui_batch::batch_routes(aionui_batch::BatchRouterState { service: h.svc.clone() }).layer(axum::Extension(
        aionui_auth::CurrentUser {
            id: "u1".to_owned(),
            username: "u1".to_owned(),
            user_type: aionui_db::UserType::Local,
            status: aionui_db::UserStatus::Active,
        },
    ))
}

async fn call(h: &Harness, method: &str, uri: &str, body: Option<Value>) -> (u16, Value) {
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
    let resp = app(h).oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

async fn wait_idle(h: &Harness, job_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while h.svc.runner().is_driving(job_id).await {
        assert!(Instant::now() < deadline, "driver did not settle");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn items(h: &Harness, id: &str) -> Vec<Value> {
    let (st, body) = call(h, "GET", &format!("/api/batch/{id}/items"), None).await;
    assert_eq!(st, 200, "{body}");
    body["data"].as_array().unwrap().clone()
}

fn by_seq(items: &[Value], seq: i64) -> &Value {
    items.iter().find(|i| i["seq"] == seq).unwrap()
}

fn release_check_schema() -> Value {
    json!({
        "type": "object",
        "required": ["repo", "version", "ready", "checks"],
        "properties": {
            "repo": {"type": "string", "pattern": "^repo-[0-9]{2}$"},
            "version": {"type": "string", "pattern": "^[0-9]+\\.[0-9]+\\.[0-9]+$"},
            "ready": {"type": "boolean"},
            "checks": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["name", "ok"],
                    "properties": {"name": {"type": "string"}, "ok": {"type": "boolean"}}
                }
            }
        }
    })
}

#[tokio::test]
async fn release_readiness_batch_over_twenty_repos_with_single_item_retry() {
    let h = harness().await;
    h.exec.set(7, Agent::ClaimsDone);
    h.exec.set(12, Agent::BadArtifact);
    h.exec.set(15, Agent::Hang);

    let targets: Vec<Value> = h
        .repos
        .iter()
        .map(|r| json!({"repo": r.to_str().unwrap(), "name": r.file_name().unwrap().to_str().unwrap()}))
        .collect();
    let (st, body) = call(
        &h,
        "POST",
        "/api/batch",
        Some(json!({
            "title": "Release readiness",
            "template": {"prompt": "Run the release readiness check for {{target.name}} (#{{seq}}) and write release-check.json"},
            "targets": targets,
            "checks": [
                {"kind": "file_exists", "path": ARTIFACT},
                {"kind": "json_schema", "path": ARTIFACT, "schema": release_check_schema()}
            ],
            "assistant_id": "release-checker",
            "concurrency": CONCURRENCY,
            "trial_size": 1,
            "item_timeout_sec": 60
        })),
    )
    .await;
    assert_eq!(st, 201, "{body}");
    let id = body["data"]["id"].as_str().unwrap().to_owned();
    assert_eq!(body["data"]["counts"]["total"], REPOS);

    // No scale before the trial.
    let (st, body) = call(&h, "POST", &format!("/api/batch/{id}/scale"), None).await;
    assert_eq!(st, 409, "{body}");

    // Trial: exactly one repo runs, and the job waits for confirmation.
    let (st, body) = call(&h, "POST", &format!("/api/batch/{id}/trial"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("trial")), "{body}");
    wait_idle(&h, &id).await;
    let (_, job) = call(&h, "GET", &format!("/api/batch/{id}"), None).await;
    assert_eq!(job["data"]["status"], "trial", "a trial never scales itself");
    assert_eq!(job["data"]["trial_settled"], true);
    assert_eq!(job["data"]["counts"]["passed"], 1);
    assert_eq!(job["data"]["counts"]["pending"], REPOS - 1);
    assert_eq!(h.exec.world.lock().unwrap().sessions.len(), 1);
    assert!(
        h.exec.world.lock().unwrap().summaries.is_empty(),
        "no summary for a trial"
    );

    // Confirm → scale the remaining 19 at concurrency 4.
    let (st, body) = call(&h, "POST", &format!("/api/batch/{id}/scale"), None).await;
    assert_eq!((st, body["data"]["status"].as_str()), (200, Some("running")), "{body}");
    wait_idle(&h, &id).await;
    assert_eq!(
        h.exec.max_in_turn.load(Ordering::SeqCst),
        CONCURRENCY,
        "concurrency bound"
    );
    assert!(
        h.exec
            .world
            .lock()
            .unwrap()
            .disk_checks
            .iter()
            .any(|(n, c)| *n == (REPOS - 1) as usize && *c == CONCURRENCY as u32),
        "scale prechecks disk for the remaining repos"
    );

    let (_, job) = call(&h, "GET", &format!("/api/batch/{id}"), None).await;
    let counts = &job["data"]["counts"];
    assert_eq!(job["data"]["status"], "failed", "{job}");
    assert_eq!(
        (
            counts["passed"].as_i64(),
            counts["failed"].as_i64(),
            counts["timeout"].as_i64()
        ),
        (Some(17), Some(2), Some(1)),
        "{job}"
    );

    let list = items(&h, &id).await;
    // Judged only by the artifact: "done" without release-check.json fails.
    let claims = by_seq(&list, 7);
    assert_eq!(claims["status"], "failed");
    assert_eq!(claims["error"], "2 of 2 checks failed");
    // A schema-breaking artifact fails the schema check only.
    let bad = by_seq(&list, 12);
    assert_eq!(bad["status"], "failed");
    assert_eq!(bad["error"], "1 of 2 checks failed");
    // The hung agent was stopped and marked timeout.
    let hung = by_seq(&list, 15);
    assert_eq!(hung["status"], "timeout");
    assert!(
        hung["error"].as_str().unwrap().starts_with("no result within"),
        "{hung}"
    );
    {
        let w = h.exec.world.lock().unwrap();
        assert_eq!(w.cancelled.len(), 1, "only the hung turn is cancelled");
        assert!(w.cancelled[0].starts_with("conv_ts_15_"));
    }
    // Not ready is a valid result: the artifact says so and the item passes.
    let not_ready = by_seq(&list, 18);
    assert_eq!(not_ready["status"], "passed");
    let ts18 = not_ready["task_session_id"].as_str().unwrap();
    let wt18 = h.exec.world.lock().unwrap().sessions[ts18].1.clone();
    let report: Value = serde_json::from_slice(&std::fs::read(wt18.join(ARTIFACT)).unwrap()).unwrap();
    assert_eq!(
        (report["repo"].as_str(), report["ready"].as_bool()),
        (Some("repo-18"), Some(false))
    );
    let ok = by_seq(&list, 3);
    let ts3 = ok["task_session_id"].as_str().unwrap();
    let wt3 = h.exec.world.lock().unwrap().sessions[ts3].1.clone();
    let report: Value = serde_json::from_slice(&std::fs::read(wt3.join(ARTIFACT)).unwrap()).unwrap();
    assert_eq!(
        (report["version"].as_str(), report["ready"].as_bool()),
        (Some("1.3.0"), Some(true))
    );
    // Each attempt ran in its own worktree, never in the fixture repo itself.
    for repo in &h.repos {
        assert!(!repo.join(ARTIFACT).exists(), "{} was written in place", repo.display());
    }
    assert!(
        list.iter()
            .all(|i| i["attempt"] == 1 && i["task_session_id"].is_string())
    );

    // One summary for the terminal job.
    let (first_key, first_counts) = {
        let w = h.exec.world.lock().unwrap();
        assert_eq!(w.summaries.len(), 1);
        let s = &w.summaries[0];
        assert_eq!((s.kind, s.status.as_str()), ("failed", "failed"));
        (s.dedupe_key.clone(), s.counts)
    };
    assert_eq!(first_key, format!("batch:{id}"));
    assert_eq!(
        (first_counts.passed, first_counts.failed, first_counts.timeout),
        (17, 2, 1)
    );

    // Single-item retry of #7 once its agent behaves.
    h.exec.set(7, Agent::Check);
    let old_ts7 = claims["task_session_id"].as_str().unwrap().to_owned();
    let item7 = claims["id"].as_str().unwrap().to_owned();
    let (st, body) = call(
        &h,
        "POST",
        &format!("/api/batch/{id}/retry"),
        Some(json!({"item_ids": [item7]})),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    wait_idle(&h, &id).await;

    let list = items(&h, &id).await;
    let retried = by_seq(&list, 7);
    assert_eq!(
        (retried["status"].as_str(), retried["attempt"].as_i64()),
        (Some("passed"), Some(2))
    );
    assert!(retried["error"].is_null());
    let new_ts7 = retried["task_session_id"].as_str().unwrap();
    assert_ne!(new_ts7, old_ts7, "a retry gets a new TaskSession");
    // Only #7 re-ran; the other failures are untouched.
    assert_eq!(by_seq(&list, 12)["attempt"], 1);
    assert_eq!(by_seq(&list, 15)["attempt"], 1);
    {
        let w = h.exec.world.lock().unwrap();
        // The old attempt is kept for comparison: its session and worktree.
        let (_, old_wt, old_outcome) = &w.sessions[&old_ts7];
        assert_eq!(*old_outcome, Some(AttemptOutcome::Failed));
        assert!(old_wt.is_dir() && !old_wt.join(ARTIFACT).exists());
        let (_, new_wt, new_outcome) = &w.sessions[new_ts7];
        assert_eq!(*new_outcome, Some(AttemptOutcome::Passed));
        assert!(new_wt.join(ARTIFACT).is_file());
        assert_eq!(w.sessions.len(), REPOS as usize + 1);

        // The same `batch:{id}` summary is refreshed with the new counts.
        assert_eq!(w.summaries.len(), 2);
        let s = &w.summaries[1];
        assert_eq!(s.dedupe_key, first_key);
        assert_eq!((s.counts.passed, s.counts.failed, s.counts.timeout), (18, 1, 1));
        assert_eq!(s.kind, "failed");
    }
    let (_, job) = call(&h, "GET", &format!("/api/batch/{id}"), None).await;
    assert_eq!(job["data"]["status"], "failed");
    assert_eq!(job["data"]["counts"]["passed"], 18);

    // Retry the rest by filter → every repo has a valid report.
    h.exec.set(12, Agent::Check);
    h.exec.set(15, Agent::Check);
    let (st, body) = call(
        &h,
        "POST",
        &format!("/api/batch/{id}/retry"),
        Some(json!({"filter": "failed"})),
    )
    .await;
    assert_eq!(st, 200, "{body}");
    wait_idle(&h, &id).await;
    let (_, job) = call(&h, "GET", &format!("/api/batch/{id}"), None).await;
    assert_eq!(job["data"]["status"], "completed", "{job}");
    assert_eq!(job["data"]["counts"]["passed"], REPOS);
    {
        let w = h.exec.world.lock().unwrap();
        let s = w.summaries.last().unwrap();
        assert_eq!(
            (s.kind, s.counts.passed, s.dedupe_key.as_str()),
            ("completed", 20, first_key.as_str())
        );
    }

    // The CSV export lists every repo once.
    let (st, csv) = call(&h, "GET", &format!("/api/batch/{id}/export.csv"), None).await;
    assert_eq!(st, 200);
    assert_eq!(csv.as_str().unwrap().lines().count(), REPOS as usize + 1);
}
