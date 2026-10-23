//! Commit, push and remote info for git through the system `git` CLI (M7 E4-4).
//!
//! Reads stay on git2 (`git_provider.rs`); these three go through the CLI so a
//! commit runs the repository's hooks and signing config, and a push uses the
//! user's credential helper / ssh agent — exactly what the same command does in
//! a terminal. The shared [`GitCli`] runner scrubs WorkMate secrets
//! (`AIONUI_*`, runtime tokens) from the child environment, disables
//! interactive prompts and passes arguments as an array, never through a shell.
//!
//! Everything that leaves this module as text — hook output, push errors,
//! remote URLs — is redacted ([`redact`]) and length-capped first.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use aionui_worktree::GitCli;
use aionui_worktree::git_cli::{DEFAULT_TIMEOUT, FETCH_TIMEOUT, GitCliError, GitOutput, RunOptions};
use async_trait::async_trait;
use regex::Regex;

use super::commit_types::{
    CommitRequest, CommitResult, PushRequest, PushResult, RemoteInfo, ScmProgress, ScmProgressFn, ScmRemote,
};
use super::error::ScmError;
use super::git_provider::GitScmProvider;
use super::provider::{IScmCommit, IScmRemote};
use super::types::RepoRef;

/// Largest accepted commit message.
const MAX_MESSAGE_BYTES: usize = 64 * 1024;

/// Largest hook output / error text returned to the client. The tail is kept:
/// a hook's verdict is usually its last lines.
const MAX_OUTPUT_CHARS: usize = 16 * 1024;

/// Hooks `git commit` runs that can stop it. `post-commit` cannot, so it does
/// not make a failure a hook failure.
const COMMIT_HOOKS: &[&str] = &["pre-commit", "prepare-commit-msg", "commit-msg"];

/// Hooks may run linters and test suites; give them the long budget.
const COMMIT_TIMEOUT: Duration = FETCH_TIMEOUT;

#[async_trait]
impl IScmCommit for GitScmProvider {
    async fn commit(&self, repo: &RepoRef, request: &CommitRequest) -> Result<CommitResult, ScmError> {
        let workdir = self.workdir_of(repo)?;
        commit(self.git(), &workdir, request).await
    }
}

#[async_trait]
impl IScmRemote for GitScmProvider {
    async fn push(
        &self,
        repo: &RepoRef,
        request: &PushRequest,
        progress: Option<ScmProgressFn>,
    ) -> Result<PushResult, ScmError> {
        let workdir = self.workdir_of(repo)?;
        push(self.git(), &workdir, request, progress).await
    }

    async fn remote_info(&self, repo: &RepoRef) -> Result<RemoteInfo, ScmError> {
        let workdir = self.workdir_of(repo)?;
        remote_info(self.git(), &workdir).await
    }
}

// ── commit ──────────────────────────────────────────────────────────────

pub(super) async fn commit(cli: &GitCli, workdir: &Path, request: &CommitRequest) -> Result<CommitResult, ScmError> {
    if request.amend {
        return Err(ScmError::CapabilityUnsupported { capability: "amend" });
    }
    let message = request.message.as_str();
    if message.trim().is_empty() || message.len() > MAX_MESSAGE_BYTES || message.contains('\0') {
        return Err(ScmError::InvalidParams { what: "message" });
    }
    let git = Git::new(cli, workdir);

    // Exactly what is staged; "stage everything first" is the client's
    // explicit, separate step. `diff --cached` also works on an unborn branch.
    let staged = git
        .run("commit", &["diff", "--cached", "--quiet", "--no-ext-diff"])
        .await?;
    match staged.code {
        Some(1) => {}
        Some(0) => return Err(ScmError::CommitNothingStaged),
        _ => return Err(failed("commit", &staged)),
    }

    // Identity before hooks: a hook that runs for a minute and then a refusal
    // over a missing e-mail address is the worst order. `useConfigOnly` makes
    // git refuse to guess `user@hostname` — an identity nobody chose.
    for var in ["GIT_AUTHOR_IDENT", "GIT_COMMITTER_IDENT"] {
        let ident = git
            .run("commit", &["-c", "user.useConfigOnly=true", "var", var])
            .await?;
        if !ident.success() {
            return Err(ScmError::CommitIdentityMissing {
                message: clip(&redact(ident.stderr.trim())),
            });
        }
    }

    let hooks = if request.no_verify {
        Vec::new()
    } else {
        commit_hooks(&git).await
    };
    let mut args = vec!["-c", "user.useConfigOnly=true", "commit", "-F", "-"];
    if request.signoff {
        args.push("--signoff");
    }
    if request.no_verify {
        args.push("--no-verify");
    }
    // Hooks keep the user's locale: their output is shown verbatim.
    let opts = git
        .base()
        .with_timeout(COMMIT_TIMEOUT)
        .with_stdin(message.as_bytes().to_vec());
    let out = git.run_with("commit", &args, opts).await?;
    if !out.success() {
        return Err(commit_failure(&out, !hooks.is_empty()));
    }

    let head = git.run("commit", &["rev-parse", "HEAD"]).await?;
    if !head.success() {
        return Err(failed("commit", &head));
    }
    Ok(CommitResult {
        commit_sha: head.stdout.trim().to_owned(),
    })
}

/// Classify a failed `git commit`. With commit hooks installed (and not
/// skipped) a failure is reported as the hook's, carrying its full output —
/// that is the only place the user learns what the hook objected to.
pub(super) fn commit_failure(out: &GitOutput, hooks_ran: bool) -> ScmError {
    let combined = combine(&out.stdout, &out.stderr);
    if is_identity_failure(&out.stderr) {
        return ScmError::CommitIdentityMissing {
            message: clip(&redact(out.stderr.trim())),
        };
    }
    if hooks_ran {
        return ScmError::CommitHookFailed {
            output: clip(&redact(&combined)),
        };
    }
    ScmError::OperationFailed {
        context: "commit",
        message: clip(&redact(&combined)),
    }
}

fn is_identity_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "please tell me who you are",
        "empty ident name",
        "unable to auto-detect email address",
        "no email was given and auto-detection is disabled",
        "no name was given and auto-detection is disabled",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

/// Installed commit hooks, honouring `core.hooksPath`.
async fn commit_hooks(git: &Git<'_>) -> Vec<String> {
    let Ok(out) = git.run("commit", &["rev-parse", "--git-path", "hooks"]).await else {
        return Vec::new();
    };
    if !out.success() {
        return Vec::new();
    }
    let dir = PathBuf::from(out.stdout.trim());
    let dir = if dir.is_absolute() { dir } else { git.workdir.join(dir) };
    COMMIT_HOOKS
        .iter()
        .filter(|name| is_runnable(&dir.join(name)))
        .map(|name| (*name).to_owned())
        .collect()
}

/// Whether git would run this hook: a file, and on unix an executable one
/// (git skips a non-executable hook with a warning).
fn is_runnable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

// ── push ────────────────────────────────────────────────────────────────

pub(super) async fn push(
    cli: &GitCli,
    workdir: &Path,
    request: &PushRequest,
    progress: Option<ScmProgressFn>,
) -> Result<PushResult, ScmError> {
    let git = Git::new(cli, workdir);
    let branch = current_branch(&git).await?.ok_or(ScmError::OperationFailed {
        context: "push",
        message: "HEAD is detached; check out a branch to push".into(),
    })?;
    let tracking = tracking_of(&git, &branch).await?;

    let remote = match (&request.remote, &tracking) {
        (Some(remote), _) => remote.clone(),
        (None, Some((remote, _))) => remote.clone(),
        (None, None) => "origin".to_owned(),
    };
    if !is_valid_remote_name(&remote) {
        return Err(ScmError::InvalidParams { what: "remote" });
    }
    if !remote_names(&git).await?.contains(&remote) {
        return Err(ScmError::OperationFailed {
            context: "push",
            message: format!("remote '{remote}' is not configured"),
        });
    }

    // The current branch only, to its upstream on that remote — or, when it has
    // none and the user confirmed, to the same name with the upstream set.
    let existing = tracking.filter(|(r, _)| *r == remote).map(|(_, merge)| merge);
    let (destination, set_upstream) = match existing {
        Some(merge) => (merge, false),
        None if request.set_upstream => (format!("refs/heads/{branch}"), true),
        None => return Err(ScmError::PushNoUpstream { remote, branch }),
    };
    let refspec = format!("refs/heads/{branch}:{destination}");
    let mut args = vec!["push", "--progress"];
    if set_upstream {
        args.push("--set-upstream");
    }
    // Never `--force`: a rejected push is reported, not overridden.
    args.extend(["--", remote.as_str(), refspec.as_str()]);

    let mut opts = git.classified().with_timeout(FETCH_TIMEOUT);
    if let Some(sink) = progress {
        opts = opts.with_progress(progress_adapter(sink));
    }
    let out = git.run_with("push", &args, opts).await?;
    if !out.success() {
        return Err(classify_push_failure(&out.stderr));
    }

    let short = destination.strip_prefix("refs/heads/").unwrap_or(&destination);
    let (ahead, _) = ahead_behind(&git).await.unwrap_or((0, 0));
    Ok(PushResult {
        upstream: format!("{remote}/{short}"),
        remote,
        branch,
        ahead,
    })
}

/// Map a failed push's stderr (C locale) to the error the UI acts on.
/// Authentication first: a 403 page or `Permission denied (publickey)` also
/// ends in "failed to push some refs".
pub(super) fn classify_push_failure(stderr: &str) -> ScmError {
    let lower = stderr.to_ascii_lowercase();
    let message = clip(&redact(stderr.trim()));
    const AUTH: &[&str] = &[
        "authentication failed",
        "could not read username",
        "could not read password",
        "terminal prompts disabled",
        "permission denied",
        "invalid username or password",
        "invalid credentials",
        "access denied",
        "returned error: 401",
        "returned error: 403",
        "host key verification failed",
    ];
    const REJECTED: &[&str] = &[
        "[rejected]",
        "[remote rejected]",
        "non-fast-forward",
        "fetch first",
        "failed to push some refs",
    ];
    if AUTH.iter().any(|p| lower.contains(p)) {
        ScmError::PushAuthFailed { message }
    } else if REJECTED.iter().any(|p| lower.contains(p)) {
        ScmError::PushRejected { message }
    } else {
        ScmError::OperationFailed {
            context: "push",
            message,
        }
    }
}

/// Turn git's stderr progress lines (`Writing objects:  40% (2/5)`) into
/// deduplicated [`ScmProgress`] steps. Remote chatter (`remote: ...`), ref
/// summaries and anything without a percentage or `done` are dropped.
fn progress_adapter(sink: ScmProgressFn) -> aionui_worktree::git_cli::ProgressFn {
    let last: Mutex<Option<ScmProgress>> = Mutex::new(None);
    Arc::new(move |line: &str| {
        let Some(step) = parse_progress_line(line) else {
            return;
        };
        let mut last = last.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.as_ref() == Some(&step) {
            return;
        }
        *last = Some(step.clone());
        drop(last);
        sink(step);
    })
}

pub(super) fn parse_progress_line(line: &str) -> Option<ScmProgress> {
    static PHASE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"^[A-Z][a-z]+(?: [a-z]+){0,3}$").expect("phase regex"));
    let (phase, rest) = line.trim().split_once(':')?;
    if !PHASE.is_match(phase) {
        return None;
    }
    let percent = aionui_worktree::git_cli::parse_progress_percent(rest);
    if percent.is_none() && !rest.contains("done") {
        return None;
    }
    Some(ScmProgress {
        phase: phase.to_owned(),
        percent,
    })
}

// ── remote info ─────────────────────────────────────────────────────────

pub(super) async fn remote_info(cli: &GitCli, workdir: &Path) -> Result<RemoteInfo, ScmError> {
    let git = Git::new(cli, workdir);
    let mut remotes = Vec::new();
    let urls = git
        .run("remote", &["config", "--get-regexp", r"^remote\..*\.url$"])
        .await?;
    // Exit 1 with no output: no remotes configured.
    if urls.success() {
        for line in urls.stdout.lines() {
            let Some((key, url)) = line.split_once(' ') else {
                continue;
            };
            let Some(name) = key.strip_prefix("remote.").and_then(|k| k.strip_suffix(".url")) else {
                continue;
            };
            if remotes.iter().any(|r: &ScmRemote| r.name == name) {
                continue; // multiple urls: the first is the fetch url
            }
            remotes.push(ScmRemote {
                name: name.to_owned(),
                url_redacted: redact_url(url.trim()),
                provider: remote_provider(url.trim()),
            });
        }
    }

    let branch = current_branch(&git).await?;
    let upstream = match &branch {
        Some(_) => {
            let out = git
                .run(
                    "remote",
                    &["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{upstream}"],
                )
                .await?;
            out.success()
                .then(|| out.stdout.trim().to_owned())
                .filter(|u| !u.is_empty())
        }
        None => None,
    };
    let (ahead, behind) = match &upstream {
        Some(_) => match ahead_behind(&git).await {
            Some((a, b)) => (Some(a), Some(b)),
            None => (None, None),
        },
        None => (None, None),
    };
    Ok(RemoteInfo {
        remotes,
        branch,
        upstream,
        ahead,
        behind,
        commit_hooks: commit_hooks(&git).await,
    })
}

/// `github` / `gitlab` / `local` / `other`, from the URL's host.
pub(super) fn remote_provider(url: &str) -> &'static str {
    let lower = url.to_ascii_lowercase();
    let local = lower.starts_with('/')
        || lower.starts_with("file://")
        || lower.starts_with("./")
        || lower.starts_with("../")
        || lower.starts_with('\\')
        || (lower.len() > 2 && lower.as_bytes()[1] == b':' && matches!(lower.as_bytes()[2], b'\\' | b'/'));
    if local {
        "local"
    } else if lower.contains("github.com") || lower.contains("github.") {
        "github"
    } else if lower.contains("gitlab") {
        "gitlab"
    } else {
        "other"
    }
}

// ── shared reads ────────────────────────────────────────────────────────

/// Current branch name; `None` when HEAD is detached.
async fn current_branch(git: &Git<'_>) -> Result<Option<String>, ScmError> {
    let out = git
        .run("remote", &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .await?;
    Ok(out
        .success()
        .then(|| out.stdout.trim().to_owned())
        .filter(|b| !b.is_empty()))
}

/// The branch's configured upstream as `(remote, merge ref)`. A local
/// upstream (`remote = .`) is not a publishing target and reads as none.
async fn tracking_of(git: &Git<'_>, branch: &str) -> Result<Option<(String, String)>, ScmError> {
    let remote_key = format!("branch.{branch}.remote");
    let merge_key = format!("branch.{branch}.merge");
    let remote = git.run("push", &["config", "--get", &remote_key]).await?;
    let merge = git.run("push", &["config", "--get", &merge_key]).await?;
    if !remote.success() || !merge.success() {
        return Ok(None);
    }
    let remote = remote.stdout.trim().to_owned();
    let merge = merge.stdout.trim().to_owned();
    if remote == "." || !is_valid_remote_name(&remote) || !merge.starts_with("refs/heads/") {
        return Ok(None);
    }
    Ok(Some((remote, merge)))
}

async fn remote_names(git: &Git<'_>) -> Result<Vec<String>, ScmError> {
    let out = git.run("push", &["remote"]).await?;
    if !out.success() {
        return Err(failed("push", &out));
    }
    Ok(out
        .stdout
        .lines()
        .map(|l| l.trim().to_owned())
        .filter(|l| !l.is_empty())
        .collect())
}

/// `(ahead, behind)` of HEAD against its upstream, from local refs only.
async fn ahead_behind(git: &Git<'_>) -> Option<(u32, u32)> {
    let out = git
        .run("remote", &["rev-list", "--left-right", "--count", "HEAD...@{upstream}"])
        .await
        .ok()?;
    if !out.success() {
        return None;
    }
    let mut parts = out.stdout.split_whitespace();
    let ahead = parts.next()?.parse().ok()?;
    let behind = parts.next()?.parse().ok()?;
    Some((ahead, behind))
}

/// A remote name a user could have created: no option injection, no path or
/// refspec syntax.
pub(super) fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('-')
        && !name.starts_with('.')
        && !name.contains("..")
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

// ── redaction ───────────────────────────────────────────────────────────

static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^/\s@]+@").expect("userinfo regex"));

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}",
        r"|github_pat_[A-Za-z0-9_]{20,}",
        r"|glpat-[A-Za-z0-9_\-]{20,}",
        r"|xox[abposr]-[A-Za-z0-9\-]{10,}",
        r")",
        r"|(?i:\bbearer\s+[A-Za-z0-9._~+/=\-]{12,})",
    ))
    .expect("token regex")
});

/// Remove credentials from text bound for the client: `user:pass@` in URLs and
/// token-shaped strings (GitHub / GitLab / Slack tokens, `Bearer ...`).
pub(super) fn redact(text: &str) -> String {
    let stripped = URL_USERINFO.replace_all(text, "$1");
    TOKENS.replace_all(&stripped, "[REDACTED]").into_owned()
}

/// A remote URL for display: credentials removed.
pub(super) fn redact_url(url: &str) -> String {
    redact(url)
}

/// Cap at [`MAX_OUTPUT_CHARS`], keeping the tail.
fn clip(text: &str) -> String {
    let count = text.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return text.to_owned();
    }
    let tail: String = text.chars().skip(count - MAX_OUTPUT_CHARS).collect();
    format!("…\n{tail}")
}

fn combine(stdout: &str, stderr: &str) -> String {
    match (stdout.trim(), stderr.trim()) {
        ("", err) => err.to_owned(),
        (out, "") => out.to_owned(),
        (out, err) => format!("{out}\n{err}"),
    }
}

fn failed(context: &'static str, out: &GitOutput) -> ScmError {
    ScmError::OperationFailed {
        context,
        message: clip(&redact(&combine(&out.stdout, &out.stderr))),
    }
}

// ── runner ──────────────────────────────────────────────────────────────

/// [`GitCli`] bound to one work tree.
struct Git<'a> {
    cli: &'a GitCli,
    workdir: &'a Path,
}

impl<'a> Git<'a> {
    fn new(cli: &'a GitCli, workdir: &'a Path) -> Self {
        Self { cli, workdir }
    }

    fn base(&self) -> RunOptions {
        RunOptions::in_dir(self.workdir).with_timeout(DEFAULT_TIMEOUT)
    }

    /// Options for a command whose stderr is classified: C locale, so the
    /// patterns above match whatever language the user runs git in.
    fn classified(&self) -> RunOptions {
        self.base().with_env("LC_ALL", "C")
    }

    async fn run(&self, context: &'static str, args: &[&str]) -> Result<GitOutput, ScmError> {
        self.run_with(context, args, self.classified()).await
    }

    async fn run_with(&self, context: &'static str, args: &[&str], opts: RunOptions) -> Result<GitOutput, ScmError> {
        self.cli
            .run(args.iter().copied(), opts)
            .await
            .map_err(|err| cli_error(context, err))
    }
}

pub(super) fn cli_error(context: &'static str, err: GitCliError) -> ScmError {
    match err {
        GitCliError::NotAvailable(_) | GitCliError::TooOld { .. } | GitCliError::UnknownVersion(_) => {
            ScmError::GitNotAvailable {
                message: err.to_string(),
            }
        }
        other => ScmError::OperationFailed {
            context,
            message: clip(&redact(&other.to_string())),
        },
    }
}

#[cfg(test)]
#[path = "commit_test.rs"]
mod commit_test;
