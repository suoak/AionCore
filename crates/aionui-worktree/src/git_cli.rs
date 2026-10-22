//! System `git` runner for worktree write operations.
//!
//! Every invocation:
//! - passes arguments as an argv array (never a shell string);
//! - starts from a scrubbed copy of the parent environment: all `AIONUI_*`,
//!   `AIONCORE_*`, `AION_MCP_*`, `CSBU_WORKMATE_*` variables, local auth
//!   secrets, runtime tokens and repository-redirecting `GIT_*` variables are
//!   dropped, then `GIT_TERMINAL_PROMPT=0` and `GCM_INTERACTIVE=never` are set
//!   so git can never block on an interactive credential prompt;
//! - runs in its own process group (Unix) / without a console window
//!   (Windows, `CREATE_NO_WINDOW`) and is killed as a whole tree on timeout or
//!   cancellation;
//! - on Windows adds `-c core.longpaths=true`.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

/// Minimum supported git version (`rev-parse --path-format=absolute`, modern
/// `worktree` porcelain).
pub const MIN_GIT_VERSION: GitVersion = GitVersion {
    major: 2,
    minor: 31,
    patch: 0,
};

/// Default timeout for network operations (`fetch`).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Default timeout for every other git operation.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2 * 60);

/// Environment variable prefixes that never reach a git child.
const SCRUBBED_PREFIXES: &[&str] = &["AIONUI_", "AIONCORE_", "AION_MCP_", "CSBU_WORKMATE_"];

/// Substrings that mark a variable as WorkMate auth material regardless of
/// prefix (local API secret, runtime/agent tokens, bootstrap secret).
const SCRUBBED_MARKERS: &[&str] = &["RUNTIME_TOKEN", "LOCAL_SECRET", "LOCAL_API_SECRET", "BOOTSTRAP_SECRET"];

/// Exact variable names that never reach a git child: debugger/agent state
/// inherited from the host and variables that would redirect git to another
/// repository or inject configuration.
const SCRUBBED_EXACT: &[&str] = &[
    "NODE_OPTIONS",
    "NODE_INSPECT",
    "NODE_DEBUG",
    "CLAUDECODE",
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
    "GIT_TERMINAL_PROMPT",
    "GCM_INTERACTIVE",
];

/// `GIT_CONFIG_KEY_<n>` / `GIT_CONFIG_VALUE_<n>` inject configuration too.
const SCRUBBED_GIT_CONFIG_PREFIXES: &[&str] = &["GIT_CONFIG_KEY_", "GIT_CONFIG_VALUE_"];

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct GitVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl std::fmt::Display for GitVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

impl GitVersion {
    /// Parse `git --version` output, e.g. `git version 2.47.3`,
    /// `git version 2.39.2.windows.1`, `git version 2.39.5 (Apple Git-154)`.
    pub fn parse(output: &str) -> Option<Self> {
        let rest = output.trim().strip_prefix("git version ")?;
        let token = rest.split_whitespace().next()?;
        let mut parts = token.split('.').map(|part| {
            let digits: String = part.chars().take_while(|c| c.is_ascii_digit()).collect();
            digits.parse::<u32>().ok()
        });
        let major = parts.next()??;
        let minor = parts.next().flatten().unwrap_or(0);
        let patch = parts.next().flatten().unwrap_or(0);
        Some(Self { major, minor, patch })
    }
}

#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum GitCliError {
    #[error("git executable not found: {0}")]
    NotAvailable(String),
    #[error("git {found} is older than the required {required}")]
    TooOld { found: GitVersion, required: GitVersion },
    #[error("git output is not recognised: {0}")]
    UnknownVersion(String),
    #[error("git {operation} timed out after {seconds}s")]
    Timeout { operation: String, seconds: u64 },
    #[error("git {operation} was cancelled")]
    Cancelled { operation: String },
    #[error("git {operation} failed (exit {code:?}): {stderr}")]
    Failed {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("failed to run git: {0}")]
    Io(String),
    #[error("invalid git ref: {0}")]
    InvalidRef(String),
}

/// Captured result of a finished git invocation.
#[derive(Debug, Clone)]
pub struct GitOutput {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl GitOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Callback for stderr progress lines (split on `\r` and `\n`).
pub type ProgressFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Per-invocation options.
#[derive(Clone, Default)]
pub struct RunOptions {
    pub cwd: Option<PathBuf>,
    pub timeout: Option<Duration>,
    pub cancel: Option<CancellationToken>,
    pub on_progress: Option<ProgressFn>,
}

impl RunOptions {
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            cwd: Some(dir.into()),
            ..Self::default()
        }
    }

    pub fn with_cancel(mut self, cancel: CancellationToken) -> Self {
        self.cancel = Some(cancel);
        self
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn with_progress(mut self, on_progress: ProgressFn) -> Self {
        self.on_progress = Some(on_progress);
        self
    }
}

/// Source of the parent environment (injectable for tests).
#[derive(Clone)]
enum BaseEnv {
    Process,
    Fixed(Vec<(OsString, OsString)>),
}

#[derive(Clone)]
pub struct GitCli {
    program: PathBuf,
    base_env: BaseEnv,
}

impl std::fmt::Debug for GitCli {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitCli").field("program", &self.program).finish()
    }
}

impl Default for GitCli {
    fn default() -> Self {
        Self::new("git")
    }
}

impl GitCli {
    /// `program` is a bare name resolved on `PATH` (default `git`) or a path.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            base_env: BaseEnv::Process,
        }
    }

    /// Use `env` instead of the current process environment as the starting
    /// point (it is still scrubbed). Test hook; production uses the process env.
    pub fn with_base_env<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<OsString>,
        V: Into<OsString>,
    {
        self.base_env = BaseEnv::Fixed(env.into_iter().map(|(k, v)| (k.into(), v.into())).collect());
        self
    }

    pub fn program(&self) -> &Path {
        &self.program
    }

    /// Detect the installed git version and enforce [`MIN_GIT_VERSION`].
    pub async fn probe_version(&self) -> Result<GitVersion, GitCliError> {
        let output = self
            .run(
                ["--version"],
                RunOptions::default().with_timeout(Duration::from_secs(20)),
            )
            .await?;
        if !output.success() {
            return Err(GitCliError::NotAvailable(output.stderr.trim().to_owned()));
        }
        let version = GitVersion::parse(&output.stdout)
            .ok_or_else(|| GitCliError::UnknownVersion(output.stdout.trim().into()))?;
        if version < MIN_GIT_VERSION {
            return Err(GitCliError::TooOld {
                found: version,
                required: MIN_GIT_VERSION,
            });
        }
        Ok(version)
    }

    /// Run git and return its output whatever the exit code; only spawn
    /// failures, timeouts and cancellation are errors.
    pub async fn run<I, S>(&self, args: I, opts: RunOptions) -> Result<GitOutput, GitCliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
        let operation = operation_name(&args);
        let mut cmd = Command::new(&self.program);
        cmd.args(global_args()).args(&args);
        cmd.env_clear();
        cmd.envs(scrub_env(self.base_env_vars()));
        cmd.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never");
        if let Some(dir) = &opts.cwd {
            cmd.current_dir(dir);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        #[cfg(windows)]
        cmd.creation_flags(CREATE_NO_WINDOW);

        let mut child = cmd.spawn().map_err(|err| {
            if err.kind() == std::io::ErrorKind::NotFound {
                GitCliError::NotAvailable(format!("{}: {err}", self.program.display()))
            } else {
                GitCliError::Io(err.to_string())
            }
        })?;

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdout_task = tokio::spawn(read_all(stdout, None));
        let stderr_task = tokio::spawn(read_all(stderr, opts.on_progress.clone()));

        let timeout = opts.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let cancel = opts.cancel.clone().unwrap_or_default();

        enum Outcome {
            Exited(std::io::Result<std::process::ExitStatus>),
            TimedOut,
            Cancelled,
        }

        let outcome = tokio::select! {
            status = child.wait() => Outcome::Exited(status),
            _ = tokio::time::sleep(timeout) => Outcome::TimedOut,
            _ = cancel.cancelled() => Outcome::Cancelled,
        };

        match outcome {
            Outcome::Exited(status) => {
                let status = status.map_err(|err| GitCliError::Io(err.to_string()))?;
                let stdout = join_reader(stdout_task).await;
                let stderr = join_reader(stderr_task).await;
                Ok(GitOutput {
                    code: status.code(),
                    stdout,
                    stderr,
                })
            }
            Outcome::TimedOut | Outcome::Cancelled => {
                if let Err(err) = aionui_runtime::kill_process_tree(&mut child).await {
                    tracing::warn!(operation = %operation, error = %err, "failed to kill git process tree");
                }
                stdout_task.abort();
                stderr_task.abort();
                Err(match outcome {
                    Outcome::TimedOut => GitCliError::Timeout {
                        operation,
                        seconds: timeout.as_secs(),
                    },
                    _ => GitCliError::Cancelled { operation },
                })
            }
        }
    }

    /// Run git and require exit code 0.
    pub async fn run_ok<I, S>(&self, args: I, opts: RunOptions) -> Result<GitOutput, GitCliError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args.into_iter().map(|a| a.as_ref().to_os_string()).collect();
        let output = self.run(&args, opts).await?;
        if output.success() {
            Ok(output)
        } else {
            Err(GitCliError::Failed {
                operation: operation_name(&args),
                code: output.code,
                stderr: output.stderr.trim().to_owned(),
            })
        }
    }

    /// Validate a new branch name: local whitelist first, then
    /// `git check-ref-format --branch`.
    pub async fn check_branch_name(&self, name: &str) -> Result<(), GitCliError> {
        validate_branch_name(name)?;
        let output = self
            .run(["check-ref-format", "--branch", name], RunOptions::default())
            .await?;
        if output.success() {
            Ok(())
        } else {
            Err(GitCliError::InvalidRef(name.to_owned()))
        }
    }

    fn base_env_vars(&self) -> Vec<(OsString, OsString)> {
        match &self.base_env {
            BaseEnv::Process => std::env::vars_os().collect(),
            BaseEnv::Fixed(vars) => vars.clone(),
        }
    }
}

fn global_args() -> Vec<&'static str> {
    let mut args = vec!["-c", "core.quotepath=off", "-c", "color.ui=false"];
    if cfg!(windows) {
        args.extend(["-c", "core.longpaths=true"]);
    }
    args
}

fn operation_name(args: &[OsString]) -> String {
    args.iter()
        .map(|a| a.to_string_lossy())
        .find(|a| !a.starts_with('-'))
        .map(|a| a.into_owned())
        .unwrap_or_else(|| "git".to_owned())
}

/// Whether `name` must be removed from a git child's environment.
pub fn is_scrubbed_env(name: &str) -> bool {
    // Windows env names are case-insensitive.
    let upper = name.to_ascii_uppercase();
    SCRUBBED_PREFIXES.iter().any(|p| upper.starts_with(p))
        || SCRUBBED_MARKERS.iter().any(|m| upper.contains(m))
        || SCRUBBED_EXACT.contains(&upper.as_str())
        || SCRUBBED_GIT_CONFIG_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// Drop every variable [`is_scrubbed_env`] rejects.
pub fn scrub_env<I>(vars: I) -> Vec<(OsString, OsString)>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    vars.into_iter()
        .filter(|(key, _)| !is_scrubbed_env(&key.to_string_lossy()))
        .collect()
}

/// Revision text the user may pass as a base (`main`, `origin/main`,
/// `v1.2`, a SHA, `HEAD~2`). Rejects option injection (`-x`,
/// `--upload-pack=`), whitespace, control characters and shell/rev-spec
/// metacharacters outside a small whitelist.
pub fn validate_rev_text(rev: &str) -> Result<(), GitCliError> {
    let ok = !rev.is_empty()
        && rev.len() <= 255
        && !rev.starts_with('-')
        && !rev.contains("..")
        && rev
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-' | '~' | '^' | '@'));
    if ok {
        Ok(())
    } else {
        Err(GitCliError::InvalidRef(rev.to_owned()))
    }
}

/// Branch names WorkMate is willing to create: `[A-Za-z0-9._/-]`, no leading
/// `-`/`/`, plus git's own structural rules (checked again by
/// `check-ref-format`).
pub fn validate_branch_name(name: &str) -> Result<(), GitCliError> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('-')
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("//")
        && name != "HEAD"
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'));
    if ok {
        Ok(())
    } else {
        Err(GitCliError::InvalidRef(name.to_owned()))
    }
}

async fn read_all<R>(reader: Option<R>, on_progress: Option<ProgressFn>) -> String
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return String::new();
    };
    let mut all = Vec::new();
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        all.extend_from_slice(&buf[..n]);
        if let Some(cb) = &on_progress {
            for &b in &buf[..n] {
                if b == b'\r' || b == b'\n' {
                    if !line.is_empty() {
                        cb(String::from_utf8_lossy(&line).trim());
                        line.clear();
                    }
                } else {
                    line.push(b);
                }
            }
        }
    }
    if let Some(cb) = &on_progress
        && !line.is_empty()
    {
        cb(String::from_utf8_lossy(&line).trim());
    }
    String::from_utf8_lossy(&all).into_owned()
}

async fn join_reader(task: tokio::task::JoinHandle<String>) -> String {
    // A grandchild that inherited the pipe could keep it open after git
    // exits; never let that hang the caller.
    match tokio::time::timeout(Duration::from_secs(5), task).await {
        Ok(Ok(text)) => text,
        _ => String::new(),
    }
}

/// Extract a trailing percentage from a git progress line
/// (`Updating files:  45% (450/1000)`).
pub fn parse_progress_percent(line: &str) -> Option<u8> {
    let idx = line.find('%')?;
    let digits: String = line[..idx]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_digit())
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    digits.parse::<u8>().ok().filter(|p| *p <= 100)
}

#[cfg(test)]
#[path = "git_cli_test.rs"]
mod tests;
