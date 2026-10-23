//! Request and result shapes for committing and publishing (M7 E4-4).
//!
//! Kept beside, not inside, `types.rs`: these are the wire shapes of
//! `scm/commit`, `scm/push` and `scm/remoteInfo`, and stay provider-neutral —
//! "remote", "branch" and "upstream" are the vocabulary every distributed VCS
//! the UI can drive shares.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// `scm/commit` parameters beyond the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct CommitRequest {
    /// Full commit message. Must not be blank.
    #[serde(default)]
    pub message: String,
    /// Amending is not offered in this stage; `true` is refused as an
    /// unsupported capability rather than silently ignored.
    #[serde(default)]
    pub amend: bool,
    /// Append a `Signed-off-by` trailer.
    #[serde(default)]
    pub signoff: bool,
    /// Skip the repository's commit hooks. The UI exposes this as an explicit,
    /// default-off toggle shown only when hooks exist.
    #[serde(default)]
    pub no_verify: bool,
}

/// `scm/commit` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommitResult {
    pub commit_sha: String,
}

/// `scm/push` parameters beyond the repository.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct PushRequest {
    /// Remote to push to. Defaults to the branch's upstream remote, else
    /// `origin`.
    #[serde(default)]
    pub remote: Option<String>,
    /// Create the upstream (`<remote>/<branch>`) when the branch has none. The
    /// client asks the user first; without it a branch lacking an upstream is
    /// refused with `PUSH_NO_UPSTREAM`.
    #[serde(default)]
    pub set_upstream: bool,
}

/// `scm/push` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PushResult {
    pub remote: String,
    pub branch: String,
    /// The upstream after the push (`origin/main`).
    pub upstream: String,
    /// Commits still ahead of the upstream — `0` after a successful push.
    pub ahead: u32,
}

/// One remote of a repository, as `scm/remoteInfo` reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ScmRemote {
    pub name: String,
    /// Fetch URL with any embedded credentials removed.
    pub url_redacted: String,
    /// `github` / `gitlab` / `local` / `other` — a hint for the UI's "create
    /// PR" affordance, never a security decision.
    pub provider: &'static str,
}

/// `scm/remoteInfo` result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RemoteInfo {
    pub remotes: Vec<ScmRemote>,
    /// Current branch; absent when detached or unborn.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Upstream (`origin/main`); absent when the branch has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ahead: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub behind: Option<u32>,
    /// Commit hooks a `scm/commit` would run (`pre-commit`, `commit-msg`, ...).
    /// Empty when none are installed; the UI then hides its "skip hooks" toggle.
    pub commit_hooks: Vec<String>,
}

/// One progress step of a long remote operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScmProgress {
    /// Phase as the engine names it (`Counting objects`, `Writing objects`).
    pub phase: String,
    pub percent: Option<u8>,
}

/// Progress sink for remote operations. Called from the operation's task, so
/// it must be cheap and must not block.
pub type ScmProgressFn = Arc<dyn Fn(ScmProgress) + Send + Sync>;
