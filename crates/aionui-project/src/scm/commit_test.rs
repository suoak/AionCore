//! Commit / push / remote info over real temporary repositories and a local
//! bare remote, driven through the system `git` with a pinned environment:
//! `HOME` is a temp dir and system config is off, so the developer's or CI
//! runner's identity, hooks and credential helpers never leak in. A WorkMate
//! secret is planted in the parent environment to prove the scrub.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use tempfile::TempDir;

use super::*;

const PLANTED_SECRET: &str = "tok-e44-planted-secret";

struct Sandbox {
    _tmp: TempDir,
    root: PathBuf,
    cli: GitCli,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = dunce_canonical(tmp.path());
        let home = root.join("home");
        std::fs::create_dir_all(&home).expect("home");
        let cli = GitCli::default().with_base_env([
            ("PATH".to_owned(), std::env::var("PATH").unwrap_or_default()),
            ("HOME".to_owned(), home.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME".to_owned(),
                home.join(".config").to_string_lossy().into_owned(),
            ),
            ("GIT_CONFIG_NOSYSTEM".to_owned(), "1".to_owned()),
            ("AIONUI_RUNTIME_TOKEN".to_owned(), PLANTED_SECRET.to_owned()),
            ("AIONUI_LOCAL_SECRET".to_owned(), PLANTED_SECRET.to_owned()),
            ("SYSTEMROOT".to_owned(), std::env::var("SYSTEMROOT").unwrap_or_default()),
        ]);
        Self { _tmp: tmp, root, cli }
    }

    async fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = self
            .cli
            .run_ok(args.iter().copied(), RunOptions::in_dir(dir))
            .await
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        out.stdout.trim().to_owned()
    }

    /// A bare remote.
    async fn bare(&self, name: &str) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("mkdir bare");
        self.git(&path, &["init", "--bare", "-q", "-b", "main"]).await;
        path
    }

    /// A work tree on `main`, with or without an identity.
    async fn repo(&self, name: &str, identity: bool) -> PathBuf {
        let path = self.root.join(name);
        std::fs::create_dir_all(&path).expect("mkdir repo");
        self.git(&path, &["init", "-q", "-b", "main"]).await;
        if identity {
            self.git(&path, &["config", "user.name", "E44 Tester"]).await;
            self.git(&path, &["config", "user.email", "e44@test.local"]).await;
        }
        path
    }

    async fn stage(&self, repo: &Path, file: &str, body: &str) {
        std::fs::write(repo.join(file), body).expect("write");
        self.git(repo, &["add", "--", file]).await;
    }
}

fn dunce_canonical(path: &Path) -> PathBuf {
    let canonical = std::fs::canonicalize(path).expect("canonicalize");
    // Strip the verbatim prefix on Windows; git does not accept `\\?\` paths.
    let text = canonical.to_string_lossy();
    PathBuf::from(text.strip_prefix(r"\\?\").unwrap_or(&text).to_owned())
}

fn request(message: &str) -> CommitRequest {
    CommitRequest {
        message: message.to_owned(),
        ..CommitRequest::default()
    }
}

fn upstream_push() -> PushRequest {
    PushRequest {
        remote: None,
        set_upstream: true,
    }
}

#[cfg(unix)]
fn write_hook(dir: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::create_dir_all(dir).expect("hooks dir");
    std::fs::write(&path, body).expect("write hook");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod hook");
}

// ── acceptance ① commit + push to a local bare remote ──────────────────

#[tokio::test]
async fn commit_then_push_to_a_local_bare_remote() {
    let sb = Sandbox::new();
    let bare = sb.bare("remote.git").await;
    let repo = sb.repo("work", true).await;
    sb.git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()])
        .await;
    sb.stage(&repo, "a.txt", "one\n").await;

    let committed = commit(&sb.cli, &repo, &request("feat: first\n\nbody line"))
        .await
        .expect("commit");
    assert_eq!(committed.commit_sha.len(), 40, "{committed:?}");
    assert_eq!(sb.git(&repo, &["log", "-1", "--format=%s"]).await, "feat: first");
    assert_eq!(
        sb.git(&repo, &["log", "-1", "--format=%an <%ae>"]).await,
        "E44 Tester <e44@test.local>"
    );

    // No upstream yet: refused until the user confirms creating it.
    let err = push(&sb.cli, &repo, &PushRequest::default(), None)
        .await
        .expect_err("no upstream");
    assert!(
        matches!(&err, ScmError::PushNoUpstream { remote, branch } if remote == "origin" && branch == "main"),
        "{err:?}"
    );

    let steps: Arc<Mutex<Vec<ScmProgress>>> = Arc::default();
    let sink = {
        let steps = Arc::clone(&steps);
        Arc::new(move |step: ScmProgress| steps.lock().unwrap().push(step)) as ScmProgressFn
    };
    let pushed = push(&sb.cli, &repo, &upstream_push(), Some(sink)).await.expect("push");
    assert_eq!(
        pushed,
        PushResult {
            remote: "origin".into(),
            branch: "main".into(),
            upstream: "origin/main".into(),
            ahead: 0,
        }
    );
    assert_eq!(
        sb.git(&bare, &["rev-parse", "refs/heads/main"]).await,
        committed.commit_sha
    );
    let steps = steps.lock().unwrap().clone();
    assert!(
        steps.iter().any(|s| s.phase == "Writing objects"),
        "push progress is streamed: {steps:?}"
    );

    // With the upstream recorded, the next push needs no confirmation.
    sb.stage(&repo, "b.txt", "two\n").await;
    let second = commit(&sb.cli, &repo, &request("feat: second"))
        .await
        .expect("commit 2");
    let info = remote_info(&sb.cli, &repo).await.expect("remote info");
    assert_eq!(info.upstream.as_deref(), Some("origin/main"));
    assert_eq!((info.ahead, info.behind), (Some(1), Some(0)));
    push(&sb.cli, &repo, &PushRequest::default(), None)
        .await
        .expect("push 2");
    assert_eq!(
        sb.git(&bare, &["rev-parse", "refs/heads/main"]).await,
        second.commit_sha
    );

    let info = remote_info(&sb.cli, &repo).await.expect("remote info");
    assert_eq!(info.branch.as_deref(), Some("main"));
    assert_eq!((info.ahead, info.behind), (Some(0), Some(0)));
    assert_eq!(info.remotes.len(), 1);
    assert_eq!(info.remotes[0].name, "origin");
    assert_eq!(info.remotes[0].provider, "local");
    assert!(info.commit_hooks.is_empty(), "{:?}", info.commit_hooks);
}

#[tokio::test]
async fn commit_takes_only_what_is_staged() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    sb.stage(&repo, "a.txt", "one\n").await;
    std::fs::write(repo.join("unstaged.txt"), "left alone\n").expect("write");

    commit(&sb.cli, &repo, &request("only a")).await.expect("commit");
    assert_eq!(
        sb.git(&repo, &["show", "--name-only", "--format=", "HEAD"]).await,
        "a.txt"
    );

    let err = commit(&sb.cli, &repo, &request("again"))
        .await
        .expect_err("nothing staged");
    assert!(matches!(err, ScmError::CommitNothingStaged), "{err:?}");
}

#[tokio::test]
async fn commit_validates_its_request() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    sb.stage(&repo, "a.txt", "one\n").await;

    for bad in ["", "   \n\t", "nul\0byte"] {
        let err = commit(&sb.cli, &repo, &request(bad)).await.expect_err("bad message");
        assert!(matches!(err, ScmError::InvalidParams { what: "message" }), "{err:?}");
    }
    let huge = "x".repeat(MAX_MESSAGE_BYTES + 1);
    assert!(matches!(
        commit(&sb.cli, &repo, &request(&huge)).await,
        Err(ScmError::InvalidParams { what: "message" })
    ));
    let amend = CommitRequest {
        amend: true,
        ..request("amend me")
    };
    assert!(matches!(
        commit(&sb.cli, &repo, &amend).await,
        Err(ScmError::CapabilityUnsupported { capability: "amend" })
    ));
}

#[tokio::test]
async fn signoff_adds_the_trailer() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    sb.stage(&repo, "a.txt", "one\n").await;
    let req = CommitRequest {
        signoff: true,
        ..request("signed")
    };
    commit(&sb.cli, &repo, &req).await.expect("commit");
    let body = sb.git(&repo, &["log", "-1", "--format=%B"]).await;
    assert!(body.contains("Signed-off-by: E44 Tester <e44@test.local>"), "{body}");
}

// ── acceptance ① hooks ─────────────────────────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn failing_hook_is_reported_with_its_output() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    write_hook(
        &repo.join(".git/hooks"),
        "pre-commit",
        "#!/bin/sh\necho 'lint: trailing whitespace in a.txt'\necho 'leaked ghp_abcdefghijklmnopqrstuvwxyz0123' >&2\nexit 1\n",
    );
    sb.stage(&repo, "a.txt", "one \n").await;

    let info = remote_info(&sb.cli, &repo).await.expect("remote info");
    assert_eq!(info.commit_hooks, vec!["pre-commit".to_owned()]);

    let err = commit(&sb.cli, &repo, &request("blocked"))
        .await
        .expect_err("hook fails");
    let ScmError::CommitHookFailed { output } = &err else {
        panic!("expected COMMIT_HOOK_FAILED, got {err:?}");
    };
    assert!(output.contains("lint: trailing whitespace in a.txt"), "{output}");
    assert!(!output.contains("ghp_abcdefghij"), "tokens are redacted: {output}");
    assert!(output.contains("[REDACTED]"), "{output}");
    assert_eq!(
        sb.git(&repo, &["rev-list", "--all", "--count"]).await,
        "0",
        "nothing committed"
    );

    // The explicit opt-out skips the hook.
    let skip = CommitRequest {
        no_verify: true,
        ..request("skip hooks")
    };
    commit(&sb.cli, &repo, &skip).await.expect("no-verify commit");
}

#[cfg(unix)]
#[tokio::test]
async fn hooks_path_config_is_honoured() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    write_hook(
        &repo.join("custom-hooks"),
        "commit-msg",
        "#!/bin/sh\necho 'bad subject'\nexit 1\n",
    );
    sb.git(&repo, &["config", "core.hooksPath", "custom-hooks"]).await;
    sb.stage(&repo, "a.txt", "one\n").await;

    let err = commit(&sb.cli, &repo, &request("x"))
        .await
        .expect_err("commit-msg fails");
    assert!(
        matches!(&err, ScmError::CommitHookFailed { output } if output.contains("bad subject")),
        "{err:?}"
    );
}

// ── acceptance ④ no AIONUI_* in the child ──────────────────────────────

#[cfg(unix)]
#[tokio::test]
async fn hook_environment_carries_no_workmate_secrets() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    write_hook(
        &repo.join(".git/hooks"),
        "pre-commit",
        "#!/bin/sh\nenv > \"$(git rev-parse --git-dir)/hook-env.txt\"\nexit 0\n",
    );
    sb.stage(&repo, "a.txt", "one\n").await;
    commit(&sb.cli, &repo, &request("env probe")).await.expect("commit");

    let env = std::fs::read_to_string(repo.join(".git/hook-env.txt")).expect("hook wrote env");
    assert!(!env.contains("AIONUI_"), "{env}");
    assert!(!env.contains(PLANTED_SECRET), "{env}");
    assert!(env.contains("GIT_TERMINAL_PROMPT=0"), "{env}");
}

// ── acceptance ② identity ──────────────────────────────────────────────

#[tokio::test]
async fn missing_identity_is_reported_before_hooks_run() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", false).await;
    sb.stage(&repo, "a.txt", "one\n").await;

    let err = commit(&sb.cli, &repo, &request("who am i"))
        .await
        .expect_err("no identity");
    assert!(matches!(err, ScmError::CommitIdentityMissing { .. }), "{err:?}");
}

// ── acceptance ③ push failures ─────────────────────────────────────────

#[tokio::test]
async fn non_fast_forward_push_is_rejected() {
    let sb = Sandbox::new();
    let bare = sb.bare("remote.git").await;
    let url = bare.to_string_lossy().into_owned();
    let first = sb.repo("first", true).await;
    let second = sb.repo("second", true).await;
    for repo in [&first, &second] {
        sb.git(repo, &["remote", "add", "origin", &url]).await;
    }
    sb.stage(&first, "a.txt", "first\n").await;
    commit(&sb.cli, &first, &request("first")).await.expect("commit");
    push(&sb.cli, &first, &upstream_push(), None).await.expect("first push");

    sb.stage(&second, "a.txt", "second\n").await;
    commit(&sb.cli, &second, &request("second")).await.expect("commit");
    let err = push(&sb.cli, &second, &upstream_push(), None)
        .await
        .expect_err("diverged");
    assert!(
        matches!(&err, ScmError::PushRejected { message } if message.contains("rejected")),
        "{err:?}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn remote_hook_decline_is_rejected() {
    let sb = Sandbox::new();
    let bare = sb.bare("remote.git").await;
    write_hook(
        &bare.join("hooks"),
        "pre-receive",
        "#!/bin/sh\necho 'protected branch' >&2\nexit 1\n",
    );
    let repo = sb.repo("work", true).await;
    sb.git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()])
        .await;
    sb.stage(&repo, "a.txt", "one\n").await;
    commit(&sb.cli, &repo, &request("one")).await.expect("commit");

    let err = push(&sb.cli, &repo, &upstream_push(), None)
        .await
        .expect_err("declined");
    assert!(
        matches!(&err, ScmError::PushRejected { message } if message.contains("protected branch")),
        "{err:?}"
    );
}

/// An HTTP remote that answers every request with `401`, so git (prompts
/// disabled, no credential helper) fails authentication.
fn unauthorized_server() -> u16 {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let mut buf = [0u8; 4096];
            let _ = stream.read(&mut buf);
            let _ = stream.write_all(
                b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"e44\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            );
        }
    });
    port
}

#[tokio::test]
async fn http_401_maps_to_auth_failed_and_never_echoes_credentials() {
    let sb = Sandbox::new();
    let port = unauthorized_server();
    let repo = sb.repo("work", true).await;
    let url = format!("http://e44user:sekrit-pass@127.0.0.1:{port}/repo.git");
    sb.git(&repo, &["remote", "add", "origin", &url]).await;
    sb.stage(&repo, "a.txt", "one\n").await;
    commit(&sb.cli, &repo, &request("one")).await.expect("commit");

    let info = remote_info(&sb.cli, &repo).await.expect("remote info");
    assert_eq!(
        info.remotes[0].url_redacted,
        format!("http://127.0.0.1:{port}/repo.git")
    );
    assert_eq!(info.remotes[0].provider, "other");

    let err = push(&sb.cli, &repo, &upstream_push(), None).await.expect_err("401");
    let ScmError::PushAuthFailed { message } = &err else {
        panic!("expected PUSH_AUTH_FAILED, got {err:?}");
    };
    assert!(!message.contains("sekrit-pass"), "{message}");

    // Without credentials in the URL the prompt is what fails.
    sb.git(
        &repo,
        &[
            "remote",
            "set-url",
            "origin",
            &format!("http://127.0.0.1:{port}/repo.git"),
        ],
    )
    .await;
    let err = push(&sb.cli, &repo, &upstream_push(), None).await.expect_err("401");
    assert!(matches!(err, ScmError::PushAuthFailed { .. }), "{err:?}");
}

#[tokio::test]
async fn push_refuses_detached_head_and_bad_remotes() {
    let sb = Sandbox::new();
    let bare = sb.bare("remote.git").await;
    let repo = sb.repo("work", true).await;
    sb.git(&repo, &["remote", "add", "origin", &bare.to_string_lossy()])
        .await;
    sb.stage(&repo, "a.txt", "one\n").await;
    commit(&sb.cli, &repo, &request("one")).await.expect("commit");

    for bad in ["--upload-pack=touch /tmp/x", "-x", "../elsewhere", "a b"] {
        let req = PushRequest {
            remote: Some(bad.to_owned()),
            set_upstream: true,
        };
        let err = push(&sb.cli, &repo, &req, None).await.expect_err("bad remote");
        assert!(
            matches!(err, ScmError::InvalidParams { what: "remote" }),
            "{bad}: {err:?}"
        );
    }
    let missing = PushRequest {
        remote: Some("upstream".into()),
        set_upstream: true,
    };
    assert!(matches!(
        push(&sb.cli, &repo, &missing, None).await,
        Err(ScmError::OperationFailed { context: "push", .. })
    ));

    sb.git(&repo, &["checkout", "-q", "--detach"]).await;
    let err = push(&sb.cli, &repo, &upstream_push(), None)
        .await
        .expect_err("detached");
    assert!(
        matches!(err, ScmError::OperationFailed { context: "push", .. }),
        "{err:?}"
    );
}

#[tokio::test]
async fn missing_git_is_git_not_available() {
    let sb = Sandbox::new();
    let repo = sb.repo("work", true).await;
    sb.stage(&repo, "a.txt", "one\n").await;
    let missing = GitCli::new(sb.root.join("no-such-git"));
    let err = commit(&missing, &repo, &request("x")).await.expect_err("no git");
    assert!(matches!(err, ScmError::GitNotAvailable { .. }), "{err:?}");
    let err = push(&missing, &repo, &upstream_push(), None).await.expect_err("no git");
    assert!(matches!(err, ScmError::GitNotAvailable { .. }), "{err:?}");
}

// ── pure helpers ───────────────────────────────────────────────────────

#[test]
fn push_stderr_is_classified() {
    let cases = [
        (
            "To github.com:o/r.git\n ! [rejected]        main -> main (fetch first)\nerror: failed to push some refs",
            "rejected",
        ),
        (
            " ! [remote rejected] main -> main (protected branch hook declined)",
            "rejected",
        ),
        (
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
            "auth",
        ),
        (
            "remote: Permission to o/r.git denied to u.\nfatal: unable to access 'https://github.com/o/r.git/': The requested URL returned error: 403",
            "auth",
        ),
        (
            "git@github.com: Permission denied (publickey).\nfatal: Could not read from remote repository.",
            "auth",
        ),
        ("fatal: Authentication failed for 'https://gitlab.com/o/r.git/'", "auth"),
        (
            "fatal: unable to access 'https://x/': Could not resolve host: x",
            "other",
        ),
    ];
    for (stderr, want) in cases {
        let got = match classify_push_failure(stderr) {
            ScmError::PushRejected { .. } => "rejected",
            ScmError::PushAuthFailed { .. } => "auth",
            ScmError::OperationFailed { context: "push", .. } => "other",
            other => panic!("unexpected {other:?}"),
        };
        assert_eq!(got, want, "{stderr}");
    }
}

#[test]
fn commit_failures_are_classified() {
    let out = |stdout: &str, stderr: &str| GitOutput {
        code: Some(1),
        stdout: stdout.into(),
        stderr: stderr.into(),
    };
    assert!(matches!(
        commit_failure(&out("", "*** Please tell me who you are."), true),
        ScmError::CommitIdentityMissing { .. }
    ));
    assert!(matches!(
        commit_failure(&out("lint failed", ""), true),
        ScmError::CommitHookFailed { output } if output == "lint failed"
    ));
    assert!(matches!(
        commit_failure(&out("", "fatal: unable to write new index file"), false),
        ScmError::OperationFailed { context: "commit", .. }
    ));
}

#[test]
fn secrets_are_redacted() {
    assert_eq!(
        redact_url("https://user:ghp_abcdefghijklmnopqrstuvwxyz@github.com/o/r.git"),
        "https://github.com/o/r.git"
    );
    assert_eq!(
        redact_url("https://oauth2:tok@gitlab.com/o/r.git"),
        "https://gitlab.com/o/r.git"
    );
    assert_eq!(
        redact_url("git@github.com:o/r.git"),
        "git@github.com:o/r.git",
        "scp form has no secret"
    );
    assert_eq!(redact_url("/srv/git/r.git"), "/srv/git/r.git");
    let text = redact(
        "push to https://x:y@h/r failed: ghp_0123456789abcdefghijABCD github_pat_11ABCDEFG0123456789_abcdef glpat-abcdefghij0123456789 xoxb-1234567890-abc Authorization: Bearer abc.def.ghijklmnop",
    );
    for secret in [
        "x:y@",
        "ghp_0123",
        "github_pat_11",
        "glpat-abc",
        "xoxb-123",
        "abc.def.ghij",
    ] {
        assert!(!text.contains(secret), "{secret} survived: {text}");
    }
    assert!(text.contains("https://h/r"), "{text}");
}

#[test]
fn long_output_keeps_its_tail() {
    let long = format!("{}THE VERDICT", "x".repeat(MAX_OUTPUT_CHARS * 2));
    let clipped = clip(&long);
    assert!(clipped.ends_with("THE VERDICT"));
    assert!(clipped.chars().count() <= MAX_OUTPUT_CHARS + 2);
}

#[test]
fn remote_names_and_providers() {
    for ok in ["origin", "upstream", "my-fork", "fork_2", "a.b"] {
        assert!(is_valid_remote_name(ok), "{ok}");
    }
    for bad in ["", "-o", "--upload-pack=x", ".", "..", "a..b", "a b", "a/b", "a:b", "ä"] {
        assert!(!is_valid_remote_name(bad), "{bad}");
    }
    assert_eq!(remote_provider("git@github.com:o/r.git"), "github");
    assert_eq!(remote_provider("https://github.com/o/r"), "github");
    assert_eq!(remote_provider("https://gitlab.example.com/o/r"), "gitlab");
    assert_eq!(remote_provider("/srv/r.git"), "local");
    assert_eq!(remote_provider(r"C:\repos\r.git"), "local");
    assert_eq!(remote_provider("file:///srv/r.git"), "local");
    assert_eq!(remote_provider("https://bitbucket.org/o/r"), "other");
}

#[test]
fn progress_lines_are_parsed() {
    let parse = |l: &str| parse_progress_line(l).map(|p| (p.phase, p.percent));
    assert_eq!(
        parse("Writing objects:  40% (2/5)"),
        Some(("Writing objects".into(), Some(40)))
    );
    assert_eq!(
        parse("Enumerating objects: 5, done."),
        Some(("Enumerating objects".into(), None))
    );
    assert_eq!(parse("remote: Resolving deltas: 100% (1/1)"), None);
    assert_eq!(parse("To /srv/r.git"), None);
    assert_eq!(parse(" * [new branch]      main -> main"), None);
    assert_eq!(parse("branch 'main' set up to track 'origin/main'."), None);
}

#[test]
fn progress_steps_are_deduplicated() {
    let seen: Arc<Mutex<Vec<ScmProgress>>> = Arc::default();
    let sink = {
        let seen = Arc::clone(&seen);
        Arc::new(move |s: ScmProgress| seen.lock().unwrap().push(s)) as ScmProgressFn
    };
    let adapter = progress_adapter(sink);
    for line in [
        "Writing objects:  50% (1/2)",
        "Writing objects:  50% (1/2)",
        "Writing objects: 100% (2/2), done.",
    ] {
        adapter(line);
    }
    let percents: Vec<_> = seen.lock().unwrap().iter().map(|s| s.percent).collect();
    assert_eq!(percents, vec![Some(50), Some(100)]);
}
