//! `scm/commit`, `scm/push` and `scm/remoteInfo` through the actor (M7 E4-4):
//! the wire shapes, the asynchronous replies, `scm/progress` reaching only the
//! requesting connection, and the status broadcast after a commit.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use aionui_db::{Database, IProjectStore, SqliteProjectStore, init_database_memory};
use aionui_project::ProjectService;
use aionui_project::canonical::to_file_uri;
use aionui_project::scm::{ScmActor, ScmInbound, ScmWirePush};
use serde_json::{Value, json};
use tokio::sync::mpsc::{UnboundedSender, unbounded_channel};

struct CollectingPush {
    sent: std::sync::Mutex<Vec<(String, Value)>>,
}

impl ScmWirePush for CollectingPush {
    fn push(&self, session: &str, frame: Value) {
        self.sent.lock().expect("sink").push((session.to_owned(), frame));
    }
}

struct Fixture {
    _db: Database,
    _tmp: tempfile::TempDir,
    _service: Arc<ProjectService>,
    push: Arc<CollectingPush>,
    inbound: UnboundedSender<ScmInbound>,
    project_id: String,
    work: std::path::PathBuf,
    bare: std::path::PathBuf,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

async fn fixture() -> Fixture {
    let db = init_database_memory().await.expect("db");
    let store: Arc<dyn IProjectStore> = Arc::new(SqliteProjectStore::new(db.pool().clone()));
    let service = Arc::new(ProjectService::new(Arc::clone(&store), std::env::temp_dir()));

    let tmp = tempfile::tempdir().expect("tempdir");
    let bare = tmp.path().join("remote.git");
    let work = tmp.path().join("work");
    std::fs::create_dir_all(&bare).expect("mkdir");
    std::fs::create_dir_all(&work).expect("mkdir");
    git(&bare, &["init", "--bare", "-q", "-b", "main"]);
    git(&work, &["init", "-q", "-b", "main"]);
    git(&work, &["config", "user.name", "scm test"]);
    git(&work, &["config", "user.email", "scm@test.local"]);
    git(&work, &["remote", "add", "origin", &bare.to_string_lossy()]);
    std::fs::write(work.join("a.txt"), "base\n").expect("write");
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "base"]);

    let created = service
        .create_standard("system_default_user", to_file_uri(&work).expect("uri"))
        .await
        .expect("create project");
    let project_id = created.project.project_id.clone();

    let push = Arc::new(CollectingPush {
        sent: std::sync::Mutex::new(Vec::new()),
    });
    let actor = ScmActor::new(Arc::clone(&service), Arc::clone(&push) as Arc<dyn ScmWirePush>).expect("actor");
    let (inbound, inbound_rx) = unbounded_channel();
    tokio::spawn(actor.run(inbound_rx));

    Fixture {
        _db: db,
        _tmp: tmp,
        _service: service,
        push,
        inbound,
        project_id,
        work,
        bare,
    }
}

impl Fixture {
    fn send(&self, session: &str, id: u64, method: &str, params: Value) {
        self.inbound
            .send(ScmInbound::Frame {
                session: session.to_owned(),
                user_id: "system_default_user".to_owned(),
                frame: json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            })
            .expect("actor alive");
    }

    /// Wait (bounded — push spawns git) for the reply to `id` on `session`.
    async fn reply(&self, session: &str, id: u64) -> Value {
        for _ in 0..1500 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let sent = self.push.sent.lock().expect("sink");
            if let Some((_, frame)) = sent
                .iter()
                .find(|(s, f)| s == session && f.get("id").and_then(Value::as_u64) == Some(id))
            {
                return frame.clone();
            }
        }
        panic!("no reply {id} to {session}");
    }

    async fn call(&self, session: &str, id: u64, method: &str, params: Value) -> Value {
        self.send(session, id, method, params);
        self.reply(session, id).await
    }

    fn notifications(&self, session: &str, method: &str) -> Vec<Value> {
        self.push
            .sent
            .lock()
            .expect("sink")
            .iter()
            .filter(|(s, f)| s == session && f.get("id").is_none() && f["method"] == method)
            .map(|(_, f)| f["params"].clone())
            .collect()
    }

    async fn repo_id(&self) -> String {
        let listed = self
            .call(
                "conn-1",
                1,
                "scm/listRepositories",
                json!({ "project_id": self.project_id }),
            )
            .await;
        listed["result"]["repositories"][0]["repo_id"]
            .as_str()
            .unwrap_or_else(|| panic!("no repository: {listed}"))
            .to_owned()
    }
}

fn error_name(reply: &Value) -> &str {
    reply["error"]["message"].as_str().unwrap_or("<none>")
}

#[tokio::test]
async fn commit_and_push_round_trip_over_the_wire() {
    let fx = fixture().await;
    let repo = fx.repo_id().await;
    // A second connection watches the repository too.
    let sub = fx
        .call("conn-2", 2, "scm/subscribe", json!({ "repositories": [repo] }))
        .await;
    assert!(sub.get("error").is_none(), "{sub}");

    let nothing = fx
        .call(
            "conn-1",
            3,
            "scm/commit",
            json!({ "repository": repo, "message": "empty" }),
        )
        .await;
    assert_eq!(error_name(&nothing), "COMMIT_NOTHING_STAGED", "{nothing}");

    std::fs::write(fx.work.join("a.txt"), "changed\n").expect("edit");
    git(&fx.work, &["add", "a.txt"]);
    let status_before = fx.notifications("conn-2", "scm/statusChanged").len();
    let committed = fx
        .call(
            "conn-1",
            4,
            "scm/commit",
            json!({ "repository": repo, "message": "feat: over the wire", "amend": false }),
        )
        .await;
    let sha = committed["result"]["commit_sha"]
        .as_str()
        .unwrap_or_else(|| panic!("{committed}"));
    assert_eq!(sha, git(&fx.work, &["rev-parse", "HEAD"]));
    for _ in 0..300 {
        if fx.notifications("conn-2", "scm/statusChanged").len() > status_before {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        fx.notifications("conn-2", "scm/statusChanged").len() > status_before,
        "subscribers hear about the commit"
    );

    let info = fx
        .call("conn-1", 5, "scm/remoteInfo", json!({ "repository": repo }))
        .await;
    let result = &info["result"];
    assert_eq!(result["remotes"][0]["name"], "origin", "{info}");
    assert_eq!(result["remotes"][0]["provider"], "local", "{info}");
    assert!(
        result.get("upstream").is_none() || result["upstream"].is_null(),
        "{info}"
    );

    let refused = fx.call("conn-1", 6, "scm/push", json!({ "repository": repo })).await;
    assert_eq!(error_name(&refused), "PUSH_NO_UPSTREAM", "{refused}");
    assert_eq!(refused["error"]["data"]["branch"], "main", "{refused}");

    let pushed = fx
        .call(
            "conn-1",
            7,
            "scm/push",
            json!({ "repository": repo, "set_upstream": true }),
        )
        .await;
    assert_eq!(
        pushed["result"],
        json!({ "remote": "origin", "branch": "main", "upstream": "origin/main", "ahead": 0 }),
        "{pushed}"
    );
    assert_eq!(git(&fx.bare, &["rev-parse", "refs/heads/main"]), sha);
    let progress = fx.notifications("conn-1", "scm/progress");
    assert!(!progress.is_empty(), "the requester sees push progress");
    assert!(
        progress.iter().all(|p| p["repository"] == repo.as_str()),
        "{progress:?}"
    );
    assert!(
        fx.notifications("conn-2", "scm/progress").is_empty(),
        "progress is not broadcast to other connections"
    );

    let info = fx
        .call("conn-1", 8, "scm/remoteInfo", json!({ "repository": repo }))
        .await;
    assert_eq!(info["result"]["upstream"], "origin/main", "{info}");
    assert_eq!(info["result"]["ahead"], 0, "{info}");
    assert_eq!(info["result"]["behind"], 0, "{info}");
}

#[tokio::test]
async fn bad_requests_are_refused_on_the_loop() {
    let fx = fixture().await;
    let repo = fx.repo_id().await;

    let amend = fx
        .call(
            "conn-1",
            2,
            "scm/commit",
            json!({ "repository": repo, "message": "x", "amend": true }),
        )
        .await;
    assert_eq!(error_name(&amend), "capability_unsupported", "{amend}");

    let blank = fx
        .call(
            "conn-1",
            3,
            "scm/commit",
            json!({ "repository": repo, "message": "  " }),
        )
        .await;
    assert_eq!(error_name(&blank), "invalid_params", "{blank}");

    let injected = fx
        .call(
            "conn-1",
            4,
            "scm/push",
            json!({ "repository": repo, "remote": "--receive-pack=touch pwned", "set_upstream": true }),
        )
        .await;
    assert_eq!(error_name(&injected), "invalid_params", "{injected}");

    let unknown = fx
        .call("conn-1", 5, "scm/push", json!({ "repository": "scm:does-not-exist" }))
        .await;
    assert!(unknown.get("error").is_some(), "{unknown}");
    let unknown = fx
        .call(
            "conn-1",
            6,
            "scm/remoteInfo",
            json!({ "repository": "scm:does-not-exist" }),
        )
        .await;
    assert!(unknown.get("error").is_some(), "{unknown}");
}
