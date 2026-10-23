//! Delivery tests drive a fake `gh` / `glab` shell script (design §9 ①).
#![cfg(unix)]

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as StdCommand;
use std::sync::Arc;

use super::*;
use crate::resolver::AllowedRootsResolver;

struct Fixture {
    service: DeliveryService,
    db: aionui_db::Database,
    tmp: tempfile::TempDir,
    repo: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let status = StdCommand::new("git")
        .args(["-c", "user.name=T", "-c", "user.email=t@example.com"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A stub CLI: records argv / stdin / env, answers `auth status` from
/// `auth_ok`, and prints `create_output` (exit `create_exit`) for create.
fn stub(dir: &Path, name: &str, auth_ok: bool, create_output: &str, create_exit: i32) -> PathBuf {
    let path = dir.join(name);
    let log = dir.join(format!("{name}.log"));
    let script = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$@\" > '{log}.args'\n\
         env > '{log}.env'\n\
         if [ \"$1\" = auth ]; then {auth}; fi\n\
         cat > '{log}.stdin'\n\
         printf '%s\\n' '{out}'\n\
         exit {code}\n",
        log = log.display(),
        auth = if auth_ok {
            "echo 'Logged in'; exit 0"
        } else {
            "echo 'You are not logged into any hosts. Run gh auth login' >&2; exit 1"
        },
        out = create_output,
        code = create_exit,
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn base_env() -> Vec<(OsString, OsString)> {
    vec![
        ("PATH".into(), std::env::var_os("PATH").unwrap_or_default()),
        ("AIONUI_LOCAL_SECRET".into(), "s3cret".into()),
        ("AIONUI_RUNTIME_TOKEN".into(), "rt".into()),
        ("HOME".into(), std::env::var_os("HOME").unwrap_or_default()),
    ]
}

async fn fixture(remote_url: &str, gh: Option<PathBuf>) -> Fixture {
    let db = aionui_db::init_database_memory().await.unwrap();
    for user in ["u1", "u2"] {
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)")
            .bind(user)
            .bind(user)
            .execute(db.pool())
            .await
            .unwrap();
    }
    seed_run(&db).await;
    let tmp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(tmp.path()).unwrap();
    let repo = root.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "feat/x"]);
    std::fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "a.txt"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    git(&repo, &["remote", "add", "origin", remote_url]);
    let resolver = Arc::new(AllowedRootsResolver::new(vec![root.clone()]));
    let gh = gh.unwrap_or_else(|| root.join("missing-gh"));
    let clis = DeliveryClis {
        git: GitCli::new("git"),
        gh: HostCli::new(gh).with_base_env(base_env()),
        glab: HostCli::new(root.join("missing-glab")).with_base_env(base_env()),
    };
    let service = DeliveryService::new(db.pool().clone(), resolver).with_clis(clis);
    Fixture { service, db, tmp, repo }
}

async fn seed_run(db: &aionui_db::Database) {
    for sql in [
        "INSERT INTO conversations (id, user_id, name, type, extra, status, created_at, updated_at) \
         VALUES ('conv-1', 'u1', 'c', 'acp', '{}', 'finished', 1, 1)",
        "INSERT INTO task_sessions \
         (id, user_id, title, conversation_id, mode, objective, acceptance_criteria, status, agent_type, created_at, updated_at) \
         VALUES ('task-1', 'u1', 'T', 'conv-1', 'agent', '', '[]', 'completed', 'codex', 1, 1)",
        "INSERT INTO task_artifacts (id, task_session_id, kind, version, content, content_hash, status, created_at, updated_at) \
         VALUES ('art-1', 'task-1', 'plan', 1, '{}', 'h', 'approved', 1, 1)",
        "INSERT INTO task_approvals (id, task_session_id, approval_type, artifact_id, artifact_hash, status, requested_at) \
         VALUES ('appr-1', 'task-1', 'plan', 'art-1', 'h', 'approved', 1)",
        "INSERT INTO task_runs (id, task_session_id, conversation_id, run_kind, plan_artifact_id, approval_id, status, started_at) \
         VALUES ('run-1', 'task-1', 'conv-1', 'execution', 'art-1', 'appr-1', 'completed', 1)",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
}

fn request(repo: &Path) -> CreatePullRequestRequest {
    CreatePullRequestRequest {
        repo_path: repo.to_string_lossy().into_owned(),
        branch: "feat/x".into(),
        base: "main".into(),
        title: "Add x".into(),
        body: "Summary\n\n- resolved: 2 annotations".into(),
        draft: true,
        remote: None,
        task_session_id: Some("task-1".into()),
        run_id: Some("run-1".into()),
        worktree_id: None,
    }
}

#[tokio::test]
async fn creates_pr_and_records_task_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh", true, "https://github.com/o/r/pull/42", 0);
    let fx = fixture("https://github.com/o/r.git", Some(gh)).await;

    let created = fx.service.create_pull_request("u1", request(&fx.repo)).await.unwrap();
    assert_eq!(created.pr_url, "https://github.com/o/r/pull/42");
    assert_eq!(created.pr_number.as_deref(), Some("42"));
    assert_eq!(created.provider, HostProvider::Github);
    let evidence_id = created.evidence_id.expect("evidence written");

    let row = sqlx::query("SELECT kind, reference, metadata, run_id FROM task_evidence WHERE id = ?")
        .bind(&evidence_id)
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("kind"), "artifact");
    assert_eq!(row.get::<String, _>("reference"), "https://github.com/o/r/pull/42");
    assert_eq!(row.get::<String, _>("run_id"), "run-1");
    let meta: serde_json::Value = serde_json::from_str(&row.get::<String, _>("metadata")).unwrap();
    assert_eq!(meta["type"], "pull_request");
    assert_eq!(meta["url"], "https://github.com/o/r/pull/42");
    assert_eq!(meta["draft"], true);

    let listed = fx
        .service
        .list(
            "u1",
            ListScmDeliveriesQuery {
                task_session_id: Some("task-1".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].pr_url.as_deref(), Some("https://github.com/o/r/pull/42"));
    assert!(listed[0].commit_sha.as_deref().is_some_and(|s| s.len() == 40));
    assert!(
        fx.service
            .list("u2", ListScmDeliveriesQuery::default())
            .await
            .unwrap()
            .is_empty()
    );

    // argv and stdin as designed; the body never reaches argv.
    let args = std::fs::read_to_string(tmp.path().join("gh.log.args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "pr",
            "create",
            "--head",
            "feat/x",
            "--base",
            "main",
            "--title",
            "Add x",
            "--body-file",
            "-",
            "--draft"
        ]
    );
    let stdin = std::fs::read_to_string(tmp.path().join("gh.log.stdin")).unwrap();
    assert_eq!(stdin, "Summary\n\n- resolved: 2 annotations");
    // Scrubbed environment, non-interactive.
    let env = std::fs::read_to_string(tmp.path().join("gh.log.env")).unwrap();
    assert!(!env.contains("AIONUI_"), "{env}");
    assert!(!env.contains("s3cret"));
    assert!(env.contains("GH_PROMPT_DISABLED=1"));
    assert!(env.contains("GIT_TERMINAL_PROMPT=0"));
    drop(fx.tmp);
}

#[tokio::test]
async fn delivery_without_task_run_writes_no_evidence() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh", true, "https://github.com/o/r/pull/7", 0);
    let fx = fixture("git@github.com:o/r.git", Some(gh)).await;
    let mut req = request(&fx.repo);
    req.task_session_id = None;
    req.run_id = None;
    let created = fx.service.create_pull_request("u1", req).await.unwrap();
    assert!(created.evidence_id.is_none());
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM task_evidence")
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn foreign_task_run_is_rejected_before_cli_runs() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh", true, "https://github.com/o/r/pull/1", 0);
    let fx = fixture("https://github.com/o/r.git", Some(gh)).await;
    let err = fx
        .service
        .create_pull_request("u2", request(&fx.repo))
        .await
        .unwrap_err();
    // The repo is inside the allowed roots, but the task run is u1's.
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: CODE_RUN_NOT_FOUND,
                ..
            }
        ),
        "{err:?}"
    );
    assert!(!tmp.path().join("gh.log.args").exists(), "CLI must not run");
}

#[tokio::test]
async fn detect_host_reports_install_and_login() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(tmp.path(), "gh", false, "", 0);
    let fx = fixture("https://tok:x@github.com/o/r.git", Some(gh)).await;
    let query = DetectHostQuery {
        repo_path: fx.repo.to_string_lossy().into_owned(),
        remote: None,
    };
    let status = fx.service.detect_host("u1", query.clone()).await.unwrap();
    assert_eq!(status.provider, HostProvider::Github);
    assert_eq!(status.cli, Some("gh"));
    assert!(status.installed);
    assert!(!status.authenticated);
    assert!(status.detail.unwrap().contains("gh auth login"));
    assert!(!status.remote_url_redacted.unwrap().contains("tok"));
    let args = std::fs::read_to_string(tmp.path().join("gh.log.args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        ["auth", "status", "--hostname", "github.com"]
    );

    // Not installed.
    let fx = fixture("https://gitlab.com/g/r.git", None).await;
    let status = fx
        .service
        .detect_host(
            "u1",
            DetectHostQuery {
                repo_path: fx.repo.to_string_lossy().into_owned(),
                remote: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(status.provider, HostProvider::Gitlab);
    assert_eq!(status.cli, Some("glab"));
    assert!(!status.installed);
    assert!(!status.authenticated);
}

#[tokio::test]
async fn cli_failures_map_to_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let gh = stub(
        tmp.path(),
        "gh",
        true,
        "To get started with GitHub CLI, please run:  gh auth login",
        4,
    );
    let fx = fixture("https://github.com/o/r.git", Some(gh)).await;
    let err = fx
        .service
        .create_pull_request("u1", request(&fx.repo))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: CODE_CLI_NOT_AUTHENTICATED,
                ..
            }
        ),
        "{err:?}"
    );

    let gh = stub(
        tmp.path(),
        "gh",
        true,
        "a pull request for branch feat/x already exists",
        1,
    );
    let fx = fixture("https://github.com/o/r.git", Some(gh)).await;
    let err = fx
        .service
        .create_pull_request("u1", request(&fx.repo))
        .await
        .unwrap_err();
    assert!(
        matches!(&err, ReviewError::Conflict { code: CODE_CREATE_FAILED, message } if message.contains("already exists")),
        "{err:?}"
    );

    let fx = fixture("https://github.com/o/r.git", None).await;
    let err = fx
        .service
        .create_pull_request("u1", request(&fx.repo))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: CODE_CLI_NOT_INSTALLED,
                ..
            }
        ),
        "{err:?}"
    );

    let fx = fixture("https://gitea.corp.local/o/r.git", None).await;
    let err = fx
        .service
        .create_pull_request("u1", request(&fx.repo))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            ReviewError::Conflict {
                code: CODE_PROVIDER_UNSUPPORTED,
                ..
            }
        ),
        "{err:?}"
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM scm_deliveries")
        .fetch_one(fx.db.pool())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn validation_and_sandbox() {
    let fx = fixture("https://github.com/o/r.git", None).await;
    for mutate in [
        (|r: &mut CreatePullRequestRequest| r.branch = "-x".into()) as fn(&mut CreatePullRequestRequest),
        |r| r.base = "main..x".into(),
        |r| r.base = "feat/x".into(),
        |r| r.title = "  ".into(),
        |r| r.title = "a\nb".into(),
        |r| r.body = "x".repeat(MAX_PR_BODY_BYTES + 1),
        |r| r.run_id = None,
        |r| r.remote = Some("--upload-pack=x".into()),
    ] {
        let mut req = request(&fx.repo);
        mutate(&mut req);
        let err = fx.service.create_pull_request("u1", req).await.unwrap_err();
        assert!(matches!(err, ReviewError::BadRequest(_)), "{err:?}");
    }
    let mut req = request(&fx.repo);
    req.repo_path = "/etc".into();
    assert!(matches!(
        fx.service.create_pull_request("u1", req).await.unwrap_err(),
        ReviewError::BadRequest(_)
    ));
}
