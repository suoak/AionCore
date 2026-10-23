//! PR delivery through the user's own `gh` / `glab` (design §2.3, §6.3).
//!
//! - [`DeliveryService::detect_host`]: remote provider plus whether the
//!   matching CLI is installed and logged in (`auth status` only; WorkMate
//!   never reads or stores the token);
//! - [`DeliveryService::create_pull_request`]: `gh pr create` /
//!   `glab mr create` in the repository, then one `scm_deliveries` row and,
//!   when a task run is named, a `task_evidence(kind='artifact')` row with
//!   `metadata.type = "pull_request"` (the plan's acceptance: the PR link is
//!   TaskRun evidence). Both rows are written in one transaction.

use std::path::PathBuf;
use std::sync::Arc;

use aionui_common::{generate_prefixed_id, now_ms};
use aionui_worktree::GitCli;
use aionui_worktree::git_cli::{RunOptions, validate_branch_name};
use serde::{Deserialize, Serialize};
use sqlx::{Row, SqlitePool};

use crate::delivery_cli::{
    AUTH_STATUS_TIMEOUT, CREATE_TIMEOUT, CliRunError, HostCli, HostProvider, PrCreateArgs, auth_status_argv,
    create_argv, parse_pr_url, provider_for_host, redact_output, remote_host,
};
use crate::error::ReviewError;
use crate::resolver::ScopeContentResolver;
use crate::types::ReviewScopeKind;

pub const MAX_PR_TITLE_CHARS: usize = 256;
/// `glab` takes the description in argv; keep it well under OS argv limits.
pub const MAX_PR_BODY_BYTES: usize = 24 * 1024;
const MAX_REF_ID_LEN: usize = 256;
const DEFAULT_REMOTE: &str = "origin";
const LIST_LIMIT_MAX: u32 = 200;

pub const CODE_PROVIDER_UNSUPPORTED: &str = "PR_PROVIDER_UNSUPPORTED";
pub const CODE_CLI_NOT_INSTALLED: &str = "PR_CLI_NOT_INSTALLED";
pub const CODE_CLI_NOT_AUTHENTICATED: &str = "PR_CLI_NOT_AUTHENTICATED";
pub const CODE_CREATE_FAILED: &str = "PR_CREATE_FAILED";
pub const CODE_NO_REMOTE: &str = "PR_NO_REMOTE";
pub const CODE_RUN_NOT_FOUND: &str = "PR_TASK_RUN_NOT_FOUND";

#[derive(Debug, Clone, Deserialize)]
pub struct DetectHostQuery {
    pub repo_path: String,
    #[serde(default)]
    pub remote: Option<String>,
}

/// What the PR dialog needs to decide between "create" and "install / log in".
#[derive(Debug, Clone, Serialize)]
pub struct HostStatus {
    pub provider: HostProvider,
    pub host: Option<String>,
    pub remote: Option<String>,
    pub remote_url_redacted: Option<String>,
    /// `gh` / `glab`; `None` for an unsupported provider.
    pub cli: Option<&'static str>,
    pub installed: bool,
    pub authenticated: bool,
    /// Redacted `auth status` output when not logged in.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatePullRequestRequest {
    pub repo_path: String,
    pub branch: String,
    pub base: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    #[serde(default)]
    pub worktree_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CreatePullRequestResponse {
    pub pr_url: String,
    pub pr_number: Option<String>,
    pub provider: HostProvider,
    pub delivery: ScmDelivery,
    pub evidence_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ScmDelivery {
    pub id: String,
    pub repo_path: String,
    pub branch: String,
    pub commit_sha: Option<String>,
    pub remote: Option<String>,
    pub provider: Option<String>,
    pub pr_url: Option<String>,
    pub pr_number: Option<String>,
    pub worktree_id: Option<String>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListScmDeliveriesQuery {
    #[serde(default)]
    pub repo_path: Option<String>,
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// CLIs used by the service (injectable for tests).
#[derive(Clone)]
pub struct DeliveryClis {
    pub git: GitCli,
    pub gh: HostCli,
    pub glab: HostCli,
}

impl Default for DeliveryClis {
    fn default() -> Self {
        Self {
            git: GitCli::new("git"),
            gh: HostCli::new("gh"),
            glab: HostCli::new("glab"),
        }
    }
}

pub struct DeliveryService {
    pool: SqlitePool,
    resolver: Arc<dyn ScopeContentResolver>,
    clis: DeliveryClis,
}

struct RemoteTarget {
    name: String,
    url: String,
    host: Option<String>,
    provider: HostProvider,
}

impl DeliveryService {
    pub fn new(pool: SqlitePool, resolver: Arc<dyn ScopeContentResolver>) -> Self {
        Self {
            pool,
            resolver,
            clis: DeliveryClis::default(),
        }
    }

    pub fn with_clis(mut self, clis: DeliveryClis) -> Self {
        self.clis = clis;
        self
    }

    fn cli(&self, provider: HostProvider) -> Option<&HostCli> {
        match provider {
            HostProvider::Github => Some(&self.clis.gh),
            HostProvider::Gitlab => Some(&self.clis.glab),
            HostProvider::Other => None,
        }
    }

    /// The repository directory, sandboxed like the review scopes (allowed
    /// roots or the user's own `pe:` project entry).
    async fn repo_dir(&self, user_id: &str, repo_path: &str) -> Result<PathBuf, ReviewError> {
        if repo_path.is_empty() || repo_path.len() > 4_096 || repo_path.contains('\0') {
            return Err(ReviewError::BadRequest("repo_path is invalid".into()));
        }
        self.resolver
            .scope_root(user_id, ReviewScopeKind::Scm, repo_path)
            .await
            .ok_or_else(|| ReviewError::BadRequest("repository not found or not allowed".into()))
    }

    async fn remote_target(&self, dir: &PathBuf, remote: Option<&str>) -> Result<RemoteTarget, ReviewError> {
        let name = remote.unwrap_or(DEFAULT_REMOTE);
        if !is_valid_remote_name(name) {
            return Err(ReviewError::BadRequest("remote name is invalid".into()));
        }
        let out = self
            .clis
            .git
            .run(["remote", "get-url", "--", name], RunOptions::in_dir(dir))
            .await
            .map_err(|e| ReviewError::Conflict {
                code: CODE_NO_REMOTE,
                message: redact_output(&e.to_string()),
            })?;
        if !out.success() || out.stdout.trim().is_empty() {
            return Err(ReviewError::Conflict {
                code: CODE_NO_REMOTE,
                message: format!("remote '{name}' is not configured"),
            });
        }
        let url = out.stdout.trim().to_owned();
        let host = remote_host(&url);
        let provider = host.as_deref().map(provider_for_host).unwrap_or(HostProvider::Other);
        Ok(RemoteTarget {
            name: name.to_owned(),
            url,
            host,
            provider,
        })
    }

    pub async fn detect_host(&self, user_id: &str, query: DetectHostQuery) -> Result<HostStatus, ReviewError> {
        let dir = self.repo_dir(user_id, &query.repo_path).await?;
        let target = self.remote_target(&dir, query.remote.as_deref()).await?;
        let mut status = HostStatus {
            provider: target.provider,
            host: target.host.clone(),
            remote: Some(target.name.clone()),
            remote_url_redacted: Some(redact_output(&target.url)),
            cli: target.provider.cli_name(),
            installed: false,
            authenticated: false,
            detail: None,
        };
        let (Some(cli), Some(host)) = (self.cli(target.provider), target.host.as_deref()) else {
            return Ok(status);
        };
        match cli.run(&auth_status_argv(host), &dir, None, AUTH_STATUS_TIMEOUT).await {
            Ok(out) => {
                status.installed = true;
                status.authenticated = out.success();
                if !out.success() {
                    status.detail = Some(redact_output(&out.combined()));
                }
            }
            Err(CliRunError::NotInstalled(_)) => {}
            Err(other) => {
                status.installed = true;
                status.detail = Some(redact_output(&other.to_string()));
            }
        }
        Ok(status)
    }

    pub async fn create_pull_request(
        &self,
        user_id: &str,
        req: CreatePullRequestRequest,
    ) -> Result<CreatePullRequestResponse, ReviewError> {
        validate_create(&req)?;
        let dir = self.repo_dir(user_id, &req.repo_path).await?;
        let target = self.remote_target(&dir, req.remote.as_deref()).await?;
        let (Some(cli), Some(host)) = (self.cli(target.provider), target.host.clone()) else {
            return Err(ReviewError::Conflict {
                code: CODE_PROVIDER_UNSUPPORTED,
                message: "only GitHub (gh) and GitLab (glab) remotes are supported".into(),
            });
        };
        // Fail before running the CLI when the evidence target is not the
        // user's own run, so no PR is created that cannot be recorded.
        if let (Some(session), Some(run)) = (req.task_session_id.as_deref(), req.run_id.as_deref())
            && !self.owns_run(user_id, session, run).await?
        {
            return Err(ReviewError::Conflict {
                code: CODE_RUN_NOT_FOUND,
                message: "task run not found".into(),
            });
        }
        let commit_sha = self
            .clis
            .git
            .run(["rev-parse", "--verify", "HEAD"], RunOptions::in_dir(&dir))
            .await
            .ok()
            .filter(|o| o.success())
            .map(|o| o.stdout.trim().to_owned());

        let args = PrCreateArgs {
            branch: &req.branch,
            base: &req.base,
            title: req.title.trim(),
            body: &req.body,
            draft: req.draft,
        };
        let Some((argv, stdin)) = create_argv(target.provider, &args) else {
            return Err(ReviewError::Conflict {
                code: CODE_PROVIDER_UNSUPPORTED,
                message: "unsupported provider".into(),
            });
        };
        let output = match cli.run(&argv, &dir, stdin, CREATE_TIMEOUT).await {
            Ok(out) => out,
            Err(CliRunError::NotInstalled(name)) => {
                return Err(ReviewError::Conflict {
                    code: CODE_CLI_NOT_INSTALLED,
                    message: format!("{name} is not installed"),
                });
            }
            Err(other) => {
                return Err(ReviewError::Conflict {
                    code: CODE_CREATE_FAILED,
                    message: redact_output(&other.to_string()),
                });
            }
        };
        let combined = output.combined();
        if !output.success() {
            let code = if looks_unauthenticated(&combined) {
                CODE_CLI_NOT_AUTHENTICATED
            } else {
                CODE_CREATE_FAILED
            };
            return Err(ReviewError::Conflict {
                code,
                message: redact_output(&combined),
            });
        }
        let Some((pr_url, pr_number)) = parse_pr_url(&combined) else {
            return Err(ReviewError::Conflict {
                code: CODE_CREATE_FAILED,
                message: format!("no pull request URL in output: {}", redact_output(&combined)),
            });
        };

        let delivery = ScmDelivery {
            id: generate_prefixed_id("scmd"),
            repo_path: dir.to_string_lossy().into_owned(),
            branch: req.branch.clone(),
            commit_sha,
            remote: Some(target.name.clone()),
            provider: Some(target.provider.as_str().to_owned()),
            pr_url: Some(pr_url.clone()),
            pr_number: Some(pr_number.clone()),
            worktree_id: req.worktree_id.clone(),
            task_session_id: req.task_session_id.clone(),
            run_id: req.run_id.clone(),
            created_at: now_ms(),
        };
        let evidence_id = self.record(user_id, &delivery, &req, &host).await?;
        tracing::info!(
            target: "scm",
            event = "scm.pr_created",
            delivery_id = %delivery.id,
            provider = target.provider.as_str(),
            task_session_id = ?delivery.task_session_id,
            run_id = ?delivery.run_id,
            "pull request created"
        );
        Ok(CreatePullRequestResponse {
            pr_url,
            pr_number: Some(pr_number),
            provider: target.provider,
            delivery,
            evidence_id,
        })
    }

    async fn owns_run(&self, user_id: &str, session: &str, run: &str) -> Result<bool, ReviewError> {
        let row = sqlx::query(
            "SELECT 1 FROM task_sessions task JOIN task_runs run ON run.task_session_id = task.id \
             WHERE task.user_id = ? AND task.id = ? AND run.id = ?",
        )
        .bind(user_id)
        .bind(session)
        .bind(run)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    async fn record(
        &self,
        user_id: &str,
        delivery: &ScmDelivery,
        req: &CreatePullRequestRequest,
        host: &str,
    ) -> Result<Option<String>, ReviewError> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO scm_deliveries (id, user_id, repo_path, branch, commit_sha, remote, provider, pr_url, \
             pr_number, worktree_id, task_session_id, run_id, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&delivery.id)
        .bind(user_id)
        .bind(&delivery.repo_path)
        .bind(&delivery.branch)
        .bind(&delivery.commit_sha)
        .bind(&delivery.remote)
        .bind(&delivery.provider)
        .bind(&delivery.pr_url)
        .bind(&delivery.pr_number)
        .bind(&delivery.worktree_id)
        .bind(&delivery.task_session_id)
        .bind(&delivery.run_id)
        .bind(delivery.created_at)
        .execute(&mut *tx)
        .await?;

        let mut evidence_id = None;
        if let (Some(session), Some(run)) = (delivery.task_session_id.as_deref(), delivery.run_id.as_deref()) {
            let id = generate_prefixed_id("evidence");
            let metadata = serde_json::json!({
                "type": "pull_request",
                "url": delivery.pr_url,
                "number": delivery.pr_number,
                "provider": delivery.provider,
                "host": host,
                "branch": delivery.branch,
                "base": req.base,
                "draft": req.draft,
                "commit_sha": delivery.commit_sha,
                "delivery_id": delivery.id,
                "worktree_id": delivery.worktree_id,
            });
            let summary = format!("Pull request: {}", req.title.trim());
            let result = sqlx::query(
                "INSERT INTO task_evidence \
                 (id, task_session_id, run_id, trace_event_id, criterion_id, kind, summary, reference, metadata, created_at) \
                 SELECT ?, ?, ?, NULL, NULL, 'artifact', ?, ?, ?, ? WHERE EXISTS (\
                     SELECT 1 FROM task_sessions task JOIN task_runs run ON run.task_session_id = task.id \
                     WHERE task.user_id = ? AND task.id = ? AND run.id = ?)",
            )
            .bind(&id)
            .bind(session)
            .bind(run)
            .bind(summary)
            .bind(&delivery.pr_url)
            .bind(metadata.to_string())
            .bind(delivery.created_at)
            .bind(user_id)
            .bind(session)
            .bind(run)
            .execute(&mut *tx)
            .await?;
            if result.rows_affected() == 1 {
                evidence_id = Some(id);
            } else {
                // The run vanished between the check and now: keep the
                // delivery row (the PR exists) and log the missing evidence.
                tracing::warn!(target: "scm", delivery_id = %delivery.id, "task run gone; PR evidence not recorded");
            }
        }
        tx.commit().await?;
        Ok(evidence_id)
    }

    pub async fn list(&self, user_id: &str, query: ListScmDeliveriesQuery) -> Result<Vec<ScmDelivery>, ReviewError> {
        let limit = query.limit.unwrap_or(50).clamp(1, LIST_LIMIT_MAX);
        let repo = match query.repo_path.as_deref() {
            Some(path) => Some(self.repo_dir(user_id, path).await?.to_string_lossy().into_owned()),
            None => None,
        };
        let rows = sqlx::query(
            "SELECT id, repo_path, branch, commit_sha, remote, provider, pr_url, pr_number, worktree_id, \
             task_session_id, run_id, created_at FROM scm_deliveries \
             WHERE user_id = ? AND (? IS NULL OR repo_path = ?) AND (? IS NULL OR task_session_id = ?) \
             ORDER BY created_at DESC, id DESC LIMIT ?",
        )
        .bind(user_id)
        .bind(&repo)
        .bind(&repo)
        .bind(&query.task_session_id)
        .bind(&query.task_session_id)
        .bind(i64::from(limit))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ScmDelivery {
                    id: row.try_get("id")?,
                    repo_path: row.try_get("repo_path")?,
                    branch: row.try_get("branch")?,
                    commit_sha: row.try_get("commit_sha")?,
                    remote: row.try_get("remote")?,
                    provider: row.try_get("provider")?,
                    pr_url: row.try_get("pr_url")?,
                    pr_number: row.try_get("pr_number")?,
                    worktree_id: row.try_get("worktree_id")?,
                    task_session_id: row.try_get("task_session_id")?,
                    run_id: row.try_get("run_id")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }
}

fn is_valid_remote_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_ref_id(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_none_or(|v| !v.is_empty() && v.len() <= MAX_REF_ID_LEN && !v.chars().any(char::is_control))
}

fn validate_create(req: &CreatePullRequestRequest) -> Result<(), ReviewError> {
    let bad = |m: &str| Err(ReviewError::BadRequest(m.to_owned()));
    if validate_branch_name(&req.branch).is_err() {
        return bad("branch is invalid");
    }
    if validate_branch_name(&req.base).is_err() {
        return bad("base is invalid");
    }
    if req.branch == req.base {
        return bad("branch and base must differ");
    }
    let title = req.title.trim();
    if title.is_empty() || title.chars().count() > MAX_PR_TITLE_CHARS || title.chars().any(char::is_control) {
        return bad("title must be 1-256 characters without control characters");
    }
    if req.body.len() > MAX_PR_BODY_BYTES || req.body.contains('\0') {
        return bad("body is too large or contains NUL");
    }
    if req.task_session_id.is_some() != req.run_id.is_some() {
        return bad("task_session_id and run_id go together");
    }
    if !valid_ref_id(&req.task_session_id) || !valid_ref_id(&req.run_id) || !valid_ref_id(&req.worktree_id) {
        return bad("id is invalid");
    }
    Ok(())
}

fn looks_unauthenticated(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "gh auth login",
        "glab auth login",
        "not logged in",
        "authentication",
        "401 unauthorized",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[cfg(test)]
#[path = "delivery_test.rs"]
mod tests;
