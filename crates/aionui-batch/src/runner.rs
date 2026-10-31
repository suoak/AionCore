//! BatchRunner: one driver task per active job.
//!
//! The driver launches the job's pending items in scope (`trial`: items with
//! `seq <= trial_size`; `running`: every item) up to the job's concurrency,
//! and settles the job once nothing is pending or in flight in scope:
//! a `trial` job stays `trial` (the user decides to scale), a `running` job
//! becomes `completed` when every item passed / was skipped, else `failed`.
//!
//! Each item attempt: TaskSession -> `start_item` (CAS pending -> running) ->
//! worktree / conversation -> prompt -> wait for the turn to end (bounded by
//! `item_timeout_sec`) -> artifact checks in the item workspace -> verdict.
//! The verdict only ever comes from the checks, never from the agent.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use aionui_api_types::{
    BatchCheckResult, BatchCheckSpec, BatchTemplate, batch_item_status as item_status, batch_job_status as job_status,
};
use aionui_common::now_ms;
use aionui_db::{BatchItemRow, BatchJobRow, FinishBatchItemParams, IBatchRepository};
use serde_json::Value;
use tokio::sync::{Mutex, Notify, watch};
use tokio::task::JoinSet;
use tracing::{debug, warn};

use crate::checks::{CompiledChecks, all_passed};
use crate::error::BatchError;
use crate::port::{AttemptOutcome, BatchEvent, IBatchExecutor, ItemSpec};
use crate::summary::summarize;
use crate::template::PromptTemplate;
use crate::validate::target_location;

#[derive(Debug, Clone)]
pub struct BatchRunnerConfig {
    /// How often an in-flight attempt polls its conversation.
    pub poll_interval: Duration,
    /// An attempt whose conversation never reports busy is treated as done
    /// after this long (the turn may finish before the first poll).
    pub start_grace: Duration,
    /// Wall-clock length of one `item_timeout_sec` unit (1s in production;
    /// shortened by tests).
    pub timeout_unit: Duration,
}

impl Default for BatchRunnerConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(2),
            start_grace: Duration::from_secs(10),
            timeout_unit: Duration::from_secs(1),
        }
    }
}

struct Driver {
    wake: Notify,
    cancel: watch::Sender<bool>,
}

/// Immutable per-job execution context, parsed once per driver.
struct JobCtx {
    title: String,
    template: PromptTemplate,
    checks: Arc<CompiledChecks>,
    assistant_id: String,
    agent_type: Option<String>,
    project_id: Option<String>,
    timeout: Duration,
    concurrency: usize,
}

impl JobCtx {
    fn from_row(job: &BatchJobRow, timeout_unit: Duration) -> Result<Self, BatchError> {
        let template: BatchTemplate = serde_json::from_str(&job.template)
            .map_err(|e| BatchError::Internal(format!("stored template is invalid: {e}")))?;
        let specs: Vec<BatchCheckSpec> = serde_json::from_str(&job.checks)
            .map_err(|e| BatchError::Internal(format!("stored checks are invalid: {e}")))?;
        let assistant_id = job
            .assistant_id
            .clone()
            .ok_or_else(|| BatchError::Internal("job has no assistant".into()))?;
        Ok(Self {
            title: job.title.clone(),
            template: PromptTemplate::parse(&template.prompt)?,
            checks: Arc::new(CompiledChecks::compile(&specs)?),
            assistant_id,
            agent_type: job.agent_type.clone(),
            project_id: job.project_id.clone(),
            timeout: timeout_unit.saturating_mul(job.item_timeout_sec.clamp(1, u32::MAX as i64) as u32),
            concurrency: job.concurrency.clamp(1, aionui_api_types::BATCH_MAX_CONCURRENCY as i64) as usize,
        })
    }
}

pub struct BatchRunner {
    repo: Arc<dyn IBatchRepository>,
    exec: Arc<dyn IBatchExecutor>,
    cfg: BatchRunnerConfig,
    drivers: Mutex<HashMap<String, Arc<Driver>>>,
}

/// Item statuses that are still owed work.
fn is_open(status: &str) -> bool {
    status == item_status::PENDING || status == item_status::RUNNING
}

/// The highest `seq` the job may launch in its current status, `None` when
/// it must not launch anything.
fn launch_scope(job: &BatchJobRow) -> Option<i64> {
    match job.status.as_str() {
        job_status::TRIAL => Some(job.trial_size),
        job_status::RUNNING => Some(i64::MAX),
        _ => None,
    }
}

impl BatchRunner {
    pub fn new(repo: Arc<dyn IBatchRepository>, exec: Arc<dyn IBatchExecutor>, cfg: BatchRunnerConfig) -> Arc<Self> {
        Arc::new(Self {
            repo,
            exec,
            cfg,
            drivers: Mutex::new(HashMap::new()),
        })
    }

    /// Starts the job's driver, or wakes it to pick up newly pending items.
    pub async fn ensure_driver(self: &Arc<Self>, user_id: &str, job_id: &str) {
        let mut drivers = self.drivers.lock().await;
        if let Some(driver) = drivers.get(job_id) {
            driver.wake.notify_one();
            return;
        }
        let (cancel, _) = watch::channel(false);
        let driver = Arc::new(Driver {
            wake: Notify::new(),
            cancel,
        });
        drivers.insert(job_id.to_string(), driver.clone());
        let this = self.clone();
        let (user_id, job_id) = (user_id.to_string(), job_id.to_string());
        tokio::spawn(async move { this.drive(user_id, job_id, driver).await });
    }

    /// Signals in-flight attempts of the job to stop (the caller has already
    /// moved the job to `cancelled` and cancelled its pending items).
    pub async fn cancel(&self, job_id: &str) {
        if let Some(driver) = self.drivers.lock().await.get(job_id) {
            driver.cancel.send_replace(true);
            driver.wake.notify_one();
        }
    }

    pub async fn is_driving(&self, job_id: &str) -> bool {
        self.drivers.lock().await.contains_key(job_id)
    }

    async fn drive(self: Arc<Self>, user_id: String, job_id: String, driver: Arc<Driver>) {
        let mut cancel_rx = driver.cancel.subscribe();
        let mut in_flight: JoinSet<()> = JoinSet::new();
        let mut task_items: HashMap<tokio::task::Id, String> = HashMap::new();
        let mut launched: HashSet<String> = HashSet::new();
        let mut ctx: Option<Arc<JobCtx>> = None;

        loop {
            let cancelled = *cancel_rx.borrow();
            match self.repo.get_job(&user_id, &job_id).await {
                Ok(Some(job)) if !cancelled => {
                    if let Some(max_seq) = launch_scope(&job) {
                        if ctx.is_none() {
                            match JobCtx::from_row(&job, self.cfg.timeout_unit) {
                                Ok(c) => ctx = Some(Arc::new(c)),
                                Err(e) => {
                                    warn!(job_id = %job_id, error = %e, "batch job cannot run");
                                    self.set_job(
                                        &user_id,
                                        &job_id,
                                        &[job_status::TRIAL, job_status::RUNNING],
                                        job_status::FAILED,
                                    )
                                    .await;
                                }
                            }
                        }
                        if let Some(ctx) = ctx.clone() {
                            match self.repo.list_items(&user_id, &job_id).await {
                                Ok(items) => {
                                    let ready: Vec<BatchItemRow> = items
                                        .into_iter()
                                        .filter(|i| {
                                            i.status == item_status::PENDING
                                                && i.seq <= max_seq
                                                && !launched.contains(&i.id)
                                        })
                                        .collect();
                                    for item in ready {
                                        if in_flight.len() >= ctx.concurrency {
                                            break;
                                        }
                                        launched.insert(item.id.clone());
                                        let this = self.clone();
                                        let (user, ctx, rx) = (user_id.clone(), ctx.clone(), cancel_rx.clone());
                                        let item_id = item.id.clone();
                                        let handle =
                                            in_flight.spawn(async move { this.run_item(user, ctx, item, rx).await });
                                        task_items.insert(handle.id(), item_id);
                                    }
                                }
                                Err(e) => warn!(job_id = %job_id, error = %e, "batch list items failed"),
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => warn!(job_id = %job_id, error = %e, "batch load job failed"),
            }

            if in_flight.is_empty() {
                let mut drivers = self.drivers.lock().await;
                // Re-check under the driver-map lock: a retry / scale that
                // raced the scan either finds this driver still registered
                // (and wakes it) or sees it gone and starts a new one.
                if !*cancel_rx.borrow() && self.has_launchable(&user_id, &job_id).await {
                    drop(drivers);
                    continue;
                }
                self.settle(&user_id, &job_id).await;
                drivers.remove(&job_id);
                debug!(job_id = %job_id, "batch driver stopped");
                return;
            }

            tokio::select! {
                Some(joined) = in_flight.join_next_with_id() => {
                    let (id, failed) = match joined {
                        Ok((id, ())) => (id, false),
                        Err(e) => (e.id(), true),
                    };
                    if let Some(item_id) = task_items.remove(&id) {
                        // Reset (retry) items may be launched again later.
                        launched.remove(&item_id);
                        if failed {
                            warn!(job_id = %job_id, item_id = %item_id, "batch item task panicked");
                            self.finish_row(&user_id, &job_id, &item_id, item_status::FAILED, None, Some("internal error")).await;
                        }
                    }
                }
                _ = driver.wake.notified() => {}
                _ = cancel_rx.changed() => {}
            }
        }
    }

    /// Any pending item in the job's current scope.
    async fn has_launchable(&self, user_id: &str, job_id: &str) -> bool {
        let Ok(Some(job)) = self.repo.get_job(user_id, job_id).await else {
            return false;
        };
        let Some(max_seq) = launch_scope(&job) else {
            return false;
        };
        match self.repo.list_items(user_id, job_id).await {
            Ok(items) => items
                .iter()
                .any(|i| i.status == item_status::PENDING && i.seq <= max_seq),
            Err(_) => false,
        }
    }

    /// Nothing open in scope: a `running` job gets its verdict; a `trial`
    /// job stays `trial` until the user scales or retries.
    async fn settle(&self, user_id: &str, job_id: &str) {
        let Ok(Some(job)) = self.repo.get_job(user_id, job_id).await else {
            return;
        };
        if job.status == job_status::RUNNING {
            let Ok(items) = self.repo.list_items(user_id, job_id).await else {
                return;
            };
            if items.iter().any(|i| is_open(&i.status)) {
                return;
            }
            let ok = items
                .iter()
                .all(|i| i.status == item_status::PASSED || i.status == item_status::SKIPPED);
            let to = if ok { job_status::COMPLETED } else { job_status::FAILED };
            self.set_job(user_id, job_id, &[job_status::RUNNING], to).await;
        } else {
            let cancelled = job.status == job_status::CANCELLED;
            self.exec.notify(
                user_id,
                BatchEvent::JobUpdated {
                    job_id: job_id.to_string(),
                    status: job.status,
                },
            );
            if cancelled {
                // In-flight attempts have now ended: summarise the final counts.
                self.report_summary(user_id, job_id).await;
            }
        }
    }

    /// Raises the job's Inbox summary if it is in a terminal status.
    pub(crate) async fn report_summary(&self, user_id: &str, job_id: &str) {
        let Ok(Some(job)) = self.repo.get_job(user_id, job_id).await else {
            return;
        };
        let items = match self.repo.list_items(user_id, job_id).await {
            Ok(items) => items,
            Err(e) => {
                warn!(job_id = %job_id, error = %e, "batch summary: list items failed");
                return;
            }
        };
        if let Some(summary) = summarize(&job, &items) {
            self.exec.job_finished(user_id, &summary).await;
        }
    }

    async fn set_job(&self, user_id: &str, job_id: &str, from: &[&str], to: &str) {
        match self
            .repo
            .transition_job_status(user_id, job_id, from, to, now_ms())
            .await
        {
            Ok(true) => {
                self.exec.notify(
                    user_id,
                    BatchEvent::JobUpdated {
                        job_id: job_id.to_string(),
                        status: to.to_string(),
                    },
                );
                if to == job_status::COMPLETED || to == job_status::FAILED {
                    self.report_summary(user_id, job_id).await;
                }
            }
            Ok(false) => {}
            Err(e) => warn!(job_id = %job_id, error = %e, "batch job transition failed"),
        }
    }

    async fn run_item(
        self: Arc<Self>,
        user_id: String,
        ctx: Arc<JobCtx>,
        item: BatchItemRow,
        mut cancel_rx: watch::Receiver<bool>,
    ) {
        let job_id = item.batch_job_id.clone();
        let prepared = (|| -> Result<(ItemSpec, String), BatchError> {
            let target: Value = serde_json::from_str(&item.target)
                .map_err(|e| BatchError::Internal(format!("stored target is invalid: {e}")))?;
            let spec = ItemSpec {
                job_id: job_id.clone(),
                job_title: ctx.title.clone(),
                item_id: item.id.clone(),
                seq: item.seq,
                location: target_location(&target)?,
                assistant_id: ctx.assistant_id.clone(),
                agent_type: ctx.agent_type.clone(),
                project_id: ctx.project_id.clone(),
            };
            let prompt = ctx.template.render(&target, item.seq)?;
            Ok((spec, prompt))
        })();
        let (spec, prompt) = match prepared {
            Ok(v) => v,
            Err(e) => return self.fail_unstarted(&user_id, &item, &e.to_string()).await,
        };
        let session_id = match self.exec.create_session(&user_id, &spec).await {
            Ok(id) => id,
            Err(e) => {
                return self
                    .fail_unstarted(&user_id, &item, &format!("task session: {e}"))
                    .await;
            }
        };
        match self.repo.start_item(&user_id, &item.id, &session_id, now_ms()).await {
            Ok(true) => {}
            Ok(false) | Err(_) => {
                // Cancelled (or reset) between the scan and the start.
                self.exec
                    .finish_session(
                        &user_id,
                        &session_id,
                        AttemptOutcome::Cancelled,
                        "batch item was no longer pending",
                    )
                    .await;
                return;
            }
        }
        self.item_event(&user_id, &job_id, &item.id, item_status::RUNNING);
        let attempt = Attempt {
            user_id: &user_id,
            job_id: &job_id,
            item_id: &item.id,
            session_id: &session_id,
        };

        if *cancel_rx.borrow() {
            return self
                .finish(&attempt, AttemptOutcome::Cancelled, None, Some("cancelled"))
                .await;
        }
        let prepared = match self.exec.prepare(&user_id, &session_id, &spec).await {
            Ok(p) => p,
            Err(e) => {
                return self
                    .finish(&attempt, AttemptOutcome::Failed, None, Some(&format!("prepare: {e}")))
                    .await;
            }
        };
        let started = Instant::now();
        if let Err(e) = self
            .exec
            .send_prompt(&user_id, &prepared.conversation_id, &prompt)
            .await
        {
            return self
                .finish(
                    &attempt,
                    AttemptOutcome::Failed,
                    None,
                    Some(&format!("send prompt: {e}")),
                )
                .await;
        }

        let mut seen_busy = false;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(self.cfg.poll_interval) => {}
                _ = cancel_rx.changed() => {}
            }
            if *cancel_rx.borrow() {
                self.exec.cancel_turn(&user_id, &prepared.conversation_id).await;
                return self
                    .finish(&attempt, AttemptOutcome::Cancelled, None, Some("cancelled"))
                    .await;
            }
            if self.exec.is_busy(&prepared.conversation_id).await {
                seen_busy = true;
            } else if seen_busy || started.elapsed() >= self.cfg.start_grace {
                break;
            }
            if started.elapsed() >= ctx.timeout {
                self.exec.cancel_turn(&user_id, &prepared.conversation_id).await;
                let msg = format!("no result within {}s", ctx.timeout.as_secs());
                return self.finish(&attempt, AttemptOutcome::Timeout, None, Some(&msg)).await;
            }
        }

        let checks = ctx.checks.clone();
        let workspace: PathBuf = prepared.workspace.clone();
        let results = match tokio::task::spawn_blocking(move || checks.evaluate(&workspace)).await {
            Ok(r) => r,
            Err(e) => {
                return self
                    .finish(&attempt, AttemptOutcome::Failed, None, Some(&format!("checks: {e}")))
                    .await;
            }
        };
        if all_passed(&results) {
            self.finish(&attempt, AttemptOutcome::Passed, Some(&results), None)
                .await;
        } else {
            let failed = results.iter().filter(|r| !r.ok).count();
            let msg = format!("{failed} of {} checks failed", results.len());
            self.finish(&attempt, AttemptOutcome::Failed, Some(&results), Some(&msg))
                .await;
        }
    }

    /// The attempt could not even start: fail the item if still pending.
    async fn fail_unstarted(&self, user_id: &str, item: &BatchItemRow, error: &str) {
        warn!(item_id = %item.id, error = %error, "batch item could not start");
        if let Ok(Some(row)) = self.repo.get_item(user_id, &item.id).await
            && row.status == item_status::PENDING
        {
            let now = now_ms();
            if let Ok(true) = self
                .repo
                .set_item_status(
                    user_id,
                    &item.id,
                    item_status::FAILED,
                    Some(error),
                    Some(now),
                    Some(now),
                )
                .await
            {
                self.item_event(user_id, &item.batch_job_id, &item.id, item_status::FAILED);
            }
        }
    }

    async fn finish(
        &self,
        attempt: &Attempt<'_>,
        outcome: AttemptOutcome,
        checks: Option<&[BatchCheckResult]>,
        error: Option<&str>,
    ) {
        let status = match outcome {
            AttemptOutcome::Passed => item_status::PASSED,
            AttemptOutcome::Failed => item_status::FAILED,
            AttemptOutcome::Timeout => item_status::TIMEOUT,
            AttemptOutcome::Cancelled => item_status::CANCELLED,
        };
        let checks_json = checks.and_then(|c| serde_json::to_string(c).ok());
        self.finish_row(
            attempt.user_id,
            attempt.job_id,
            attempt.item_id,
            status,
            checks_json.as_deref(),
            error,
        )
        .await;
        let summary = error.unwrap_or("all checks passed");
        self.exec
            .finish_session(attempt.user_id, attempt.session_id, outcome, summary)
            .await;
    }

    async fn finish_row(
        &self,
        user_id: &str,
        job_id: &str,
        item_id: &str,
        status: &str,
        check_results: Option<&str>,
        error: Option<&str>,
    ) {
        let params = FinishBatchItemParams {
            item_id,
            status,
            check_results,
            error,
            finished_at: now_ms(),
        };
        match self.repo.finish_item(user_id, &params).await {
            Ok(true) => self.item_event(user_id, job_id, item_id, status),
            Ok(false) => {}
            Err(e) => warn!(item_id = %item_id, error = %e, "batch finish item failed"),
        }
    }

    fn item_event(&self, user_id: &str, job_id: &str, item_id: &str, status: &str) {
        self.exec.notify(
            user_id,
            BatchEvent::ItemUpdated {
                job_id: job_id.to_string(),
                item_id: item_id.to_string(),
                status: status.to_string(),
            },
        );
    }
}

struct Attempt<'a> {
    user_id: &'a str,
    job_id: &'a str,
    item_id: &'a str,
    session_id: &'a str,
}
