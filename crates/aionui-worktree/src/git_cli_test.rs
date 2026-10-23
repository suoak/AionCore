use super::*;

#[test]
fn parses_git_versions() {
    assert_eq!(
        GitVersion::parse("git version 2.47.3\n"),
        Some(GitVersion {
            major: 2,
            minor: 47,
            patch: 3
        })
    );
    assert_eq!(
        GitVersion::parse("git version 2.39.2.windows.1"),
        Some(GitVersion {
            major: 2,
            minor: 39,
            patch: 2
        })
    );
    assert_eq!(
        GitVersion::parse("git version 2.39.5 (Apple Git-154)"),
        Some(GitVersion {
            major: 2,
            minor: 39,
            patch: 5
        })
    );
    assert_eq!(GitVersion::parse("hg version 6"), None);
    assert!(GitVersion::parse("git version 2.30.9").unwrap() < MIN_GIT_VERSION);
    assert!(GitVersion::parse("git version 2.31.0").unwrap() >= MIN_GIT_VERSION);
}

#[test]
fn scrubs_workmate_secrets_and_repo_redirects() {
    let env: Vec<(OsString, OsString)> = [
        ("PATH", "/usr/bin"),
        ("HOME", "/home/u"),
        ("AIONUI_RUNTIME_TOKEN", "tok"),
        ("AIONUI_USER_ID", "u1"),
        ("aionui_lower", "x"),
        ("AIONCORE_BOOTSTRAP_SECRET", "s"),
        ("AION_MCP_TOKEN", "t"),
        ("CSBU_WORKMATE_OFFICECLI_PATH", "/x"),
        ("SOME_LOCAL_SECRET", "s"),
        ("X_RUNTIME_TOKEN", "t"),
        ("GIT_DIR", "/elsewhere/.git"),
        ("GIT_WORK_TREE", "/elsewhere"),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "core.sshCommand"),
        ("GIT_CONFIG_VALUE_0", "evil"),
        ("GIT_TERMINAL_PROMPT", "1"),
        ("NODE_OPTIONS", "--inspect"),
        ("SSH_AUTH_SOCK", "/tmp/agent"),
        ("GIT_AUTHOR_NAME", "Dev"),
    ]
    .into_iter()
    .map(|(k, v)| (OsString::from(k), OsString::from(v)))
    .collect();
    let kept: Vec<String> = scrub_env(env)
        .into_iter()
        .map(|(k, _)| k.to_string_lossy().into_owned())
        .collect();
    assert_eq!(kept, ["PATH", "HOME", "SSH_AUTH_SOCK", "GIT_AUTHOR_NAME"]);
}

#[test]
fn rejects_option_injection_in_revs() {
    for bad in [
        "",
        "-x",
        "--upload-pack=touch /tmp/pwn",
        "main;rm -rf /",
        "main\nHEAD",
        "a b",
        "main..dev",
        "HEAD:secret",
        "$(id)",
        "`id`",
    ] {
        assert!(validate_rev_text(bad).is_err(), "{bad:?}");
    }
    for good in [
        "main",
        "origin/main",
        "v1.2.3",
        "HEAD~2",
        "HEAD^",
        "abc123def",
        "feature/x-1",
        "@",
    ] {
        assert!(validate_rev_text(good).is_ok(), "{good:?}");
    }
}

#[test]
fn rejects_bad_branch_names() {
    for bad in [
        "", "-b", "--force", "/abs", "a/", "a..b", "a//b", "x.lock", "a b", "a;b", "a\nb", "HEAD", "wm/~1", "a^", "a:b",
    ] {
        assert!(validate_branch_name(bad).is_err(), "{bad:?}");
    }
    for good in ["wm/abcd1234-fix-login", "feature/x_y.z", "main"] {
        assert!(validate_branch_name(good).is_ok(), "{good:?}");
    }
}

#[test]
fn parses_progress_percent() {
    assert_eq!(parse_progress_percent("Updating files:  45% (450/1000)"), Some(45));
    assert_eq!(parse_progress_percent("Updating files: 100% (3/3), done."), Some(100));
    assert_eq!(parse_progress_percent("Preparing worktree"), None);
}

#[tokio::test]
async fn missing_program_is_not_available() {
    let cli = GitCli::new("/definitely/not/here/git-xyz");
    let err = cli.probe_version().await.unwrap_err();
    assert!(matches!(err, GitCliError::NotAvailable(_)), "{err:?}");
}

/// Fake `git` stubs: a shell script on Unix, a `.cmd` on Windows.
mod stub {
    use std::path::{Path, PathBuf};

    pub fn write(dir: &Path, unix_body: &str, windows_body: &str) -> PathBuf {
        if cfg!(windows) {
            let path = dir.join("fake-git.cmd");
            std::fs::write(&path, format!("@echo off\r\n{windows_body}\r\n")).unwrap();
            path
        } else {
            let path = dir.join("fake-git");
            std::fs::write(&path, format!("#!/bin/sh\n{unix_body}\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            path
        }
    }
}

#[tokio::test]
async fn missing_cwd_is_not_reported_as_missing_git() {
    let dir = tempfile::tempdir().unwrap();
    let err = GitCli::default()
        .run(["status"], RunOptions::in_dir(dir.path().join("gone")))
        .await
        .unwrap_err();
    assert!(matches!(err, GitCliError::Io(_)), "{err:?}");
}

#[tokio::test]
async fn too_old_git_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(dir.path(), "echo 'git version 2.25.1'", "echo git version 2.25.1");
    let err = GitCli::new(program).probe_version().await.unwrap_err();
    assert!(matches!(err, GitCliError::TooOld { .. }), "{err:?}");
}

#[tokio::test]
async fn child_environment_is_scrubbed() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(dir.path(), "env", "set");
    let mut base: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !is_scrubbed_env(k)).collect();
    base.extend([
        ("AIONUI_RUNTIME_TOKEN".to_owned(), "tok-secret".to_owned()),
        ("AIONUI_BASE_URL".to_owned(), "http://127.0.0.1:1".to_owned()),
        ("AIONCORE_BOOTSTRAP_SECRET".to_owned(), "boot-secret".to_owned()),
        ("GIT_TERMINAL_PROMPT".to_owned(), "1".to_owned()),
        ("WORKMATE_TEST_KEEP".to_owned(), "kept".to_owned()),
    ]);
    let cli = GitCli::new(program).with_base_env(base);
    let out = cli.run_ok(["status"], RunOptions::default()).await.unwrap();
    let text = out.stdout.to_ascii_uppercase();
    assert!(!text.contains("AIONUI_"), "{}", out.stdout);
    assert!(!text.contains("AIONCORE_"), "{}", out.stdout);
    assert!(!out.stdout.contains("tok-secret"));
    assert!(!out.stdout.contains("boot-secret"));
    assert!(text.contains("GIT_TERMINAL_PROMPT=0"), "{}", out.stdout);
    assert!(text.contains("GCM_INTERACTIVE=NEVER"), "{}", out.stdout);
    assert!(text.contains("WORKMATE_TEST_KEEP=KEPT"), "{}", out.stdout);
}

#[tokio::test]
async fn arguments_are_passed_verbatim_not_through_a_shell() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("pwned");
    // The stub prints its argv one per line; if args were shell-joined the
    // `touch` would run.
    let program = stub::write(dir.path(), r#"for a in "$@"; do echo "[$a]"; done"#, "echo [%*]");
    let evil = format!("x; touch {}", marker.display());
    let out = GitCli::new(program)
        .run_ok(["log", evil.as_str()], RunOptions::default())
        .await
        .unwrap();
    assert!(out.stdout.contains("x; touch"), "{}", out.stdout);
    assert!(!marker.exists());
}

#[tokio::test]
async fn timeout_kills_the_git_process() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(dir.path(), "sleep 30", "ping -n 30 127.0.0.1 >nul");
    let started = std::time::Instant::now();
    let err = GitCli::new(program)
        .run(
            ["fetch"],
            RunOptions::default().with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, GitCliError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(20), "{:?}", started.elapsed());
}

#[tokio::test]
async fn cancellation_kills_the_git_process() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(dir.path(), "sleep 30", "ping -n 30 127.0.0.1 >nul");
    let token = CancellationToken::new();
    let trigger = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(300)).await;
        trigger.cancel();
    });
    let started = std::time::Instant::now();
    let err = GitCli::new(program)
        .run(["worktree"], RunOptions::default().with_cancel(token))
        .await
        .unwrap_err();
    assert!(matches!(err, GitCliError::Cancelled { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
}

/// The whole process group dies, not only the direct child.
#[cfg(unix)]
#[tokio::test]
async fn timeout_kills_the_process_tree() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("grandchild.pid");
    let program = stub::write(
        dir.path(),
        &format!("sleep 60 & echo $! > {}\nwait", pid_file.display()),
        "",
    );
    let err = GitCli::new(program)
        .run(
            ["fetch"],
            RunOptions::default().with_timeout(Duration::from_millis(700)),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, GitCliError::Timeout { .. }));
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
    let mut alive = true;
    for _ in 0..40 {
        // signal 0 probes existence; zombies are reaped by init quickly.
        alive = std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(!alive, "grandchild {pid} survived the timeout");
}

#[tokio::test]
async fn progress_lines_are_streamed() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(
        dir.path(),
        r#"printf 'Updating files:  10%%\rUpdating files: 100%%\n' >&2"#,
        "echo Updating files: 100%% 1>&2",
    );
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = seen.clone();
    GitCli::new(program)
        .run_ok(
            ["worktree", "add"],
            RunOptions::default().with_progress(Arc::new(move |line: &str| sink.lock().unwrap().push(line.to_owned()))),
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    assert!(seen.iter().any(|l| parse_progress_percent(l) == Some(100)), "{seen:?}");
}

#[tokio::test]
async fn stdin_is_fed_and_closed() {
    // `hash-object --stdin` only finishes once stdin reaches EOF, so this also
    // proves the pipe is closed after the input is written.
    let out = GitCli::default()
        .run_ok(
            ["hash-object", "--stdin"],
            RunOptions::default().with_stdin(b"hello\n".to_vec()),
        )
        .await
        .expect("hash-object");
    assert_eq!(out.stdout.trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
}

#[tokio::test]
async fn stdin_is_null_by_default() {
    let out = GitCli::default()
        .run_ok(["hash-object", "--stdin"], RunOptions::default())
        .await
        .expect("hash-object");
    // The empty blob: a null stdin reads as immediate EOF.
    assert_eq!(out.stdout.trim(), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
}

#[tokio::test]
async fn per_run_env_is_applied_but_still_scrubbed() {
    let dir = tempfile::tempdir().unwrap();
    let program = stub::write(dir.path(), "env", "set");
    let base: Vec<(String, String)> = std::env::vars().filter(|(k, _)| !is_scrubbed_env(k)).collect();
    let out = GitCli::new(program)
        .with_base_env(base)
        .run_ok(
            ["push"],
            RunOptions::default()
                .with_env("LC_ALL", "C")
                .with_env("AIONUI_RUNTIME_TOKEN", "tok-secret"),
        )
        .await
        .unwrap();
    assert!(out.stdout.contains("LC_ALL=C"), "{}", out.stdout);
    assert!(!out.stdout.contains("tok-secret"), "{}", out.stdout);
}
