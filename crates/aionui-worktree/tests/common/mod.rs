#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use aionui_worktree::{GitCli, WorktreeEvents, WorktreeService, WorktreeServiceConfig, WorktreeStore};

pub const USER: &str = "system_default_user";
pub const OTHER_USER: &str = "user-other";

pub fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Run real git in `dir` for fixture setup, panicking on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn git");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

pub fn git_ok(dir: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A repository with one commit on `main`.
pub fn init_repo(dir: &Path) -> PathBuf {
    let repo = dir.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "dev@example.com"]);
    git(&repo, &["config", "user.name", "Dev"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "README.md"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    dunce::canonicalize(repo).unwrap()
}

pub fn commit_file(dir: &Path, name: &str, content: &str) {
    std::fs::write(dir.join(name), content).unwrap();
    git(dir, &["add", name]);
    git(
        dir,
        &[
            "-c",
            "user.email=dev@example.com",
            "-c",
            "user.name=Dev",
            "commit",
            "-q",
            "-m",
            name,
        ],
    );
}

pub fn branch_exists(repo: &Path, branch: &str) -> bool {
    git_ok(
        repo,
        &["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")],
    )
}

pub fn worktree_listed(repo: &Path, path: &str) -> bool {
    let list = git(repo, &["worktree", "list", "--porcelain"]).replace('\\', "/");
    let want = path.replace('\\', "/");
    list.lines().any(|l| {
        l.strip_prefix("worktree ")
            .is_some_and(|p| p.eq_ignore_ascii_case(&want))
    })
}

#[derive(Default)]
pub struct RecordingBroadcaster {
    pub events: Mutex<Vec<WebSocketMessage<serde_json::Value>>>,
}

impl EventBroadcaster for RecordingBroadcaster {
    fn broadcast(&self, event: WebSocketMessage<serde_json::Value>) {
        self.events.lock().unwrap().push(event);
    }
}

impl RecordingBroadcaster {
    pub fn statuses(&self, id: &str) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|e| e.name == "worktree.changed" && e.data["worktree_id"] == id)
            .map(|e| e.data["status"].as_str().unwrap_or_default().to_owned())
            .collect()
    }
}

pub struct Harness {
    pub tmp: tempfile::TempDir,
    pub repo: PathBuf,
    pub root: PathBuf,
    pub db: aionui_db::Database,
    pub service: WorktreeService,
    pub events: Arc<RecordingBroadcaster>,
}

pub async fn harness() -> Harness {
    harness_with_git(|_| GitCli::default()).await
}

pub async fn harness_with_git(make_git: impl FnOnce(&Path) -> GitCli) -> Harness {
    let tmp = tempfile::tempdir().unwrap();
    let repo = init_repo(tmp.path());
    let root = tmp.path().join("data").join("worktrees");
    let db = aionui_db::init_database_memory().await.unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, 'x', 0, 0)",
    )
    .bind(OTHER_USER)
    .bind(OTHER_USER)
    .execute(db.pool())
    .await
    .unwrap();
    let events = Arc::new(RecordingBroadcaster::default());
    let git = make_git(tmp.path());
    let service = WorktreeService::new(
        WorktreeStore::new(db.pool().clone()),
        WorktreeServiceConfig {
            root_dir: root.clone(),
            git,
            events: WorktreeEvents::new(events.clone()),
        },
    );
    Harness {
        tmp,
        repo,
        root,
        db,
        service,
        events,
    }
}

impl Harness {
    pub fn second_service(&self) -> WorktreeService {
        WorktreeService::new(
            WorktreeStore::new(self.db.pool().clone()),
            WorktreeServiceConfig {
                root_dir: self.root.clone(),
                git: GitCli::default(),
                events: WorktreeEvents::disabled(),
            },
        )
    }

    pub async fn wait_status(&self, id: &str, want: &str) -> aionui_worktree::types::WorktreeResponse {
        self.service.wait_idle(id).await;
        for _ in 0..200 {
            let wt = self.service.get(USER, id).await.unwrap();
            if wt.status == want {
                return wt;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("worktree {id} never reached {want}");
    }
}

/// A `git` wrapper that delegates to the real git but, while `<dir>/slow`
/// exists, sleeps after `worktree add` so tests can cancel mid-create.
pub fn slow_git(dir: &Path) -> GitCli {
    std::fs::write(dir.join("slow"), "1").unwrap();
    let flag = dir.join("slow");
    if cfg!(windows) {
        let path = dir.join("slow-git.cmd");
        let body = format!(
            "@echo off\r\ngit %*\r\nset rc=%errorlevel%\r\necho %* | findstr /C:\"worktree add\" >nul && if exist \"{}\" ping -n 60 127.0.0.1 >nul\r\nexit /b %rc%\r\n",
            flag.display()
        );
        std::fs::write(&path, body).unwrap();
        GitCli::new(path)
    } else {
        let path = dir.join("slow-git");
        let body = format!(
            "#!/bin/sh\ngit \"$@\"\nrc=$?\ncase \"$*\" in *\"worktree add\"*) if [ -f '{}' ]; then sleep 60; fi;; esac\nexit $rc\n",
            flag.display()
        );
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        GitCli::new(path)
    }
}

#[macro_export]
macro_rules! require_git {
    () => {
        if !common::git_available() {
            eprintln!("skipping: system git not available");
            return;
        }
    };
}
