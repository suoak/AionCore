//! E3-4 fan-out: one prompt, 2..=5 assistants, each in its own managed
//! worktree on the same pinned base commit; compare, then adopt one.
//!
//! Group state (`fanout_groups.status`):
//! `preparing → running → comparing → adopted | discarded`; `cancelled` from
//! any live state; `failed` when no slot could start. `comparing` is entered
//! by the group watcher (or lazily on read) once no slot turn is in flight.
//!
//! Each slot is a conversation created from an existing assistant and runs
//! with that assistant's own permission mode: fan-out never forces YOLO /
//! auto-approve, so tool calls (including sub-agent ones) still go through
//! Core's approval flow. `max_parallel` bounds concurrent worktree checkouts;
//! agent turns are not throttled.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use aionui_common::now_ms;
use dashmap::DashMap;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::WorktreeError;
use crate::fanout_port::{FanoutPrompt, IFanoutConversations, SlotConversationSpec, SlotRuntime};
use crate::fanout_store::{FanoutGroupRow, FanoutStore, NewFanoutGroup};
use crate::fanout_types::*;
use crate::git_cli::RunOptions;
use crate::service::{FanoutSlotWorktree, WorktreeService, path_string};
use crate::store::WorktreeRow;
use crate::types::*;

/// Summary text kept per slot in the compare view.
const SUMMARY_MAX_CHARS: usize = 2000;
/// How long cleanup waits for a cancelled turn to settle.
const TURN_STOP_WAIT: Duration = Duration::from_secs(10);
const DEFAULT_WATCH_INTERVAL: Duration = Duration::from_secs(2);
/// Fallback identity for snapshot / merge commits when the repository has
/// none configured.
const FALLBACK_IDENTITY: [&str; 4] = ["-c", "user.name=CSBU WorkMate", "-c", "user.email=workmate@localhost"];

struct Inner {
    worktrees: WorktreeService,
    store: FanoutStore,
    conversations: OnceLock<Arc<dyn IFanoutConversations>>,
    /// Live group orchestrators (start + watch), by group id.
    runs: DashMap<String, CancellationToken>,
    /// Groups with an adopt / discard / cancel in progress.
    busy: DashMap<String, &'static str>,
    watch_interval: Duration,
}

#[derive(Clone)]
pub struct FanoutService {
    inner: Arc<Inner>,
}

struct BusyGuard {
    inner: Arc<Inner>,
    id: String,
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.inner.busy.remove(&self.id);
    }
}

impl FanoutService {
    pub fn new(worktrees: WorktreeService, store: FanoutStore) -> Self {
        Self::with_watch_interval(worktrees, store, DEFAULT_WATCH_INTERVAL)
    }

    /// Test hook: how often a running group checks whether its slots settled.
    pub fn with_watch_interval(worktrees: WorktreeService, store: FanoutStore, watch_interval: Duration) -> Self {
        Self {
            inner: Arc::new(Inner {
                worktrees,
                store,
                conversations: OnceLock::new(),
                runs: DashMap::new(),
                busy: DashMap::new(),
                watch_interval,
            }),
        }
    }

    /// Wire the conversation port (first call wins). Without it every
    /// mutating route answers `FANOUT_UNAVAILABLE`.
    pub fn set_conversations(&self, port: Arc<dyn IFanoutConversations>) {
        let _ = self.inner.conversations.set(port);
    }

    fn port(&self) -> Result<Arc<dyn IFanoutConversations>, WorktreeError> {
        self.inner
            .conversations
            .get()
            .cloned()
            .ok_or_else(|| WorktreeError::FanoutUnavailable("conversation service is not wired".into()))
    }

    fn lock(&self, id: &str, action: &'static str) -> Result<BusyGuard, WorktreeError> {
        match self.inner.busy.entry(id.to_owned()) {
            dashmap::Entry::Occupied(e) => Err(WorktreeError::FanoutInvalidState {
                status: format!("busy ({})", e.get()),
                action,
            }),
            dashmap::Entry::Vacant(v) => {
                v.insert(action);
                Ok(BusyGuard {
                    inner: self.inner.clone(),
                    id: id.to_owned(),
                })
            }
        }
    }

    fn emit(&self, group: &FanoutGroupRow, status: &str, slot: Option<(&str, &str)>) {
        self.inner
            .worktrees
            .events()
            .fanout_changed(&group.user_id, &group.id, status, slot);
    }

    // ------------------------------------------------------------------
    // Create
    // ------------------------------------------------------------------

    pub async fn create(
        &self,
        user_id: &str,
        req: CreateFanoutGroupRequest,
    ) -> Result<FanoutGroupResponse, WorktreeError> {
        let port = self.port()?;
        let n = req.slots.len();
        if !(MIN_SLOTS..=MAX_SLOTS).contains(&n) {
            return Err(WorktreeError::FanoutLimit(format!(
                "a fan-out needs {MIN_SLOTS} to {MAX_SLOTS} slots, got {n}"
            )));
        }
        let max_parallel = match req.max_parallel {
            Some(p) if !(1..=MAX_SLOTS as u8).contains(&p) => {
                return Err(WorktreeError::FanoutLimit(format!(
                    "max_parallel must be between 1 and {MAX_SLOTS}, got {p}"
                )));
            }
            Some(p) => usize::from(p).min(n),
            None => usize::from(DEFAULT_MAX_PARALLEL).min(n),
        };
        let prompt = req.prompt.trim();
        if prompt.is_empty() {
            return Err(WorktreeError::BadRequest("prompt is required".into()));
        }
        for slot in &req.slots {
            if slot.assistant_id.trim().is_empty() {
                return Err(WorktreeError::BadRequest("every slot needs an assistant_id".into()));
            }
            if slot.permission.is_some() {
                tracing::warn!(
                    assistant_id = %slot.assistant_id,
                    "fan-out: slot permission override ignored; the assistant's own permission mode applies"
                );
            }
            port.check_assistant(user_id, slot.assistant_id.trim())
                .await
                .map_err(|e| WorktreeError::BadRequest(format!("assistant {}: {e}", slot.assistant_id)))?;
        }

        let worktrees = &self.inner.worktrees;
        worktrees.ensure_git().await?;
        let repo = worktrees.resolve_repo(&req.repo_path).await?;
        let base_ref = req
            .base_ref
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);
        let (base_ref, base_commit) = match base_ref {
            Some(r) => {
                let sha = worktrees.resolve_commit(&repo.toplevel, &r).await?;
                (r, sha)
            }
            None => worktrees.default_base(&repo.toplevel).await?,
        };
        let task_session_id = req.task_session_id.filter(|s| !s.trim().is_empty());
        if let (Some(repo_ts), Some(ts)) = (worktrees.task_sessions(), task_session_id.as_deref())
            && repo_ts
                .get(user_id, ts)
                .await
                .map_err(|e| WorktreeError::Database(e.to_string()))?
                .is_none()
        {
            return Err(WorktreeError::BadRequest("unknown task_session_id".into()));
        }
        let title = req
            .title
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| prompt.lines().next().unwrap_or("").chars().take(40).collect());

        let group = self
            .inner
            .store
            .insert(&NewFanoutGroup {
                id: format!("fg_{}", aionui_common::generate_id_with_length(Some(20))),
                user_id: user_id.to_owned(),
                task_session_id: task_session_id.clone(),
                title: title.clone(),
                source_repo_path: path_string(&repo.toplevel),
                repo_common_dir: path_string(&repo.common_dir),
                base_ref: base_ref.clone(),
                base_commit: base_commit.clone(),
                prompt: prompt.to_owned(),
                max_parallel: max_parallel as i64,
                now: now_ms(),
            })
            .await?;

        let gate = Arc::new(Semaphore::new(max_parallel));
        let mut slots = Vec::with_capacity(n);
        for (label, spec) in SLOT_LABELS.iter().zip(&req.slots) {
            let slot_spec = serde_json::to_string(spec).unwrap_or_else(|_| "{}".into());
            match worktrees
                .create_slot(
                    FanoutSlotWorktree {
                        user_id,
                        group_id: &group.id,
                        slot: label,
                        slot_spec,
                        title: &title,
                        repo: &repo,
                        base_ref: &base_ref,
                        base_commit: &base_commit,
                        task_session_id: task_session_id.as_deref(),
                    },
                    gate.clone(),
                )
                .await
            {
                Ok(row) => slots.push((label.to_string(), row.id, spec.clone())),
                Err(err) => {
                    for (_, id, _) in &slots {
                        worktrees.cancel_if_creating(user_id, id).await;
                    }
                    let _ = self
                        .inner
                        .store
                        .set_status(&group.id, &[GROUP_PREPARING], GROUP_FAILED, now_ms())
                        .await;
                    return Err(err);
                }
            }
        }

        tracing::info!(
            event = "fanout.created",
            group_id = %group.id,
            user_id,
            slots = n,
            max_parallel,
            base_commit = %group.base_commit,
            "fan-out group created"
        );
        self.emit(&group, GROUP_PREPARING, None);
        let token = CancellationToken::new();
        self.inner.runs.insert(group.id.clone(), token.clone());
        let this = self.clone();
        let prompt = FanoutPrompt {
            content: prompt.to_owned(),
            files: req.files,
            sessions: req.sessions,
        };
        let run_group = group.clone();
        tokio::spawn(async move {
            let id = run_group.id.clone();
            this.run_group(run_group, slots, prompt, token).await;
            this.inner.runs.remove(&id);
        });
        self.get(user_id, &group.id).await
    }

    async fn run_group(
        &self,
        group: FanoutGroupRow,
        slots: Vec<(String, String, FanoutSlotRequest)>,
        prompt: FanoutPrompt,
        token: CancellationToken,
    ) {
        let prompt = Arc::new(prompt);
        let mut set = JoinSet::new();
        for (label, worktree_id, spec) in slots {
            let this = self.clone();
            let group = group.clone();
            let prompt = prompt.clone();
            let token = token.clone();
            set.spawn(async move {
                this.start_slot(&group, &label, &worktree_id, spec, &prompt, &token)
                    .await
            });
        }
        let mut started = 0usize;
        while let Some(res) = set.join_next().await {
            if matches!(res, Ok(true)) {
                started += 1;
            }
        }
        if token.is_cancelled() {
            return;
        }
        if started == 0 {
            if let Ok(true) = self
                .inner
                .store
                .set_status(&group.id, &[GROUP_PREPARING], GROUP_FAILED, now_ms())
                .await
            {
                tracing::info!(event = "fanout.failed", group_id = %group.id, "no fan-out slot could start");
                self.emit(&group, GROUP_FAILED, None);
            }
            return;
        }
        // Watch until every slot turn has settled.
        loop {
            tokio::select! {
                () = tokio::time::sleep(self.inner.watch_interval) => {}
                () = token.cancelled() => return,
            }
            match self.refresh_comparing(&group.user_id, &group.id).await {
                Ok(status) if status == GROUP_RUNNING => continue,
                _ => return,
            }
        }
    }

    /// Wait for the slot worktree, create its conversation, send the prompt.
    /// Returns whether the slot started.
    async fn start_slot(
        &self,
        group: &FanoutGroupRow,
        label: &str,
        worktree_id: &str,
        spec: FanoutSlotRequest,
        prompt: &FanoutPrompt,
        token: &CancellationToken,
    ) -> bool {
        let worktrees = &self.inner.worktrees;
        tokio::select! {
            () = worktrees.wait_idle(worktree_id) => {}
            () = token.cancelled() => return false,
        }
        if token.is_cancelled() {
            return false;
        }
        let row = match worktrees.store().get(&group.user_id, worktree_id).await {
            Ok(row) => row,
            Err(_) => return false,
        };
        if row.status != STATUS_READY {
            self.emit(group, GROUP_PREPARING, Some((label, "failed")));
            return false;
        }
        let Ok(port) = self.port() else { return false };
        let fail = |message: String| async move {
            tracing::warn!(group_id = %group.id, slot = label, error = %message, "fan-out slot failed to start");
            let _ = worktrees
                .store()
                .set_error(worktree_id, SLOT_START_FAILED, &message, now_ms())
                .await;
            self.emit(group, GROUP_PREPARING, Some((label, "failed")));
            false
        };
        let conversation_id = match port
            .create_slot_conversation(
                &group.user_id,
                SlotConversationSpec {
                    worktree_id: worktree_id.to_owned(),
                    assistant_id: spec.assistant_id.trim().to_owned(),
                    model: spec.model.clone(),
                    title: format!("{} · {label}", group.title),
                },
            )
            .await
        {
            Ok(id) => id,
            Err(e) => return fail(format!("create conversation: {e}")).await,
        };
        if token.is_cancelled() {
            return false;
        }
        if let Err(e) = port.send_prompt(&group.user_id, &conversation_id, prompt).await {
            return fail(format!("send prompt: {e}")).await;
        }
        if let Ok(true) = self
            .inner
            .store
            .set_status(&group.id, &[GROUP_PREPARING], GROUP_RUNNING, now_ms())
            .await
        {
            tracing::info!(event = "fanout.running", group_id = %group.id, "fan-out group running");
            self.emit(group, GROUP_RUNNING, None);
        }
        self.emit(group, GROUP_RUNNING, Some((label, "running")));
        true
    }

    // ------------------------------------------------------------------
    // Read
    // ------------------------------------------------------------------

    async fn runtimes(&self, user_id: &str, rows: &[WorktreeRow]) -> HashMap<String, SlotRuntime> {
        let mut out = HashMap::new();
        let Ok(port) = self.port() else { return out };
        for row in rows {
            if let Some(conv) = row.conversation_id.as_deref() {
                out.insert(conv.to_owned(), port.runtime(user_id, conv).await);
            }
        }
        out
    }

    fn slot_status(
        &self,
        group: &FanoutGroupRow,
        row: &WorktreeRow,
        runtimes: &HashMap<String, SlotRuntime>,
    ) -> String {
        if group.winner_slot.is_some() && group.winner_slot == row.slot {
            return "adopted".into();
        }
        let status = match row.status.as_str() {
            STATUS_CREATING => "preparing",
            STATUS_FAILED if row.error_code.as_deref() == Some("CANCELLED") => "cancelled",
            STATUS_FAILED | STATUS_ORPHANED => "failed",
            STATUS_REMOVING | STATUS_REMOVED => "removed",
            _ if row.error_code.as_deref() == Some(SLOT_START_FAILED) => "failed",
            _ => match row.conversation_id.as_deref() {
                None if GROUP_TERMINAL.contains(&group.status.as_str()) => "cancelled",
                None if !self.inner.runs.contains_key(&group.id) => "failed",
                None => "starting",
                Some(conv) if runtimes.get(conv).is_some_and(|r| r.busy) => "running",
                Some(_) => "finished",
            },
        };
        status.into()
    }

    fn respond(
        &self,
        group: FanoutGroupRow,
        rows: &[WorktreeRow],
        runtimes: &HashMap<String, SlotRuntime>,
    ) -> FanoutGroupResponse {
        let slots = rows
            .iter()
            .map(|row| {
                let spec: Option<FanoutSlotRequest> = serde_json::from_str(&row.slot_spec).ok();
                FanoutSlotResponse {
                    slot: row.slot.clone().unwrap_or_default(),
                    worktree_id: row.id.clone(),
                    status: self.slot_status(&group, row, runtimes),
                    worktree_status: row.status.clone(),
                    assistant_id: spec.as_ref().map(|s| s.assistant_id.clone()).unwrap_or_default(),
                    model: spec.and_then(|s| s.model),
                    conversation_id: row.conversation_id.clone(),
                    branch: row.branch.clone(),
                    path: row.path.clone(),
                    branch_preserved: row.branch_preserved != 0,
                    error: row.error_code.clone().map(|code| WorktreeErrorInfo {
                        code,
                        message: row.error_message.clone().unwrap_or_default(),
                    }),
                }
            })
            .collect();
        FanoutGroupResponse {
            adopt_result: group.adopt_result.as_deref().and_then(|s| serde_json::from_str(s).ok()),
            id: group.id,
            status: group.status,
            title: group.title,
            source_repo_path: group.source_repo_path,
            base_ref: group.base_ref,
            base_commit: group.base_commit,
            prompt: group.prompt,
            max_parallel: group.max_parallel,
            task_session_id: group.task_session_id,
            winner_slot: group.winner_slot,
            slots,
            created_at: group.created_at,
            updated_at: group.updated_at,
        }
    }

    /// `running → comparing` once no slot is preparing, starting or running.
    /// Returns the (possibly new) group status.
    async fn refresh_comparing(&self, user_id: &str, id: &str) -> Result<String, WorktreeError> {
        let group = self.inner.store.get(user_id, id).await?;
        if group.status != GROUP_RUNNING {
            return Ok(group.status);
        }
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        let runtimes = self.runtimes(user_id, &rows).await;
        let settled = rows.iter().all(|row| {
            !matches!(
                self.slot_status(&group, row, &runtimes).as_str(),
                "preparing" | "starting" | "running"
            )
        });
        if settled
            && self
                .inner
                .store
                .set_status(id, &[GROUP_RUNNING], GROUP_COMPARING, now_ms())
                .await?
        {
            tracing::info!(event = "fanout.comparing", group_id = %id, "every fan-out slot settled");
            self.emit(&group, GROUP_COMPARING, None);
            return Ok(GROUP_COMPARING.into());
        }
        Ok(group.status)
    }

    pub async fn get(&self, user_id: &str, id: &str) -> Result<FanoutGroupResponse, WorktreeError> {
        self.refresh_comparing(user_id, id).await?;
        let group = self.inner.store.get(user_id, id).await?;
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        let runtimes = self.runtimes(user_id, &rows).await;
        Ok(self.respond(group, &rows, &runtimes))
    }

    pub async fn list(
        &self,
        user_id: &str,
        query: ListFanoutGroupsQuery,
    ) -> Result<Vec<FanoutGroupResponse>, WorktreeError> {
        let ts = query
            .task_session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let groups = self.inner.store.list(user_id, ts).await?;
        let mut out = Vec::with_capacity(groups.len());
        for group in groups {
            out.push(self.get(user_id, &group.id).await?);
        }
        Ok(out)
    }

    pub async fn compare(&self, user_id: &str, id: &str) -> Result<FanoutCompareResponse, WorktreeError> {
        let group = self.get(user_id, id).await?;
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        let port = self.port().ok();
        let mut slots = Vec::with_capacity(rows.len());
        let mut files: BTreeMap<String, BTreeMap<String, FanoutFileCell>> = BTreeMap::new();
        for (row, slot) in rows.iter().zip(&group.slots) {
            let diff = if [STATUS_READY, STATUS_UNBOUND].contains(&row.status.as_str()) {
                self.inner.worktrees.diff_stat(user_id, &row.id).await.ok()
            } else {
                None
            };
            if let Some(diff) = &diff {
                for f in &diff.files {
                    files.entry(f.path.clone()).or_default().insert(
                        slot.slot.clone(),
                        FanoutFileCell {
                            status: f.status.clone(),
                            additions: f.additions,
                            deletions: f.deletions,
                        },
                    );
                }
            }
            let summary = match (&port, row.conversation_id.as_deref()) {
                (Some(port), Some(conv)) => port
                    .last_reply(user_id, conv)
                    .await
                    .map(|s| truncate_chars(&s, SUMMARY_MAX_CHARS)),
                _ => None,
            };
            let worktree = self
                .inner
                .worktrees
                .get(user_id, &row.id)
                .await
                .unwrap_or_else(|_| WorktreeResponse::from(row.clone()));
            slots.push(FanoutCompareSlot {
                slot: slot.slot.clone(),
                status: slot.status.clone(),
                worktree,
                assistant_id: slot.assistant_id.clone(),
                model: slot.model.clone(),
                conversation_id: slot.conversation_id.clone(),
                diff,
                summary,
                usage: None,
                tests: None,
            });
        }
        Ok(FanoutCompareResponse {
            group,
            slots,
            files: files
                .into_iter()
                .map(|(path, slots)| FanoutFileRow { path, slots })
                .collect(),
        })
    }

    // ------------------------------------------------------------------
    // Cancel / discard
    // ------------------------------------------------------------------

    /// Stop the orchestrator, pending checkouts and running turns.
    async fn stop_all(&self, group: &FanoutGroupRow, rows: &[WorktreeRow]) {
        if let Some(token) = self.inner.runs.get(&group.id).map(|t| t.clone()) {
            token.cancel();
        }
        for row in rows {
            self.inner.worktrees.cancel_if_creating(&group.user_id, &row.id).await;
        }
        if let Ok(port) = self.port() {
            for row in rows {
                if let Some(conv) = row.conversation_id.as_deref() {
                    self.stop_turn(&*port, &group.user_id, conv).await;
                }
            }
        }
    }

    async fn stop_turn(&self, port: &dyn IFanoutConversations, user_id: &str, conversation_id: &str) {
        if !port.runtime(user_id, conversation_id).await.busy {
            return;
        }
        if let Err(e) = port.cancel_turn(user_id, conversation_id).await {
            tracing::warn!(conversation_id, error = %e, "fan-out: cancel slot turn failed");
        }
        let deadline = tokio::time::Instant::now() + TURN_STOP_WAIT;
        while port.runtime(user_id, conversation_id).await.busy && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn cancel(&self, user_id: &str, id: &str) -> Result<FanoutGroupResponse, WorktreeError> {
        let _guard = self.lock(id, "cancel")?;
        let group = self.inner.store.get(user_id, id).await?;
        let live = [GROUP_PREPARING, GROUP_RUNNING, GROUP_COMPARING];
        if !live.contains(&group.status.as_str()) {
            return Err(WorktreeError::FanoutInvalidState {
                status: group.status,
                action: "cancel",
            });
        }
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        self.stop_all(&group, &rows).await;
        if self
            .inner
            .store
            .set_status(id, &live, GROUP_CANCELLED, now_ms())
            .await?
        {
            tracing::info!(event = "fanout.cancelled", group_id = %id, user_id, "fan-out group cancelled");
            self.emit(&group, GROUP_CANCELLED, None);
        }
        self.get(user_id, id).await
    }

    pub async fn discard(&self, user_id: &str, id: &str) -> Result<FanoutGroupResponse, WorktreeError> {
        let _guard = self.lock(id, "discard")?;
        let group = self.inner.store.get(user_id, id).await?;
        if [GROUP_ADOPTED, GROUP_DISCARDED].contains(&group.status.as_str()) {
            return Err(WorktreeError::FanoutInvalidState {
                status: group.status,
                action: "discard",
            });
        }
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        self.stop_all(&group, &rows).await;
        let mut result = AdoptResult::default();
        for row in &rows {
            self.remove_slot(user_id, row, false, &mut result).await;
        }
        let from = [
            GROUP_PREPARING,
            GROUP_RUNNING,
            GROUP_COMPARING,
            GROUP_CANCELLED,
            GROUP_FAILED,
        ];
        if self
            .inner
            .store
            .set_status(id, &from, GROUP_DISCARDED, now_ms())
            .await?
        {
            tracing::info!(
                event = "fanout.discarded",
                group_id = %id,
                user_id,
                preserved_branches = ?result.preserved_branches,
                "fan-out group discarded"
            );
            self.emit(&group, GROUP_DISCARDED, None);
        }
        self.get(user_id, id).await
    }

    /// Stop, release and remove one slot worktree (directory force-removed;
    /// the branch is kept when it has unmerged commits or `keep_branch`).
    async fn remove_slot(&self, user_id: &str, row: &WorktreeRow, keep_branch: bool, out: &mut AdoptResult) {
        let worktrees = &self.inner.worktrees;
        worktrees.cancel_if_creating(user_id, &row.id).await;
        if let (Ok(port), Some(conv)) = (self.port(), row.conversation_id.as_deref()) {
            self.stop_turn(&*port, user_id, conv).await;
        }
        if let Err(e) = worktrees.release_slot(user_id, &row.id).await {
            out.cleanup_errors.push(SlotCleanupError {
                worktree_id: row.id.clone(),
                message: e.to_string(),
            });
            return;
        }
        let current = match worktrees.store().get(user_id, &row.id).await {
            Ok(r) => r,
            Err(e) => {
                out.cleanup_errors.push(SlotCleanupError {
                    worktree_id: row.id.clone(),
                    message: e.to_string(),
                });
                return;
            }
        };
        if current.status == STATUS_REMOVED {
            return;
        }
        let opts = RemoveWorktreeQuery {
            branch: if keep_branch {
                BranchPolicy::Keep
            } else {
                BranchPolicy::Auto
            },
            force_dir: true,
        };
        if let Err(e) = worktrees.remove(user_id, &row.id, opts).await {
            out.cleanup_errors.push(SlotCleanupError {
                worktree_id: row.id.clone(),
                message: e.to_string(),
            });
            return;
        }
        worktrees.wait_idle(&row.id).await;
        match worktrees.store().get(user_id, &row.id).await {
            Ok(done) if done.status == STATUS_REMOVED => {
                out.removed_worktrees.push(done.id.clone());
                if done.branch_preserved != 0 {
                    out.preserved_branches.push(done.branch);
                }
            }
            Ok(done) => out.cleanup_errors.push(SlotCleanupError {
                worktree_id: done.id,
                message: done.error_message.unwrap_or_else(|| "worktree was not removed".into()),
            }),
            Err(e) => out.cleanup_errors.push(SlotCleanupError {
                worktree_id: row.id.clone(),
                message: e.to_string(),
            }),
        }
    }

    // ------------------------------------------------------------------
    // Adopt
    // ------------------------------------------------------------------

    pub async fn adopt(
        &self,
        user_id: &str,
        id: &str,
        req: AdoptFanoutRequest,
    ) -> Result<FanoutGroupResponse, WorktreeError> {
        let _guard = self.lock(id, "adopt")?;
        self.refresh_comparing(user_id, id).await?;
        let group = self.inner.store.get(user_id, id).await?;
        let from = [GROUP_RUNNING, GROUP_COMPARING];
        if !from.contains(&group.status.as_str()) {
            return Err(WorktreeError::FanoutInvalidState {
                status: group.status,
                action: "adopt",
            });
        }
        let rows = self.inner.worktrees.store().list_by_group(id).await?;
        let label = req.slot.trim().to_ascii_uppercase();
        let winner = rows
            .iter()
            .find(|r| r.slot.as_deref() == Some(label.as_str()))
            .cloned()
            .ok_or_else(|| WorktreeError::BadRequest(format!("unknown slot: {}", req.slot)))?;
        if ![STATUS_READY, STATUS_UNBOUND].contains(&winner.status.as_str()) {
            return Err(WorktreeError::InvalidState {
                status: winner.status,
                action: "adopt",
            });
        }
        let port = self.port()?;
        if let Some(conv) = winner.conversation_id.as_deref()
            && port.runtime(user_id, conv).await.busy
        {
            return Err(WorktreeError::SlotBusy(label));
        }

        let repo = Path::new(&group.source_repo_path).to_path_buf();
        let wt_path = Path::new(&winner.path).to_path_buf();
        let message = req
            .snapshot_message
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| format!("{} (fan-out slot {label})", group.title));

        // Target checks come first: a rejected adopt changes nothing.
        let target_branch = if req.strategy == AdoptStrategy::BranchOnly {
            None
        } else {
            Some(self.check_target(&repo, req.target_branch.as_deref()).await?)
        };

        // Snapshot uncommitted winner work onto its branch.
        let mut snapshot_commit = None;
        if self.inner.worktrees.is_dirty(&wt_path).await {
            self.git(&wt_path, "add", &["add", "-A"]).await?;
            let identity = self.identity_args(&wt_path).await;
            let mut args: Vec<&str> = identity.to_vec();
            args.extend(["commit", "--no-verify", "-q", "-m", message.as_str()]);
            self.git(&wt_path, "commit", &args).await?;
            snapshot_commit = Some(self.rev_parse(&wt_path, "HEAD").await?);
        }
        let branch_commit = self
            .inner
            .worktrees
            .branch_tip(&repo, &winner.branch)
            .await?
            .ok_or_else(|| WorktreeError::RefNotFound(winner.branch.clone()))?;
        if target_branch.is_some() && branch_commit == group.base_commit {
            return Err(WorktreeError::BadRequest(format!(
                "slot {label} has no changes to adopt"
            )));
        }

        let result_commit = match (req.strategy, target_branch.as_deref()) {
            (AdoptStrategy::BranchOnly, _) | (_, None) => None,
            (strategy, Some(_)) => Some(
                self.merge_into_target(&repo, strategy, &winner, &group, &message)
                    .await?,
            ),
        };

        let mut result = AdoptResult {
            slot: label.clone(),
            strategy: req.strategy,
            branch: winner.branch.clone(),
            branch_commit,
            snapshot_commit,
            target_branch,
            result_commit,
            ..AdoptResult::default()
        };
        for row in rows.iter().filter(|r| r.id != winner.id) {
            if req.cleanup.remove_others {
                self.remove_slot(user_id, row, req.cleanup.keep_branches, &mut result)
                    .await;
            } else if let Some(conv) = row.conversation_id.as_deref() {
                self.stop_turn(&*port, user_id, conv).await;
            }
        }
        if let Some(token) = self.inner.runs.get(id).map(|t| t.clone()) {
            token.cancel();
        }
        let json = serde_json::to_string(&result).unwrap_or_else(|_| "{}".into());
        if !self.inner.store.set_adopted(id, &from, &label, &json, now_ms()).await? {
            let now = self.inner.store.get(user_id, id).await?;
            return Err(WorktreeError::FanoutInvalidState {
                status: now.status,
                action: "adopt",
            });
        }
        tracing::info!(
            event = "fanout.adopted",
            group_id = %id,
            user_id,
            slot = %label,
            strategy = req.strategy.as_str(),
            result_commit = ?result.result_commit,
            preserved_branches = ?result.preserved_branches,
            "fan-out slot adopted"
        );
        self.emit(&group, GROUP_ADOPTED, Some((&label, "adopted")));
        self.inner
            .worktrees
            .trace(
                &winner,
                "fanout.adopted",
                serde_json::json!({ "fanout_group_id": id, "adopt_result": result }),
            )
            .await;
        self.get(user_id, id).await
    }

    /// The merge target must be the branch checked out in the source
    /// repository, with no tracked changes and no operation in progress.
    async fn check_target(&self, repo: &Path, requested: Option<&str>) -> Result<String, WorktreeError> {
        let out = self
            .inner
            .worktrees
            .git()
            .run(["symbolic-ref", "--quiet", "--short", "HEAD"], RunOptions::in_dir(repo))
            .await?;
        let current = out.stdout.trim().to_owned();
        if !out.success() || current.is_empty() {
            return Err(WorktreeError::TargetNotClean(format!(
                "{} has a detached HEAD; check out the target branch first",
                path_string(repo)
            )));
        }
        let target = requested.map(str::trim).filter(|s| !s.is_empty()).unwrap_or(&current);
        if target != current {
            return Err(WorktreeError::TargetNotClean(format!(
                "{target} is not checked out in {} (current branch: {current})",
                path_string(repo)
            )));
        }
        let git_dir = self.rev_parse_path(repo, "--git-dir").await?;
        for marker in [
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "rebase-merge",
            "rebase-apply",
        ] {
            if git_dir.join(marker).exists() {
                return Err(WorktreeError::TargetNotClean(format!(
                    "a {marker} operation is in progress in {}",
                    path_string(repo)
                )));
            }
        }
        let status = self
            .inner
            .worktrees
            .git()
            .run(
                ["status", "--porcelain=v1", "--untracked-files=no"],
                RunOptions::in_dir(repo),
            )
            .await?;
        if !status.success() || !status.stdout.trim().is_empty() {
            return Err(WorktreeError::TargetNotClean(format!(
                "{current} in {} has uncommitted changes",
                path_string(repo)
            )));
        }
        Ok(current)
    }

    /// Merge the winner's branch into the checked-out target. On a conflict
    /// the operation is aborted (no `MERGE_HEAD` / `CHERRY_PICK_HEAD` left)
    /// and `ADOPT_CONFLICT` lists the conflicting paths.
    async fn merge_into_target(
        &self,
        repo: &Path,
        strategy: AdoptStrategy,
        winner: &WorktreeRow,
        group: &FanoutGroupRow,
        message: &str,
    ) -> Result<String, WorktreeError> {
        let identity = self.identity_args(repo).await;
        let range = format!("{}..{}", group.base_commit, winner.branch);
        let mut args: Vec<&str> = identity.to_vec();
        let abort: &[&str] = match strategy {
            AdoptStrategy::Squash => {
                args.extend(["merge", "--squash", winner.branch.as_str()]);
                &["reset", "--merge"]
            }
            AdoptStrategy::NoFf => {
                args.extend(["merge", "--no-ff", "--no-verify", "-m", message, winner.branch.as_str()]);
                &["merge", "--abort"]
            }
            AdoptStrategy::CherryPick => {
                args.extend(["cherry-pick", range.as_str()]);
                &["cherry-pick", "--abort"]
            }
            AdoptStrategy::BranchOnly => unreachable!("branch_only never merges"),
        };
        let out = self.inner.worktrees.git().run(&args, RunOptions::in_dir(repo)).await?;
        if !out.success() {
            let conflicts = self
                .inner
                .worktrees
                .git()
                .run(["diff", "--name-only", "--diff-filter=U"], RunOptions::in_dir(repo))
                .await
                .map(|o| o.stdout.lines().map(str::to_owned).collect::<Vec<_>>())
                .unwrap_or_default();
            let aborted = self.inner.worktrees.git().run(abort, RunOptions::in_dir(repo)).await;
            if !aborted.as_ref().is_ok_and(|o| o.success()) {
                tracing::warn!(repo = %path_string(repo), "fan-out adopt: abort after conflict failed");
            }
            let detail = if conflicts.is_empty() {
                out.stderr.trim().to_owned()
            } else {
                format!("conflicting files: {}", conflicts.join(", "))
            };
            tracing::info!(
                event = "fanout.adopt_conflict",
                group_id = %group.id,
                strategy = strategy.as_str(),
                detail = %detail,
                "fan-out adopt conflicted; target restored"
            );
            return Err(WorktreeError::AdoptConflict(detail));
        }
        if strategy == AdoptStrategy::Squash {
            let mut commit: Vec<&str> = identity.to_vec();
            commit.extend(["commit", "--no-verify", "-q", "-m", message]);
            if let Err(err) = self.git(repo, "commit", &commit).await {
                let _ = self
                    .inner
                    .worktrees
                    .git()
                    .run(["reset", "--merge"], RunOptions::in_dir(repo))
                    .await;
                return Err(err);
            }
        }
        self.rev_parse(repo, "HEAD").await
    }

    async fn identity_args(&self, dir: &Path) -> &'static [&'static str] {
        let git = self.inner.worktrees.git();
        for key in ["user.name", "user.email"] {
            let configured = git
                .run(["config", "--get", key], RunOptions::in_dir(dir))
                .await
                .is_ok_and(|o| o.success() && !o.stdout.trim().is_empty());
            if !configured {
                return &FALLBACK_IDENTITY;
            }
        }
        &[]
    }

    async fn git(&self, dir: &Path, op: &str, args: &[&str]) -> Result<(), WorktreeError> {
        let out = self.inner.worktrees.git().run(args, RunOptions::in_dir(dir)).await?;
        if out.success() {
            Ok(())
        } else {
            Err(WorktreeError::Git(format!("git {op}: {}", out.stderr.trim())))
        }
    }

    async fn rev_parse(&self, dir: &Path, rev: &str) -> Result<String, WorktreeError> {
        let out = self
            .inner
            .worktrees
            .git()
            .run(["rev-parse", "--verify", rev], RunOptions::in_dir(dir))
            .await?;
        if out.success() {
            Ok(out.stdout.trim().to_owned())
        } else {
            Err(WorktreeError::Git(format!(
                "git rev-parse {rev}: {}",
                out.stderr.trim()
            )))
        }
    }

    async fn rev_parse_path(&self, dir: &Path, flag: &str) -> Result<std::path::PathBuf, WorktreeError> {
        let out = self
            .inner
            .worktrees
            .git()
            .run(["rev-parse", "--path-format=absolute", flag], RunOptions::in_dir(dir))
            .await?;
        if out.success() {
            Ok(std::path::PathBuf::from(out.stdout.trim()))
        } else {
            Err(WorktreeError::Git(format!(
                "git rev-parse {flag}: {}",
                out.stderr.trim()
            )))
        }
    }

    // ------------------------------------------------------------------
    // Boot
    // ------------------------------------------------------------------

    /// A group left `preparing` by a previous process lost its orchestrator:
    /// mark it `failed` (its worktrees stay for discard / the manager).
    pub async fn recover_on_boot(&self) -> Result<usize, WorktreeError> {
        let groups = self.inner.store.list_any_in_status(&[GROUP_PREPARING]).await?;
        let mut n = 0;
        for group in groups {
            if self.inner.runs.contains_key(&group.id) {
                continue;
            }
            if self
                .inner
                .store
                .set_status(&group.id, &[GROUP_PREPARING], GROUP_FAILED, now_ms())
                .await?
            {
                n += 1;
            }
        }
        if n > 0 {
            tracing::info!(interrupted_groups = n, "fan-out boot recovery");
        }
        Ok(n)
    }

    /// Wait until the group's orchestrator has finished starting slots and
    /// watching them (tests).
    pub async fn wait_settled(&self, id: &str) {
        while self.inner.runs.contains_key(id) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((idx, _)) => format!("{}…", &s[..idx]),
        None => s.to_owned(),
    }
}
