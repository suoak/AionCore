//! Worktree lifecycle: create / cancel / retry / remove / list / get / prune /
//! recover_on_boot.
//!
//! State machine (`task_worktrees.status`):
//! `creating → ready → (unbound) → removing → removed`; any create error →
//! `failed` (retry → `creating`); directory deleted outside WorkMate →
//! `orphaned`. A failed *removal* restores the previous status with
//! `error_code = REMOVE_FAILED`, so `failed` always means "creation did not
//! finish" and retry never destroys a healthy worktree.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_common::now_ms;
use dashmap::DashMap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::error::WorktreeError;
use crate::events::WorktreeEvents;
use crate::git_cli::{
    FETCH_TIMEOUT, GitCli, GitCliError, GitVersion, MIN_GIT_VERSION, RunOptions, parse_progress_percent,
    validate_rev_text,
};
use crate::store::{NewWorktree, WorktreeRow, WorktreeStore};
use crate::types::*;

/// `worktree add` checks out the whole tree; large repositories need more
/// than the 2-minute default, so it shares the fetch budget.
const CHECKOUT_TIMEOUT: Duration = FETCH_TIMEOUT;
/// How long `cancel` waits for the background task to unwind.
const CANCEL_WAIT: Duration = Duration::from_secs(60);
/// Windows only: conservative cap on the managed worktree path itself.
#[cfg(windows)]
const WINDOWS_MAX_WORKTREE_PATH: usize = 200;

pub struct WorktreeServiceConfig {
    /// Managed root, normally `{data_dir}/worktrees`.
    pub root_dir: PathBuf,
    pub git: GitCli,
    pub events: WorktreeEvents,
}

struct Inflight {
    cancel: CancellationToken,
    done: watch::Receiver<bool>,
}

struct Inner {
    store: WorktreeStore,
    git: GitCli,
    root_dir: PathBuf,
    events: WorktreeEvents,
    inflight: DashMap<String, Inflight>,
    git_version: Mutex<Option<GitVersion>>,
}

#[derive(Clone)]
pub struct WorktreeService {
    inner: Arc<Inner>,
}

impl WorktreeService {
    pub fn new(store: WorktreeStore, config: WorktreeServiceConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                git: config.git,
                root_dir: config.root_dir,
                events: config.events,
                inflight: DashMap::new(),
                git_version: Mutex::new(None),
            }),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.inner.root_dir
    }

    // ------------------------------------------------------------------
    // Capability
    // ------------------------------------------------------------------

    /// Probe git (successful probes are cached; failures re-probe so a user
    /// who installs git does not need to restart).
    pub async fn ensure_git(&self) -> Result<GitVersion, WorktreeError> {
        if let Some(v) = *self.inner.git_version.lock().unwrap_or_else(|e| e.into_inner()) {
            return Ok(v);
        }
        let version = self.inner.git.probe_version().await?;
        *self.inner.git_version.lock().unwrap_or_else(|e| e.into_inner()) = Some(version);
        Ok(version)
    }

    pub async fn capabilities(&self) -> GitCapabilityResponse {
        match self.ensure_git().await {
            Ok(v) => GitCapabilityResponse {
                available: true,
                version: Some(v.to_string()),
                min_version: MIN_GIT_VERSION.to_string(),
                error: None,
            },
            Err(err) => GitCapabilityResponse {
                available: false,
                version: None,
                min_version: MIN_GIT_VERSION.to_string(),
                error: Some(WorktreeErrorInfo {
                    code: err.code().to_owned(),
                    message: err.to_string(),
                }),
            },
        }
    }

    // ------------------------------------------------------------------
    // Create / cancel / retry
    // ------------------------------------------------------------------

    pub async fn create(&self, user_id: &str, req: CreateWorktreeRequest) -> Result<WorktreeResponse, WorktreeError> {
        self.ensure_git().await?;
        let repo = self.resolve_repo(&req.repo_path).await?;

        let base_ref = req
            .base_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let (base_ref, base_commit) = match base_ref {
            Some(r) => {
                let sha = self.resolve_commit(&repo.toplevel, &r).await?;
                (r, sha)
            }
            None => self.default_base(&repo.toplevel).await?,
        };

        let id = format!("wt_{}", aionui_common::generate_id_with_length(Some(20)));
        let id8: String = id.trim_start_matches("wt_").chars().take(8).collect();
        let branch = match req.branch.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(b) => b.to_owned(),
            None => default_branch_name(&id8, req.title.as_deref()),
        };
        self.inner
            .git
            .check_branch_name(&branch)
            .await
            .map_err(|_| WorktreeError::InvalidRef(branch.clone()))?;
        if self.branch_tip(&repo.toplevel, &branch).await?.is_some() {
            return Err(WorktreeError::BranchExists(branch));
        }

        let path = self.allocate_path(&repo.common_dir, &id8)?;
        let now = now_ms();
        let row = self
            .inner
            .store
            .insert(&NewWorktree {
                id: id.clone(),
                user_id: user_id.to_owned(),
                source_repo_path: path_string(&repo.toplevel),
                repo_common_dir: path_string(&repo.common_dir),
                path: path_string(&path),
                branch,
                base_ref,
                base_commit,
                task_session_id: req.task_session_id.filter(|s| !s.trim().is_empty()),
                now,
            })
            .await?;
        tracing::info!(
            event = "worktree.create_requested",
            worktree_id = %row.id,
            user_id = %user_id,
            repo = %row.source_repo_path,
            branch = %row.branch,
            base_commit = %row.base_commit,
            "worktree create requested"
        );
        self.inner.events.changed(user_id, &row.id, STATUS_CREATING);
        self.spawn_create(row.clone(), false);
        Ok(row.into())
    }

    pub async fn cancel(&self, user_id: &str, id: &str) -> Result<WorktreeResponse, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        if row.status != STATUS_CREATING {
            return Err(WorktreeError::InvalidState {
                status: row.status,
                action: "cancel",
            });
        }
        let done = self.inner.inflight.get(id).map(|entry| {
            entry.cancel.cancel();
            entry.done.clone()
        });
        match done {
            Some(mut done) => {
                let _ = tokio::time::timeout(CANCEL_WAIT, done.wait_for(|d| *d)).await;
            }
            None => {
                // No live task (e.g. it died with the process): settle directly.
                let changed = self
                    .inner
                    .store
                    .set_status(
                        id,
                        &[STATUS_CREATING],
                        STATUS_FAILED,
                        None,
                        Some(("CANCELLED", "cancelled by user")),
                        now_ms(),
                    )
                    .await?;
                if changed {
                    self.inner.events.changed(user_id, id, STATUS_FAILED);
                }
            }
        }
        Ok(self.inner.store.get(user_id, id).await?.into())
    }

    pub async fn retry(&self, user_id: &str, id: &str) -> Result<WorktreeResponse, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        if row.status != STATUS_FAILED {
            return Err(WorktreeError::InvalidState {
                status: row.status,
                action: "retry",
            });
        }
        self.ensure_git().await?;
        if !Path::new(&row.source_repo_path).is_dir() {
            return Err(WorktreeError::NotARepository(row.source_repo_path));
        }
        self.check_contained(Path::new(&row.path))?;
        let changed = self
            .inner
            .store
            .set_status(id, &[STATUS_FAILED], STATUS_CREATING, Some("add"), None, now_ms())
            .await?;
        if !changed {
            let row = self.inner.store.get(user_id, id).await?;
            return Err(WorktreeError::InvalidState {
                status: row.status,
                action: "retry",
            });
        }
        let row = self.inner.store.get(user_id, id).await?;
        tracing::info!(event = "worktree.retry", worktree_id = %id, user_id = %user_id, "worktree retry");
        self.inner.events.changed(user_id, id, STATUS_CREATING);
        self.spawn_create(row.clone(), true);
        Ok(row.into())
    }

    fn spawn_create(&self, row: WorktreeRow, is_retry: bool) {
        let cancel = CancellationToken::new();
        let (done_tx, done_rx) = watch::channel(false);
        self.inner.inflight.insert(
            row.id.clone(),
            Inflight {
                cancel: cancel.clone(),
                done: done_rx,
            },
        );
        let this = self.clone();
        tokio::spawn(async move {
            let id = row.id.clone();
            this.run_create(row, is_retry, cancel).await;
            this.inner.inflight.remove(&id);
            let _ = done_tx.send(true);
        });
    }

    async fn run_create(&self, row: WorktreeRow, is_retry: bool, cancel: CancellationToken) {
        let result = self.create_checkout(&row, is_retry, &cancel).await;
        let now = now_ms();
        match result {
            Ok(()) => {
                let _ = self
                    .inner
                    .store
                    .set_status(&row.id, &[STATUS_CREATING], STATUS_READY, None, None, now)
                    .await;
                tracing::info!(
                    event = "worktree.created",
                    worktree_id = %row.id,
                    user_id = %row.user_id,
                    path = %row.path,
                    branch = %row.branch,
                    "worktree ready"
                );
                self.inner.events.changed(&row.user_id, &row.id, STATUS_READY);
            }
            Err(err) => {
                let err = if cancel.is_cancelled() {
                    WorktreeError::Cancelled
                } else {
                    err
                };
                let message = match &err {
                    WorktreeError::Cancelled => "cancelled by user".to_owned(),
                    other => other.to_string(),
                };
                let _ = self
                    .inner
                    .store
                    .set_status(
                        &row.id,
                        &[STATUS_CREATING],
                        STATUS_FAILED,
                        None,
                        Some((err.code(), &message)),
                        now,
                    )
                    .await;
                tracing::info!(
                    event = "worktree.create_failed",
                    worktree_id = %row.id,
                    user_id = %row.user_id,
                    code = err.code(),
                    error = %message,
                    "worktree create failed"
                );
                self.inner.events.changed(&row.user_id, &row.id, STATUS_FAILED);
            }
        }
    }

    async fn create_checkout(
        &self,
        row: &WorktreeRow,
        is_retry: bool,
        cancel: &CancellationToken,
    ) -> Result<(), WorktreeError> {
        let repo = PathBuf::from(&row.source_repo_path);
        let path = PathBuf::from(&row.path);
        if !repo.is_dir() {
            return Err(WorktreeError::NotARepository(row.source_repo_path.clone()));
        }

        if is_retry {
            if self.is_healthy(row).await {
                // The previous attempt actually finished (or a removal was
                // interrupted): adopt what is there instead of rebuilding it.
                return Ok(());
            }
            self.cleanup_partial(row, false).await;
        }

        let existing_tip = self.branch_tip(&repo, &row.branch).await?;
        let mut args: Vec<String> = vec!["worktree".into(), "add".into()];
        match &existing_tip {
            None => {
                args.extend([
                    "-b".into(),
                    row.branch.clone(),
                    row.path.clone(),
                    row.base_commit.clone(),
                ]);
            }
            Some(tip) if is_retry && *tip == row.base_commit => {
                // Branch left behind by an interrupted attempt; attach to it.
                args.extend([row.path.clone(), row.branch.clone()]);
            }
            Some(_) => return Err(WorktreeError::BranchExists(row.branch.clone())),
        }

        self.inner
            .events
            .progress(&row.user_id, &row.id, "add", Some(0), "preparing worktree");
        let events = self.inner.events.clone();
        let (user_id, wt_id) = (row.user_id.clone(), row.id.clone());
        let on_progress = Arc::new(move |line: &str| {
            events.progress(&user_id, &wt_id, "checkout", parse_progress_percent(line), line);
        });
        let opts = RunOptions::in_dir(&repo)
            .with_timeout(CHECKOUT_TIMEOUT)
            .with_cancel(cancel.clone())
            .with_progress(on_progress);
        let _ = self.inner.store.set_phase(&row.id, "checkout", now_ms()).await;
        match self.inner.git.run_ok(&args, opts).await {
            Ok(_) => {
                self.inner
                    .events
                    .progress(&row.user_id, &row.id, "checkout", Some(100), "worktree ready");
                if !path.is_dir() {
                    return Err(WorktreeError::Git("worktree directory missing after add".into()));
                }
                Ok(())
            }
            Err(err) => {
                let created_branch = existing_tip.is_none();
                self.cleanup_partial(row, created_branch).await;
                Err(classify_add_error(err, row))
            }
        }
    }

    /// The worktree exists, is registered, finished initialising and has the
    /// branch checked out.
    async fn is_healthy(&self, row: &WorktreeRow) -> bool {
        let path = Path::new(&row.path);
        if !path.is_dir() {
            return false;
        }
        let Some(admin) = find_admin_entry(Path::new(&row.repo_common_dir), path) else {
            return false;
        };
        if admin.join("locked").exists() {
            return false;
        }
        match self
            .inner
            .git
            .run(["rev-parse", "--abbrev-ref", "HEAD"], RunOptions::in_dir(path))
            .await
        {
            Ok(out) if out.success() => out.stdout.trim() == row.branch,
            _ => false,
        }
    }

    /// Undo a partial `worktree add`. Only ever touches the managed path and,
    /// when `delete_branch`, a branch this attempt created that still points
    /// at `base_commit`.
    async fn cleanup_partial(&self, row: &WorktreeRow, delete_branch: bool) {
        let repo = PathBuf::from(&row.source_repo_path);
        let path = PathBuf::from(&row.path);
        if path.exists() {
            let _ = self
                .inner
                .git
                .run(
                    ["worktree", "remove", "--force", "--force", row.path.as_str()],
                    RunOptions::in_dir(&repo),
                )
                .await;
        }
        if path.exists()
            && self.check_contained(&path).is_ok()
            && let Err(err) = std::fs::remove_dir_all(&path)
        {
            tracing::warn!(worktree_id = %row.id, error = %err, "failed to remove partial worktree directory");
        }
        remove_stale_admin_entry(Path::new(&row.repo_common_dir), &path);
        if delete_branch
            && let Ok(Some(tip)) = self.branch_tip(&repo, &row.branch).await
            && tip == row.base_commit
        {
            let _ = self
                .inner
                .git
                .run(["branch", "-D", row.branch.as_str()], RunOptions::in_dir(&repo))
                .await;
        }
    }

    // ------------------------------------------------------------------
    // Remove
    // ------------------------------------------------------------------

    pub async fn remove(
        &self,
        user_id: &str,
        id: &str,
        opts: RemoveWorktreeQuery,
    ) -> Result<WorktreeResponse, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        let prev = row.status.clone();
        if ![STATUS_READY, STATUS_FAILED, STATUS_UNBOUND, STATUS_ORPHANED].contains(&prev.as_str()) {
            return Err(WorktreeError::InvalidState {
                status: prev,
                action: "remove",
            });
        }
        let path = PathBuf::from(&row.path);
        if path.is_dir() {
            self.ensure_git().await?;
            if prev != STATUS_FAILED && !opts.force_dir && self.is_dirty(&path).await {
                return Err(WorktreeError::Dirty(row.path.clone()));
            }
        }
        let changed = self
            .inner
            .store
            .set_status(id, &[prev.as_str()], STATUS_REMOVING, None, None, now_ms())
            .await?;
        if !changed {
            let row = self.inner.store.get(user_id, id).await?;
            return Err(WorktreeError::InvalidState {
                status: row.status,
                action: "remove",
            });
        }
        self.inner.events.changed(user_id, id, STATUS_REMOVING);
        let row = self.inner.store.get(user_id, id).await?;

        let (done_tx, done_rx) = watch::channel(false);
        self.inner.inflight.insert(
            id.to_owned(),
            Inflight {
                cancel: CancellationToken::new(),
                done: done_rx,
            },
        );
        let this = self.clone();
        let task_row = row.clone();
        tokio::spawn(async move {
            let id = task_row.id.clone();
            this.run_remove(task_row, prev, opts).await;
            this.inner.inflight.remove(&id);
            let _ = done_tx.send(true);
        });
        Ok(row.into())
    }

    async fn run_remove(&self, row: WorktreeRow, prev: String, opts: RemoveWorktreeQuery) {
        match self.remove_checkout(&row, &prev, opts).await {
            Ok(preserved) => {
                let _ = self.inner.store.mark_removed(&row.id, preserved, now_ms()).await;
                tracing::info!(
                    event = "worktree.removed",
                    worktree_id = %row.id,
                    user_id = %row.user_id,
                    branch = %row.branch,
                    branch_preserved = preserved,
                    "worktree removed"
                );
                self.inner.events.changed(&row.user_id, &row.id, STATUS_REMOVED);
            }
            Err(err) => {
                let message = err.to_string();
                let _ = self
                    .inner
                    .store
                    .set_status(
                        &row.id,
                        &[STATUS_REMOVING],
                        &prev,
                        None,
                        Some(("REMOVE_FAILED", &message)),
                        now_ms(),
                    )
                    .await;
                tracing::warn!(
                    event = "worktree.remove_failed",
                    worktree_id = %row.id,
                    user_id = %row.user_id,
                    error = %message,
                    "worktree remove failed"
                );
                self.inner.events.changed(&row.user_id, &row.id, &prev);
            }
        }
    }

    /// Returns whether the branch was preserved.
    async fn remove_checkout(
        &self,
        row: &WorktreeRow,
        prev: &str,
        opts: RemoveWorktreeQuery,
    ) -> Result<bool, WorktreeError> {
        let repo = PathBuf::from(&row.source_repo_path);
        let path = PathBuf::from(&row.path);
        let repo_exists = repo.is_dir();

        if path.exists() {
            if repo_exists {
                let mut args = vec!["worktree", "remove"];
                if prev == STATUS_FAILED {
                    args.extend(["--force", "--force"]);
                } else if opts.force_dir {
                    args.push("--force");
                }
                args.push(row.path.as_str());
                let attempts = if cfg!(windows) { 3 } else { 1 };
                let mut last_err = None;
                for attempt in 0..attempts {
                    match self.inner.git.run_ok(&args, RunOptions::in_dir(&repo)).await {
                        Ok(_) => {
                            last_err = None;
                            break;
                        }
                        Err(err) => {
                            last_err = Some(err);
                            // Antivirus / indexers briefly lock files on Windows.
                            tokio::time::sleep(Duration::from_millis(500 * (attempt + 1))).await;
                        }
                    }
                }
                if let Some(err) = last_err {
                    if prev == STATUS_FAILED && self.check_contained(&path).is_ok() {
                        std::fs::remove_dir_all(&path).map_err(|e| WorktreeError::Io(e.to_string()))?;
                    } else {
                        return Err(err.into());
                    }
                }
            } else if (opts.force_dir || prev == STATUS_FAILED) && self.check_contained(&path).is_ok() {
                std::fs::remove_dir_all(&path).map_err(|e| WorktreeError::Io(e.to_string()))?;
            } else {
                return Err(WorktreeError::NotARepository(row.source_repo_path.clone()));
            }
        }
        remove_stale_admin_entry(Path::new(&row.repo_common_dir), &path);

        if !repo_exists {
            return Ok(false);
        }
        let Some(_tip) = self.branch_tip(&repo, &row.branch).await? else {
            return Ok(false);
        };
        let delete = match opts.branch {
            BranchPolicy::Keep => false,
            BranchPolicy::Force => true,
            BranchPolicy::Auto => self.is_branch_merged(row).await,
        };
        if !delete {
            return Ok(true);
        }
        let deleted = self
            .inner
            .git
            .run(["branch", "-D", row.branch.as_str()], RunOptions::in_dir(&repo))
            .await
            .map(|o| o.success())
            .unwrap_or(false);
        Ok(!deleted)
    }

    /// The branch has no commits outside `base_commit`, the (re-resolved)
    /// `base_ref`, or the source repository's `HEAD`.
    pub async fn is_branch_merged(&self, row: &WorktreeRow) -> bool {
        let repo = PathBuf::from(&row.source_repo_path);
        let Ok(Some(tip)) = self.branch_tip(&repo, &row.branch).await else {
            return false;
        };
        let mut candidates = vec![row.base_commit.clone()];
        if let Ok(sha) = self.resolve_commit(&repo, &row.base_ref).await {
            candidates.push(sha);
        }
        if let Ok(sha) = self.resolve_commit(&repo, "HEAD").await {
            candidates.push(sha);
        }
        for target in candidates {
            if let Ok(out) = self
                .inner
                .git
                .run(
                    ["merge-base", "--is-ancestor", tip.as_str(), target.as_str()],
                    RunOptions::in_dir(&repo),
                )
                .await
                && out.success()
            {
                return true;
            }
        }
        false
    }

    async fn is_dirty(&self, path: &Path) -> bool {
        match self
            .inner
            .git
            .run(
                ["status", "--porcelain=v1", "--untracked-files=normal"],
                RunOptions::in_dir(path),
            )
            .await
        {
            Ok(out) if out.success() => !out.stdout.trim().is_empty(),
            // Unreadable worktree: be conservative and require force.
            _ => true,
        }
    }

    // ------------------------------------------------------------------
    // Read
    // ------------------------------------------------------------------

    pub async fn list(&self, user_id: &str, query: ListWorktreesQuery) -> Result<Vec<WorktreeResponse>, WorktreeError> {
        let repo = query
            .repo_path
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|p| {
                dunce::canonicalize(p)
                    .map(|c| path_string(&c))
                    .unwrap_or_else(|_| p.to_owned())
            });
        let status = query.status.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let rows = self
            .inner
            .store
            .list(user_id, repo.as_deref(), status, query.include_removed.unwrap_or(false))
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            let row = self.refresh_orphan(row).await?;
            if status.is_none_or(|s| s == row.status) {
                out.push(row.into());
            }
        }
        Ok(out)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<WorktreeResponse, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        let row = self.refresh_orphan(row).await?;
        let mut resp = WorktreeResponse::from(row.clone());
        if [STATUS_READY, STATUS_UNBOUND].contains(&row.status.as_str())
            && let Some((behind, ahead)) = self.ahead_behind(&row).await
        {
            resp.ahead = Some(ahead);
            resp.behind = Some(behind);
        }
        Ok(resp)
    }

    async fn ahead_behind(&self, row: &WorktreeRow) -> Option<(u32, u32)> {
        let range = format!("{}...refs/heads/{}", row.base_commit, row.branch);
        let out = self
            .inner
            .git
            .run(
                ["rev-list", "--left-right", "--count", range.as_str()],
                RunOptions::in_dir(&row.source_repo_path),
            )
            .await
            .ok()?;
        if !out.success() {
            return None;
        }
        let mut parts = out.stdout.split_whitespace().map(|n| n.parse::<u32>().ok());
        Some((parts.next()??, parts.next()??))
    }

    /// A live worktree whose directory vanished becomes `orphaned`.
    async fn refresh_orphan(&self, row: WorktreeRow) -> Result<WorktreeRow, WorktreeError> {
        if [STATUS_READY, STATUS_UNBOUND].contains(&row.status.as_str()) && !Path::new(&row.path).exists() {
            let changed = self
                .inner
                .store
                .set_status(&row.id, &[row.status.as_str()], STATUS_ORPHANED, None, None, now_ms())
                .await?;
            if changed {
                self.inner.events.changed(&row.user_id, &row.id, STATUS_ORPHANED);
            }
            return Ok(self.inner.store.get_any(&row.id).await?.unwrap_or(row));
        }
        Ok(row)
    }

    // ------------------------------------------------------------------
    // Prune / boot recovery
    // ------------------------------------------------------------------

    /// Mark managed records whose directory no longer exists as removed and
    /// drop their stale git admin entries. Branches are never deleted here.
    pub async fn prune(&self, user_id: &str) -> Result<PruneResponse, WorktreeError> {
        let rows = self.inner.store.list(user_id, None, None, false).await?;
        let mut pruned = Vec::new();
        for row in rows {
            if ![STATUS_READY, STATUS_UNBOUND, STATUS_ORPHANED, STATUS_FAILED].contains(&row.status.as_str()) {
                continue;
            }
            if self.inner.inflight.contains_key(&row.id) || Path::new(&row.path).exists() {
                continue;
            }
            remove_stale_admin_entry(Path::new(&row.repo_common_dir), Path::new(&row.path));
            let preserved = Path::new(&row.source_repo_path).is_dir()
                && matches!(
                    self.branch_tip(Path::new(&row.source_repo_path), &row.branch).await,
                    Ok(Some(_))
                );
            self.inner.store.mark_removed(&row.id, preserved, now_ms()).await?;
            self.inner.events.changed(user_id, &row.id, STATUS_REMOVED);
            tracing::info!(event = "worktree.pruned", worktree_id = %row.id, user_id = %user_id, "worktree record pruned");
            pruned.push(row.id);
        }
        Ok(PruneResponse { pruned })
    }

    /// Settle rows left mid-transition by a previous process: `creating` →
    /// `failed` (INTERRUPTED); `removing` → `removed` if the directory is
    /// gone, else `failed` (retry then re-adopts the healthy worktree).
    pub async fn recover_on_boot(&self) -> Result<RecoverReport, WorktreeError> {
        let mut report = RecoverReport::default();
        let rows = self
            .inner
            .store
            .list_any_in_status(&[STATUS_CREATING, STATUS_REMOVING])
            .await?;
        for row in rows {
            if self.inner.inflight.contains_key(&row.id) {
                continue;
            }
            let path = Path::new(&row.path);
            if !path.exists() && remove_stale_admin_entry(Path::new(&row.repo_common_dir), path) {
                report.stale_admin_entries += 1;
            }
            let now = now_ms();
            if row.status == STATUS_CREATING {
                report.interrupted_creates += 1;
                self.inner
                    .store
                    .set_status(
                        &row.id,
                        &[STATUS_CREATING],
                        STATUS_FAILED,
                        None,
                        Some(("INTERRUPTED", "WorkMate exited while the worktree was being created")),
                        now,
                    )
                    .await?;
            } else {
                report.interrupted_removes += 1;
                if path.exists() {
                    self.inner
                        .store
                        .set_status(
                            &row.id,
                            &[STATUS_REMOVING],
                            STATUS_FAILED,
                            None,
                            Some(("INTERRUPTED", "WorkMate exited while the worktree was being removed")),
                            now,
                        )
                        .await?;
                } else {
                    self.inner.store.mark_removed(&row.id, true, now).await?;
                }
            }
        }
        if report != RecoverReport::default() {
            tracing::info!(
                interrupted_creates = report.interrupted_creates,
                interrupted_removes = report.interrupted_removes,
                stale_admin_entries = report.stale_admin_entries,
                "worktree boot recovery"
            );
        }
        Ok(report)
    }

    /// Wait until no background task is running for `id` (tests, shutdown).
    pub async fn wait_idle(&self, id: &str) {
        let done = self.inner.inflight.get(id).map(|e| e.done.clone());
        if let Some(mut done) = done {
            let _ = done.wait_for(|d| *d).await;
        }
    }

    // ------------------------------------------------------------------
    // Helpers
    // ------------------------------------------------------------------

    async fn resolve_repo(&self, repo_path: &str) -> Result<RepoInfo, WorktreeError> {
        let raw = repo_path.trim();
        if raw.is_empty() {
            return Err(WorktreeError::BadRequest("repo_path is required".into()));
        }
        let dir = dunce::canonicalize(raw).map_err(|_| WorktreeError::NotARepository(raw.to_owned()))?;
        if !dir.is_dir() {
            return Err(WorktreeError::NotARepository(raw.to_owned()));
        }
        let out = self
            .inner
            .git
            .run(
                [
                    "rev-parse",
                    "--path-format=absolute",
                    "--show-toplevel",
                    "--git-common-dir",
                ],
                RunOptions::in_dir(&dir),
            )
            .await?;
        if !out.success() {
            return Err(WorktreeError::NotARepository(raw.to_owned()));
        }
        let mut lines = out.stdout.lines().map(str::trim).filter(|l| !l.is_empty());
        let (Some(top), Some(common)) = (lines.next(), lines.next()) else {
            return Err(WorktreeError::NotARepository(raw.to_owned()));
        };
        let toplevel = dunce::canonicalize(top).map_err(|_| WorktreeError::NotARepository(raw.to_owned()))?;
        let common_dir = dunce::canonicalize(common).map_err(|_| WorktreeError::NotARepository(raw.to_owned()))?;
        Ok(RepoInfo { toplevel, common_dir })
    }

    async fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String, WorktreeError> {
        validate_rev_text(rev).map_err(|_| WorktreeError::InvalidRef(rev.to_owned()))?;
        let spec = format!("{rev}^{{commit}}");
        let out = self
            .inner
            .git
            .run(
                ["rev-parse", "--verify", "--quiet", spec.as_str()],
                RunOptions::in_dir(repo),
            )
            .await?;
        let sha = out.stdout.trim();
        if out.success() && !sha.is_empty() {
            Ok(sha.to_owned())
        } else {
            Err(WorktreeError::RefNotFound(rev.to_owned()))
        }
    }

    async fn default_base(&self, repo: &Path) -> Result<(String, String), WorktreeError> {
        for candidate in ["origin/HEAD", "HEAD"] {
            if let Ok(sha) = self.resolve_commit(repo, candidate).await {
                return Ok((candidate.to_owned(), sha));
            }
        }
        Err(WorktreeError::RefNotFound("HEAD".into()))
    }

    async fn branch_tip(&self, repo: &Path, branch: &str) -> Result<Option<String>, WorktreeError> {
        let full = format!("refs/heads/{branch}");
        let out = self
            .inner
            .git
            .run(
                ["rev-parse", "--verify", "--quiet", full.as_str()],
                RunOptions::in_dir(repo),
            )
            .await?;
        let sha = out.stdout.trim();
        Ok((out.success() && !sha.is_empty()).then(|| sha.to_owned()))
    }

    fn allocate_path(&self, common_dir: &Path, id8: &str) -> Result<PathBuf, WorktreeError> {
        let repo8 =
            String::from_utf8_lossy(&aionui_common::fnv1a_hex8(path_string(common_dir).as_bytes())).into_owned();
        let parent = self.inner.root_dir.join(&repo8);
        std::fs::create_dir_all(&parent).map_err(|e| WorktreeError::Io(e.to_string()))?;
        let canonical_parent = self.check_contained(&parent)?;
        let path = canonical_parent.join(id8);
        if path.exists() {
            return Err(WorktreeError::PathConflict(path_string(&path)));
        }
        #[cfg(windows)]
        if path.as_os_str().len() > WINDOWS_MAX_WORKTREE_PATH {
            return Err(WorktreeError::PathTooLong(path_string(&path)));
        }
        Ok(path)
    }

    /// Canonical containment under the managed root (rejects symlink escape).
    /// For a not-yet-existing path the parent is checked.
    fn check_contained(&self, path: &Path) -> Result<PathBuf, WorktreeError> {
        std::fs::create_dir_all(&self.inner.root_dir).map_err(|e| WorktreeError::Io(e.to_string()))?;
        let root =
            dunce::canonicalize(&self.inner.root_dir).map_err(|e| WorktreeError::Io(format!("managed root: {e}")))?;
        let canonical = if path.exists() {
            dunce::canonicalize(path).map_err(|e| WorktreeError::Io(e.to_string()))?
        } else {
            let parent = path
                .parent()
                .ok_or_else(|| WorktreeError::PathEscape(path_string(path)))?;
            let name = path
                .file_name()
                .ok_or_else(|| WorktreeError::PathEscape(path_string(path)))?;
            dunce::canonicalize(parent)
                .map_err(|_| WorktreeError::PathEscape(path_string(path)))?
                .join(name)
        };
        if canonical != root && canonical.starts_with(&root) {
            Ok(canonical)
        } else {
            Err(WorktreeError::PathEscape(path_string(path)))
        }
    }
}

struct RepoInfo {
    toplevel: PathBuf,
    common_dir: PathBuf,
}

fn classify_add_error(err: GitCliError, row: &WorktreeRow) -> WorktreeError {
    match &err {
        GitCliError::Failed { stderr, .. } => {
            let lower = stderr.to_ascii_lowercase();
            if lower.contains("already exists") && lower.contains("branch") {
                WorktreeError::BranchExists(row.branch.clone())
            } else if lower.contains("filename too long") || lower.contains("path too long") {
                WorktreeError::PathTooLong(row.path.clone())
            } else if lower.contains("not a git repository") {
                WorktreeError::NotARepository(row.source_repo_path.clone())
            } else {
                err.into()
            }
        }
        _ => err.into(),
    }
}

/// `wm/<id8>-<slug>`; non-ASCII titles fall back to `wm/<id8>`.
pub fn default_branch_name(id8: &str, title: Option<&str>) -> String {
    let mut slug = String::new();
    for c in title.unwrap_or("").chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 32 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("wm/{id8}")
    } else {
        format!("wm/{id8}-{slug}")
    }
}

pub(crate) fn path_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn normalize_for_compare(path: &Path) -> String {
    let canonical = dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = canonical.to_string_lossy().replace('\\', "/");
    let text = text.trim_end_matches('/').to_owned();
    if cfg!(windows) { text.to_lowercase() } else { text }
}

/// The `<common>/worktrees/<name>` admin dir whose `gitdir` points at
/// `<worktree>/.git`.
fn find_admin_entry(common_dir: &Path, worktree: &Path) -> Option<PathBuf> {
    let want = normalize_for_compare(worktree);
    let entries = std::fs::read_dir(common_dir.join("worktrees")).ok()?;
    for entry in entries.flatten() {
        let Ok(gitdir) = std::fs::read_to_string(entry.path().join("gitdir")) else {
            continue;
        };
        let target = Path::new(gitdir.trim());
        let wt = target.parent().unwrap_or(target);
        // The worktree may be gone, so compare its parent canonically and the
        // final component textually.
        let have = match (wt.parent(), wt.file_name()) {
            (Some(parent), Some(name)) => {
                format!("{}/{}", normalize_for_compare(parent), name.to_string_lossy())
            }
            _ => normalize_for_compare(wt),
        };
        let have = if cfg!(windows) { have.to_lowercase() } else { have };
        if have == want {
            return Some(entry.path());
        }
    }
    None
}

/// Precise `git worktree prune` for one managed path: drop the admin entry
/// only when the worktree directory is gone. Returns whether one was removed.
fn remove_stale_admin_entry(common_dir: &Path, worktree: &Path) -> bool {
    if worktree.exists() {
        return false;
    }
    let Some(admin) = find_admin_entry(common_dir, worktree) else {
        return false;
    };
    let admin_root = common_dir.join("worktrees");
    if !admin.starts_with(&admin_root) {
        return false;
    }
    std::fs::remove_dir_all(&admin).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_names_are_slugged() {
        assert_eq!(
            default_branch_name("ab12cd34", Some("Fix Login bug!")),
            "wm/ab12cd34-fix-login-bug"
        );
        assert_eq!(default_branch_name("ab12cd34", Some("修复登录")), "wm/ab12cd34");
        assert_eq!(default_branch_name("ab12cd34", None), "wm/ab12cd34");
        let long = default_branch_name("ab12cd34", Some(&"x".repeat(100)));
        assert!(long.len() <= "wm/ab12cd34-".len() + 33, "{long}");
    }
}
