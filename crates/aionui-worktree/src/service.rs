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
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use aionui_common::now_ms;
use aionui_db::{AppendTaskTraceEventParams, ITaskSessionRepository};
use dashmap::DashMap;
use tokio::sync::{Semaphore, watch};
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
/// An `unbound` row whose conversation still exists this long after the
/// unbind is a rolled-back delete (see `reconcile_unbound`).
const UNBIND_SETTLE_MS: i64 = 30_000;
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
    /// Optional: when set, `task_session_id` is ownership-checked and
    /// lifecycle events are mirrored into `task_trace_events`.
    task_sessions: OnceLock<Arc<dyn ITaskSessionRepository>>,
    /// M8 Q12: serialises "find or create" of a work item's shared worktree
    /// (key `user_id\u{0}work_item_id`).
    work_item_locks: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
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
                task_sessions: OnceLock::new(),
                work_item_locks: DashMap::new(),
            }),
        }
    }

    pub fn root_dir(&self) -> &Path {
        &self.inner.root_dir
    }

    /// Wire the TaskSession repository (first call wins).
    pub fn set_task_sessions(&self, repo: Arc<dyn ITaskSessionRepository>) {
        let _ = self.inner.task_sessions.set(repo);
    }

    /// Mirror a lifecycle event into the linked TaskSession's latest run.
    /// Best-effort: a TaskSession without any run yet has nowhere to put a
    /// trace event (the table requires `run_id`), so the event is skipped.
    pub(crate) async fn trace(&self, row: &WorktreeRow, event_type: &str, extra: serde_json::Value) {
        let (Some(repo), Some(task_session_id)) = (self.inner.task_sessions.get(), row.task_session_id.as_deref())
        else {
            return;
        };
        let runs = match repo.list_runs(&row.user_id, task_session_id).await {
            Ok(runs) => runs,
            Err(error) => {
                tracing::warn!(worktree_id = %row.id, task_session_id, error = %error, "worktree trace: list runs failed");
                return;
            }
        };
        let Some(run) = runs.first() else {
            tracing::debug!(worktree_id = %row.id, task_session_id, event_type, "worktree trace skipped: no task run yet");
            return;
        };
        let mut payload = serde_json::json!({
            "worktree_id": row.id,
            "branch": row.branch,
            "base_commit": row.base_commit,
            "path": row.path,
        });
        if let (Some(target), serde_json::Value::Object(more)) = (payload.as_object_mut(), extra) {
            target.extend(more);
        }
        let payload = payload.to_string();
        if let Err(error) = repo
            .append_trace_event(&AppendTaskTraceEventParams {
                user_id: &row.user_id,
                task_session_id,
                run_id: &run.id,
                event_type,
                timestamp: now_ms(),
                payload: &payload,
            })
            .await
        {
            tracing::warn!(worktree_id = %row.id, task_session_id, event_type, error = %error, "worktree trace append failed");
        }
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

        let task_session_id = req.task_session_id.filter(|s| !s.trim().is_empty());
        if let (Some(repo_ts), Some(ts)) = (self.inner.task_sessions.get(), task_session_id.as_deref())
            && repo_ts
                .get(user_id, ts)
                .await
                .map_err(|e| WorktreeError::Database(e.to_string()))?
                .is_none()
        {
            return Err(WorktreeError::BadRequest("unknown task_session_id".into()));
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
                task_session_id,
                fanout_group_id: None,
                slot: None,
                slot_spec: "{}".into(),
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
        self.spawn_create(row.clone(), false, None);
        Ok(row.into())
    }

    /// E3-4: create one fan-out slot worktree on the group's pinned base.
    /// `gate` bounds how many slots of the group check out at once
    /// (`max_parallel`); a queued slot waits in phase `queued`.
    pub(crate) async fn create_slot(
        &self,
        slot: FanoutSlotWorktree<'_>,
        gate: Arc<Semaphore>,
    ) -> Result<WorktreeRow, WorktreeError> {
        let id = format!("wt_{}", aionui_common::generate_id_with_length(Some(20)));
        let id8: String = id.trim_start_matches("wt_").chars().take(8).collect();
        let title = format!("{} {}", slot.title, slot.slot);
        let branch = default_branch_name(&id8, Some(title.trim()));
        self.inner
            .git
            .check_branch_name(&branch)
            .await
            .map_err(|_| WorktreeError::InvalidRef(branch.clone()))?;
        if self.branch_tip(&slot.repo.toplevel, &branch).await?.is_some() {
            return Err(WorktreeError::BranchExists(branch));
        }
        let path = self.allocate_path(&slot.repo.common_dir, &id8)?;
        let row = self
            .inner
            .store
            .insert(&NewWorktree {
                id,
                user_id: slot.user_id.to_owned(),
                source_repo_path: path_string(&slot.repo.toplevel),
                repo_common_dir: path_string(&slot.repo.common_dir),
                path: path_string(&path),
                branch,
                base_ref: slot.base_ref.to_owned(),
                base_commit: slot.base_commit.to_owned(),
                task_session_id: slot.task_session_id.map(str::to_owned),
                fanout_group_id: Some(slot.group_id.to_owned()),
                slot: Some(slot.slot.to_owned()),
                slot_spec: slot.slot_spec,
                now: now_ms(),
            })
            .await?;
        tracing::info!(
            event = "worktree.create_requested",
            worktree_id = %row.id,
            user_id = %row.user_id,
            fanout_group_id = %slot.group_id,
            slot = %slot.slot,
            branch = %row.branch,
            "fan-out slot worktree requested"
        );
        self.inner.events.changed(&row.user_id, &row.id, STATUS_CREATING);
        self.spawn_create(row.clone(), false, Some(gate));
        Ok(row)
    }

    /// E3-4: cancel an in-flight create, if any (fan-out cancel / discard).
    pub(crate) async fn cancel_if_creating(&self, user_id: &str, id: &str) {
        if let Ok(row) = self.inner.store.get(user_id, id).await
            && row.status == STATUS_CREATING
            && let Err(error) = self.cancel(user_id, id).await
        {
            tracing::warn!(worktree_id = %id, error = %error, "fan-out: cancel slot worktree failed");
        }
    }

    /// E3-4: release a slot worktree from its conversation so it can be
    /// removed (`ready` + bound → `unbound`, `conversation_id` cleared). The
    /// conversation itself is kept; its turn must already be stopped.
    pub(crate) async fn release_slot(&self, user_id: &str, id: &str) -> Result<(), WorktreeError> {
        if self.inner.store.detach_conversation(id, now_ms()).await? {
            self.inner.events.changed(user_id, id, STATUS_UNBOUND);
        }
        Ok(())
    }

    pub(crate) fn store(&self) -> &WorktreeStore {
        &self.inner.store
    }

    pub(crate) fn git(&self) -> &GitCli {
        &self.inner.git
    }

    pub(crate) fn events(&self) -> &WorktreeEvents {
        &self.inner.events
    }

    pub(crate) fn task_sessions(&self) -> Option<&Arc<dyn ITaskSessionRepository>> {
        self.inner.task_sessions.get()
    }

    /// M8 Q12: per-work-item lock for find-or-create.
    pub(crate) fn work_item_lock(&self, user_id: &str, work_item_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.inner
            .work_item_locks
            .entry(format!("{user_id}\u{0}{work_item_id}"))
            .or_default()
            .clone()
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
        self.spawn_create(row.clone(), true, None);
        Ok(row.into())
    }

    fn spawn_create(&self, row: WorktreeRow, is_retry: bool, gate: Option<Arc<Semaphore>>) {
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
            // Fan-out `max_parallel`: hold a permit for the whole checkout.
            let mut _permit = None;
            if let Some(gate) = gate {
                if gate.available_permits() == 0 {
                    let _ = this.inner.store.set_phase(&id, "queued", now_ms()).await;
                }
                tokio::select! {
                    permit = gate.acquire_owned() => _permit = permit.ok(),
                    () = cancel.cancelled() => {}
                }
            }
            this.run_create(row, is_retry, cancel).await;
            this.inner.inflight.remove(&id);
            let _ = done_tx.send(true);
        });
    }

    async fn run_create(&self, row: WorktreeRow, is_retry: bool, cancel: CancellationToken) {
        let result = if cancel.is_cancelled() {
            // Cancelled while queued behind the fan-out gate: nothing to undo.
            Err(WorktreeError::Cancelled)
        } else {
            self.create_checkout(&row, is_retry, &cancel).await
        };
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
                self.trace(&row, "worktree.created", serde_json::json!({ "retry": is_retry }))
                    .await;
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
        // A conversation is running in it: delete the conversation first
        // (which unbinds it), so its agent never loses its cwd mid-turn.
        if prev == STATUS_READY && row.conversation_id.is_some() {
            return Err(WorktreeError::InvalidState {
                status: "bound".into(),
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
                self.trace(
                    &row,
                    "worktree.removed",
                    serde_json::json!({ "branch_preserved": preserved }),
                )
                .await;
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

    pub(crate) async fn is_dirty(&self, path: &Path) -> bool {
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
        let task_session_id = query
            .task_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let rows = self
            .inner
            .store
            .list_filtered(
                user_id,
                repo.as_deref(),
                task_session_id,
                status,
                query.include_removed.unwrap_or(false),
            )
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

    /// Reconcile a row read from the store:
    /// - a live worktree whose directory vanished becomes `orphaned`;
    /// - an `unbound` row that still names a conversation which still exists
    ///   is a rolled-back conversation delete (the unbind hook runs before the
    ///   row delete; a successful delete nulls `conversation_id` through the
    ///   FK), so it goes back to `ready`.
    pub(crate) async fn refresh_orphan(&self, row: WorktreeRow) -> Result<WorktreeRow, WorktreeError> {
        let row = self.reconcile_unbound(row).await?;
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

    async fn reconcile_unbound(&self, row: WorktreeRow) -> Result<WorktreeRow, WorktreeError> {
        let Some(conversation_id) = row.conversation_id.as_deref() else {
            return Ok(row);
        };
        // Grace period: the hook flips the status a moment before the row
        // delete commits; reading in between must not undo a delete that is
        // about to succeed.
        if row.status != STATUS_UNBOUND
            || now_ms().saturating_sub(row.updated_at) < UNBIND_SETTLE_MS
            || !self.inner.store.conversation_exists(conversation_id).await?
        {
            return Ok(row);
        }
        let changed = self
            .inner
            .store
            .set_status(&row.id, &[STATUS_UNBOUND], STATUS_READY, None, None, now_ms())
            .await?;
        if changed {
            tracing::info!(
                event = "worktree.rebound",
                worktree_id = %row.id,
                conversation_id,
                "conversation delete did not complete; worktree bound again"
            );
            self.inner.events.changed(&row.user_id, &row.id, STATUS_READY);
        }
        Ok(self.inner.store.get_any(&row.id).await?.unwrap_or(row))
    }

    // ------------------------------------------------------------------
    // Conversation binding (E3-2)
    // ------------------------------------------------------------------

    /// The row a conversation create wants to bind, reconciled. Ownership is
    /// enforced (another user's id is `NotFound`); state checks are the
    /// caller's, against the returned row.
    pub async fn binding_candidate(&self, user_id: &str, id: &str) -> Result<WorktreeRow, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        self.refresh_orphan(row).await
    }

    /// Attach `conversation_id` (compare-and-set). `Ok(false)`: the worktree
    /// is no longer bindable (taken, or left `ready`/`unbound`).
    pub async fn bind_conversation(
        &self,
        user_id: &str,
        id: &str,
        conversation_id: &str,
    ) -> Result<bool, WorktreeError> {
        let bound = self
            .inner
            .store
            .bind_conversation(user_id, id, conversation_id, now_ms())
            .await?;
        if bound {
            let row = self.inner.store.get(user_id, id).await?;
            tracing::info!(
                event = "worktree.bound",
                worktree_id = %id,
                user_id = %user_id,
                conversation_id,
                "worktree bound to conversation"
            );
            self.inner.events.changed(user_id, id, STATUS_READY);
            self.trace(
                &row,
                "worktree.bound",
                serde_json::json!({ "conversation_id": conversation_id }),
            )
            .await;
        }
        Ok(bound)
    }

    /// Conversation delete hook: every `ready` worktree held by the
    /// conversation becomes `unbound`. The directory and branch are never
    /// touched — they may hold uncommitted work the user cleans up from the
    /// worktree manager. `conversation_id` is left for the FK to null when the
    /// row delete commits; if that delete fails, the next read sees a live
    /// conversation and reverts to `ready` ([`Self::reconcile_unbound`]).
    /// Never fails: errors are logged and the read path reconciles.
    pub async fn on_conversation_deleted(&self, user_id: &str, conversation_id: &str) {
        let rows = match self.inner.store.list_by_conversation(user_id, conversation_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(user_id, conversation_id, error = %error, "worktree unbind: lookup failed");
                return;
            }
        };
        for row in rows {
            if row.status != STATUS_READY {
                continue;
            }
            match self
                .inner
                .store
                .set_status(&row.id, &[STATUS_READY], STATUS_UNBOUND, None, None, now_ms())
                .await
            {
                Ok(true) => {
                    tracing::info!(
                        event = "worktree.unbound",
                        worktree_id = %row.id,
                        user_id,
                        conversation_id,
                        path = %row.path,
                        "conversation deleted; worktree kept as unbound"
                    );
                    self.inner.events.changed(user_id, &row.id, STATUS_UNBOUND);
                    self.trace(
                        &row,
                        "worktree.unbound",
                        serde_json::json!({ "conversation_id": conversation_id }),
                    )
                    .await;
                }
                Ok(false) => {}
                Err(error) => {
                    tracing::warn!(worktree_id = %row.id, conversation_id, error = %error, "worktree unbind failed");
                }
            }
        }
    }

    /// The live managed worktree whose directory is `path` (SCM discover
    /// back-fills `ScmRepository.managed_worktree_id` with it).
    pub async fn managed_id_for_path(&self, path: &Path) -> Option<String> {
        let mut candidates = vec![path_string(path)];
        if let Ok(canonical) = dunce::canonicalize(path) {
            let canonical = path_string(&canonical);
            if !candidates.contains(&canonical) {
                candidates.push(canonical);
            }
        }
        for candidate in candidates {
            match self.inner.store.find_live_by_path(&candidate).await {
                Ok(Some(row)) => return Some(row.id),
                Ok(None) => {}
                Err(error) => {
                    tracing::debug!(error = %error, "managed worktree lookup failed");
                    return None;
                }
            }
        }
        None
    }

    // ------------------------------------------------------------------
    // Diff (read-only, git2)
    // ------------------------------------------------------------------

    async fn diffable(&self, user_id: &str, id: &str) -> Result<WorktreeRow, WorktreeError> {
        let row = self.inner.store.get(user_id, id).await?;
        let row = self.refresh_orphan(row).await?;
        if ![STATUS_READY, STATUS_UNBOUND].contains(&row.status.as_str()) {
            return Err(WorktreeError::InvalidState {
                status: row.status,
                action: "diff",
            });
        }
        self.check_contained(Path::new(&row.path))?;
        Ok(row)
    }

    pub async fn diff_stat(&self, user_id: &str, id: &str) -> Result<DiffStatResponse, WorktreeError> {
        let row = self.diffable(user_id, id).await?;
        let path = PathBuf::from(&row.path);
        let base = row.base_commit.clone();
        let stat = tokio::task::spawn_blocking(move || crate::diff::diff_stat(&path, &base))
            .await
            .map_err(|e| WorktreeError::Io(format!("diff task failed: {e}")))??;
        Ok(DiffStatResponse {
            worktree_id: row.id,
            base_commit: row.base_commit,
            files: stat.files,
            files_changed: stat.files_changed,
            additions: stat.additions,
            deletions: stat.deletions,
            truncated: stat.truncated,
        })
    }

    /// Disk estimate for one more worktree of `repo_path` (read-only).
    pub async fn estimate(
        &self,
        repo_path: &str,
        base_ref: Option<&str>,
    ) -> Result<crate::types::EstimateResponse, WorktreeError> {
        self.ensure_git().await?;
        let repo = self.resolve_repo(repo_path).await?;
        let (base_ref, base_commit) = match base_ref.map(str::trim).filter(|s| !s.is_empty()) {
            Some(r) => (r.to_owned(), self.resolve_commit(&repo.toplevel, r).await?),
            None => self.default_base(&repo.toplevel).await?,
        };
        let dir = repo.toplevel.clone();
        let commit = base_commit.clone();
        let (tracked_bytes, tracked_files, truncated) =
            tokio::task::spawn_blocking(move || crate::diff::tree_size(&dir, &commit))
                .await
                .map_err(|e| WorktreeError::Io(format!("estimate task failed: {e}")))??;
        Ok(crate::types::EstimateResponse {
            repo_path: repo.toplevel.to_string_lossy().into_owned(),
            base_ref,
            base_commit,
            tracked_bytes,
            tracked_files,
            truncated,
        })
    }

    pub async fn diff_file(&self, user_id: &str, id: &str, rel_path: &str) -> Result<DiffResponse, WorktreeError> {
        let rel = crate::diff::validate_relative_path(rel_path)?;
        let row = self.diffable(user_id, id).await?;
        let path = PathBuf::from(&row.path);
        let base = row.base_commit.clone();
        let spec = rel.clone();
        let diff = tokio::task::spawn_blocking(move || crate::diff::file_diff(&path, &base, &spec))
            .await
            .map_err(|e| WorktreeError::Io(format!("diff task failed: {e}")))??;
        Ok(DiffResponse {
            worktree_id: row.id,
            base_commit: row.base_commit,
            path: rel,
            status: diff.status,
            binary: diff.binary,
            patch: diff.patch,
            truncated: diff.truncated,
        })
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

    pub(crate) async fn resolve_repo(&self, repo_path: &str) -> Result<RepoInfo, WorktreeError> {
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

    pub(crate) async fn resolve_commit(&self, repo: &Path, rev: &str) -> Result<String, WorktreeError> {
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

    pub(crate) async fn default_base(&self, repo: &Path) -> Result<(String, String), WorktreeError> {
        for candidate in ["origin/HEAD", "HEAD"] {
            if let Ok(sha) = self.resolve_commit(repo, candidate).await {
                return Ok((candidate.to_owned(), sha));
            }
        }
        Err(WorktreeError::RefNotFound("HEAD".into()))
    }

    pub(crate) async fn branch_tip(&self, repo: &Path, branch: &str) -> Result<Option<String>, WorktreeError> {
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

pub(crate) struct RepoInfo {
    pub(crate) toplevel: PathBuf,
    pub(crate) common_dir: PathBuf,
}

/// Inputs for [`WorktreeService::create_slot`].
pub(crate) struct FanoutSlotWorktree<'a> {
    pub(crate) user_id: &'a str,
    pub(crate) group_id: &'a str,
    pub(crate) slot: &'a str,
    pub(crate) slot_spec: String,
    pub(crate) title: &'a str,
    pub(crate) repo: &'a RepoInfo,
    pub(crate) base_ref: &'a str,
    pub(crate) base_commit: &'a str,
    pub(crate) task_session_id: Option<&'a str>,
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
