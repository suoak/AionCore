//! `gh` / `glab` invocation for PR delivery (design §7.6).
//!
//! Only `auth status`, `pr create` and `mr create` are ever run. Arguments are
//! an argv array; the child gets the same scrubbed environment as the git
//! runner ([`aionui_worktree::git_cli::scrub_env`]: no `AIONUI_*`, runtime
//! tokens or local secrets) plus non-interactive switches, so a CLI can never
//! block on a prompt. WorkMate never reads the CLI's stored token.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::LazyLock;
use std::time::Duration;

use aionui_worktree::git_cli::scrub_env;
use regex::Regex;
use serde::Serialize;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

/// Timeout for `auth status`.
pub const AUTH_STATUS_TIMEOUT: Duration = Duration::from_secs(20);
/// Timeout for `pr create` / `mr create`.
pub const CREATE_TIMEOUT: Duration = Duration::from_secs(2 * 60);
/// Client-visible output is capped (tail kept).
const MAX_OUTPUT_CHARS: usize = 8 * 1024;

/// Hosting provider of a remote.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostProvider {
    Github,
    Gitlab,
    Other,
}

impl HostProvider {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Other => "other",
        }
    }

    /// The CLI that serves this provider.
    pub fn cli_name(self) -> Option<&'static str> {
        match self {
            Self::Github => Some("gh"),
            Self::Gitlab => Some("glab"),
            Self::Other => None,
        }
    }
}

/// Host name of a remote URL (`https://h/…`, `ssh://user@h:port/…`,
/// scp-like `user@h:path`). `None` for local paths.
pub fn remote_host(url: &str) -> Option<String> {
    let url = url.trim();
    if let Some((_, rest)) = url.split_once("://") {
        let authority = rest.split('/').next().unwrap_or_default();
        let host_port = authority.rsplit('@').next().unwrap_or_default();
        let host = host_port.split(':').next().unwrap_or_default();
        return (!host.is_empty() && !url.to_ascii_lowercase().starts_with("file://"))
            .then(|| host.to_ascii_lowercase());
    }
    // scp-like: [user@]host:path, where host has no '/' and the ':' is not a
    // Windows drive letter.
    let (head, _) = url.split_once(':')?;
    if head.contains('/') || head.contains('\\') || head.len() < 2 {
        return None;
    }
    let host = head.rsplit('@').next().unwrap_or_default();
    (!host.is_empty()).then(|| host.to_ascii_lowercase())
}

/// Provider by host name.
pub fn provider_for_host(host: &str) -> HostProvider {
    let host = host.to_ascii_lowercase();
    if host == "github.com" || host.ends_with(".github.com") || host.starts_with("github.") {
        HostProvider::Github
    } else if host.contains("gitlab") {
        HostProvider::Gitlab
    } else {
        HostProvider::Other
    }
}

/// Inputs for one create call (already validated by the service).
#[derive(Debug, Clone)]
pub struct PrCreateArgs<'a> {
    pub branch: &'a str,
    pub base: &'a str,
    pub title: &'a str,
    pub body: &'a str,
    pub draft: bool,
}

/// argv (without the program) and stdin for `gh pr create` / `glab mr create`.
///
/// `gh` reads the body from stdin (`--body-file -`). `glab mr create` has no
/// stdin body option (`-d -` opens an editor), so the description goes in the
/// argv array; the service caps its size.
pub fn create_argv(provider: HostProvider, args: &PrCreateArgs<'_>) -> Option<(Vec<String>, Option<Vec<u8>>)> {
    let own = |s: &str| s.to_owned();
    match provider {
        HostProvider::Github => {
            let mut argv: Vec<String> = ["pr", "create", "--head", args.branch, "--base", args.base, "--title"]
                .into_iter()
                .map(own)
                .collect();
            argv.push(args.title.to_owned());
            argv.extend(["--body-file", "-"].map(own));
            if args.draft {
                argv.push("--draft".into());
            }
            Some((argv, Some(args.body.as_bytes().to_vec())))
        }
        HostProvider::Gitlab => {
            let mut argv: Vec<String> = [
                "mr",
                "create",
                "--source-branch",
                args.branch,
                "--target-branch",
                args.base,
                "--title",
            ]
            .into_iter()
            .map(own)
            .collect();
            argv.push(args.title.to_owned());
            argv.push("--description".into());
            argv.push(args.body.to_owned());
            argv.push("--yes".into());
            if args.draft {
                argv.push("--draft".into());
            }
            Some((argv, None))
        }
        HostProvider::Other => None,
    }
}

/// argv for `auth status` against one host.
pub fn auth_status_argv(host: &str) -> Vec<String> {
    vec!["auth".into(), "status".into(), "--hostname".into(), host.to_owned()]
}

static PR_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"https?://[^\s]+/(?:pull|-/merge_requests|merge_requests)/(\d+)").expect("pr url regex")
});

/// The PR / MR URL from CLI output (the last match wins; `gh` prints it as
/// the final stdout line, `glab` after a summary).
pub fn parse_pr_url(output: &str) -> Option<(String, String)> {
    PR_URL
        .captures_iter(output)
        .last()
        .map(|c| (c[0].trim_end_matches(['.', ',', ')']).to_owned(), c[1].to_owned()))
}

static URL_USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\b([a-z][a-z0-9+.\-]*://)[^/\s@]+@").expect("userinfo regex"));

static TOKENS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"\b(?:gh[pousr]_[A-Za-z0-9]{20,}",
        r"|github_pat_[A-Za-z0-9_]{20,}",
        r"|glpat-[A-Za-z0-9_\-]{20,}",
        r")",
        r"|(?i:\b(?:bearer|token:?)\s+[A-Za-z0-9._~+/=\-]{12,})",
    ))
    .expect("token regex")
});

/// Remove URL credentials and token-shaped strings, cap the length.
pub fn redact_output(text: &str) -> String {
    let stripped = URL_USERINFO.replace_all(text, "$1");
    let redacted = TOKENS.replace_all(&stripped, "[REDACTED]");
    let count = redacted.chars().count();
    if count <= MAX_OUTPUT_CHARS {
        return redacted.trim().to_owned();
    }
    let tail: String = redacted.chars().skip(count - MAX_OUTPUT_CHARS).collect();
    format!("…\n{}", tail.trim())
}

/// Result of one CLI run.
#[derive(Debug, Clone)]
pub struct CliOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CliOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }

    pub fn combined(&self) -> String {
        match (self.stdout.trim(), self.stderr.trim()) {
            ("", err) => err.to_owned(),
            (out, "") => out.to_owned(),
            (out, err) => format!("{out}\n{err}"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CliRunError {
    #[error("{0} is not installed")]
    NotInstalled(String),
    #[error("{0} timed out")]
    Timeout(String),
    #[error("failed to run {program}: {message}")]
    Io { program: String, message: String },
}

/// Runner for one hosting CLI (`gh` or `glab`).
#[derive(Debug, Clone)]
pub struct HostCli {
    program: PathBuf,
    base_env: Option<Vec<(OsString, OsString)>>,
}

impl HostCli {
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            base_env: None,
        }
    }

    /// Replace the inherited parent environment (tests).
    pub fn with_base_env(mut self, env: Vec<(OsString, OsString)>) -> Self {
        self.base_env = Some(env);
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    fn display_name(&self) -> String {
        self.program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "cli".into())
    }

    pub async fn run(
        &self,
        argv: &[String],
        cwd: &Path,
        stdin: Option<Vec<u8>>,
        timeout: Duration,
    ) -> Result<CliOutput, CliRunError> {
        let name = self.display_name();
        let base: Vec<(OsString, OsString)> = match &self.base_env {
            Some(env) => env.clone(),
            None => std::env::vars_os().collect(),
        };
        let mut cmd = Command::new(&self.program);
        cmd.args(argv)
            .current_dir(cwd)
            .env_clear()
            .envs(scrub_env(base))
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GCM_INTERACTIVE", "never")
            .env("GH_PROMPT_DISABLED", "1")
            .env("GH_NO_UPDATE_NOTIFIER", "1")
            .env("GLAB_NO_PROMPT", "1")
            .env("NO_PROMPT", "1")
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(0x0800_0000);
        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CliRunError::NotInstalled(name.clone())
            } else {
                CliRunError::Io {
                    program: name.clone(),
                    message: e.to_string(),
                }
            }
        })?;
        if let (Some(input), Some(mut pipe)) = (stdin, child.stdin.take()) {
            // A CLI that exits before reading stdin closes the pipe; its exit
            // status is what matters, so a write error is ignored.
            let _ = pipe.write_all(&input).await;
            drop(pipe);
        }
        match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(out)) => Ok(CliOutput {
                code: out.status.code(),
                stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            }),
            Ok(Err(e)) => Err(CliRunError::Io {
                program: name,
                message: e.to_string(),
            }),
            // Dropping the future kills the child (`kill_on_drop`).
            Err(_) => Err(CliRunError::Timeout(name)),
        }
    }
}

#[cfg(test)]
#[path = "delivery_cli_test.rs"]
mod tests;
