//! Batch job API surface: create / list / get / items / trial / scale /
//! retry / cancel / CSV export / boot recovery. Routes stay thin wrappers.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use aionui_api_types::{
    BATCH_MODE_AGENT, BatchCheckResult, BatchCounts, BatchItemResponse, BatchJobResponse, BatchListQuery,
    BatchRetryRequest, BatchTemplate, CreateBatchJobRequest, batch_item_status as item_status,
    batch_job_status as job_status,
};
use aionui_common::now_ms;
use aionui_db::{
    BatchItemRow, BatchJobListFilter, BatchJobRow, CreateBatchItemParams, CreateBatchJobParams, IBatchRepository,
};
use serde_json::Value;

use crate::error::BatchError;
use crate::port::{BatchEvent, IBatchExecutor};
use crate::runner::{BatchRunner, BatchRunnerConfig};
use crate::validate::{TargetLocation, target_location, validate_create};

pub const DEFAULT_LIST_LIMIT: i64 = 50;
pub const MAX_LIST_LIMIT: i64 = 100;
/// `error` on items found `running` at boot.
pub const INTERRUPTED_ERROR: &str = "interrupted by an app restart";

pub struct BatchService {
    repo: Arc<dyn IBatchRepository>,
    exec: Arc<dyn IBatchExecutor>,
    runner: Arc<BatchRunner>,
}

impl BatchService {
    pub fn new(repo: Arc<dyn IBatchRepository>, exec: Arc<dyn IBatchExecutor>, cfg: BatchRunnerConfig) -> Self {
        let runner = BatchRunner::new(repo.clone(), exec.clone(), cfg);
        Self { repo, exec, runner }
    }

    pub fn runner(&self) -> &Arc<BatchRunner> {
        &self.runner
    }

    /// Validates and stores a `draft` job; nothing runs until `start_trial`.
    pub async fn create(&self, user_id: &str, req: CreateBatchJobRequest) -> Result<BatchJobResponse, BatchError> {
        let v = validate_create(req)?;
        self.exec.check_assistant(user_id, &v.assistant_id).await?;
        let mut checked = BTreeSet::new();
        for (i, loc) in v.locations.iter().enumerate() {
            if matches!(loc, TargetLocation::Workspace(_)) && v.locations[..i].contains(loc) {
                // Two in-place items in one directory would race each other.
                return Err(BatchError::BadRequest(format!(
                    "targets[{i}]: workspace '{}' is used by more than one target",
                    loc.path().display()
                )));
            }
            if checked.insert((matches!(loc, TargetLocation::Repo(_)), loc.path().to_path_buf())) {
                self.exec
                    .check_target(loc)
                    .await
                    .map_err(|e| BatchError::BadRequest(format!("targets[{i}]: {e}")))?;
            }
        }

        let template = serde_json::to_string(&BatchTemplate {
            prompt: v.prompt.clone(),
        })
        .map_err(|e| BatchError::Internal(e.to_string()))?;
        let checks = serde_json::to_string(&v.checks).map_err(|e| BatchError::Internal(e.to_string()))?;
        let job = self
            .repo
            .create_job(&CreateBatchJobParams {
                user_id,
                title: &v.title,
                project_id: v.project_id.as_deref(),
                template: &template,
                agent_type: v.agent_type.as_deref(),
                assistant_id: Some(&v.assistant_id),
                mode: BATCH_MODE_AGENT,
                concurrency: v.concurrency as i64,
                item_timeout_sec: v.item_timeout_sec as i64,
                checks: &checks,
                status: job_status::DRAFT,
                trial_size: v.trial_size as i64,
                created_at: now_ms(),
            })
            .await?;
        let targets: Vec<String> = v.targets.iter().map(Value::to_string).collect();
        let params: Vec<CreateBatchItemParams<'_>> = targets
            .iter()
            .enumerate()
            .map(|(i, target)| CreateBatchItemParams {
                seq: i as i64 + 1,
                target,
                status: item_status::PENDING,
            })
            .collect();
        let items = self.repo.insert_items(user_id, &job.id, &params).await?;
        Ok(job_response(job, &items))
    }

    pub async fn list(&self, user_id: &str, query: &BatchListQuery) -> Result<Vec<BatchJobResponse>, BatchError> {
        let filter = BatchJobListFilter {
            before_updated_at: query.before,
            limit: query.limit.unwrap_or(DEFAULT_LIST_LIMIT).clamp(1, MAX_LIST_LIMIT),
        };
        let jobs = self.repo.list_jobs(user_id, &filter).await?;
        let mut out = Vec::with_capacity(jobs.len());
        for job in jobs {
            let items = self.repo.list_items(user_id, &job.id).await?;
            out.push(job_response(job, &items));
        }
        Ok(out)
    }

    pub async fn get(&self, user_id: &str, job_id: &str) -> Result<BatchJobResponse, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let items = self.repo.list_items(user_id, job_id).await?;
        Ok(job_response(job, &items))
    }

    pub async fn items(&self, user_id: &str, job_id: &str) -> Result<Vec<BatchItemResponse>, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let items = self.repo.list_items(user_id, job_id).await?;
        Ok(items.into_iter().map(|i| item_response(i, job.trial_size)).collect())
    }

    /// `draft` -> `trial`: runs the first `trial_size` items (D8-2).
    pub async fn start_trial(&self, user_id: &str, job_id: &str) -> Result<BatchJobResponse, BatchError> {
        let job = self.job(user_id, job_id).await?;
        if job.status != job_status::DRAFT {
            return Err(invalid(&job, "start the trial of"));
        }
        if let Some(assistant) = job.assistant_id.as_deref() {
            self.exec.check_assistant(user_id, assistant).await?;
        }
        let items = self.repo.list_items(user_id, job_id).await?;
        let trial: Vec<&BatchItemRow> = items.iter().filter(|i| i.seq <= job.trial_size).collect();
        self.precheck_disk(&job, &trial).await?;
        self.transition(user_id, &job, &[job_status::DRAFT], job_status::TRIAL)
            .await?;
        self.runner.ensure_driver(user_id, job_id).await;
        self.get(user_id, job_id).await
    }

    /// `trial` (every trial item passed) or `paused` -> `running`.
    pub async fn scale(&self, user_id: &str, job_id: &str) -> Result<BatchJobResponse, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let items = self.repo.list_items(user_id, job_id).await?;
        match job.status.as_str() {
            job_status::TRIAL => {
                let trial: Vec<&BatchItemRow> = items.iter().filter(|i| i.seq <= job.trial_size).collect();
                if trial.iter().any(|i| is_open(&i.status)) {
                    return Err(BatchError::InvalidState("the trial is still running".into()));
                }
                if !trial.iter().all(|i| i.status == item_status::PASSED) {
                    return Err(BatchError::InvalidState(
                        "every trial item must pass before scaling; retry the trial first".into(),
                    ));
                }
            }
            job_status::PAUSED => {}
            _ => return Err(invalid(&job, "scale")),
        }
        let pending: Vec<&BatchItemRow> = items.iter().filter(|i| i.status == item_status::PENDING).collect();
        self.precheck_disk(&job, &pending).await?;
        self.transition(
            user_id,
            &job,
            &[job_status::TRIAL, job_status::PAUSED],
            job_status::RUNNING,
        )
        .await?;
        self.runner.ensure_driver(user_id, job_id).await;
        self.get(user_id, job_id).await
    }

    /// Resets failed / timed-out items (or the given failed / timed-out /
    /// cancelled items) to pending and runs them again. A finished or
    /// paused job goes back to `running`; a `trial` job re-runs its trial.
    pub async fn retry(
        &self,
        user_id: &str,
        job_id: &str,
        req: &BatchRetryRequest,
    ) -> Result<BatchJobResponse, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let allowed = [
            job_status::TRIAL,
            job_status::RUNNING,
            job_status::PAUSED,
            job_status::FAILED,
            job_status::COMPLETED,
        ];
        if !allowed.contains(&job.status.as_str()) {
            return Err(invalid(&job, "retry"));
        }
        let items = self.repo.list_items(user_id, job_id).await?;
        let (ids, from): (Vec<String>, &[&str]) = match (&req.item_ids, req.filter.as_deref()) {
            (Some(_), Some(_)) => return Err(BatchError::BadRequest("pass either item_ids or filter".into())),
            (Some(ids), None) => {
                if ids.is_empty() {
                    return Err(BatchError::BadRequest("item_ids must not be empty".into()));
                }
                if let Some(unknown) = ids.iter().find(|id| !items.iter().any(|i| &i.id == *id)) {
                    return Err(BatchError::NotFound(format!("batch item '{unknown}'")));
                }
                (
                    ids.clone(),
                    &[item_status::FAILED, item_status::TIMEOUT, item_status::CANCELLED],
                )
            }
            (None, None | Some("failed")) => (
                items.iter().map(|i| i.id.clone()).collect(),
                &[item_status::FAILED, item_status::TIMEOUT],
            ),
            (None, Some(other)) => {
                return Err(BatchError::BadRequest(format!(
                    "unknown retry filter '{other}' (expected 'failed')"
                )));
            }
        };
        let in_trial = job.status == job_status::TRIAL;
        if in_trial
            && items
                .iter()
                .any(|i| ids.contains(&i.id) && i.seq > job.trial_size && from.contains(&i.status.as_str()))
        {
            return Err(BatchError::InvalidState(
                "only trial items can be retried before scaling".into(),
            ));
        }
        let to_reset: Vec<&BatchItemRow> = items
            .iter()
            .filter(|i| ids.contains(&i.id) && from.contains(&i.status.as_str()))
            .collect();
        if to_reset.is_empty() {
            return Err(BatchError::InvalidState("no retryable items".into()));
        }
        if !in_trial && job.status != job_status::RUNNING {
            self.precheck_disk(&job, &to_reset).await?;
        }
        self.repo.reset_items(user_id, job_id, &ids, from).await?;
        if !in_trial && job.status != job_status::RUNNING {
            self.transition(
                user_id,
                &job,
                &[job_status::PAUSED, job_status::FAILED, job_status::COMPLETED],
                job_status::RUNNING,
            )
            .await?;
        }
        self.runner.ensure_driver(user_id, job_id).await;
        self.get(user_id, job_id).await
    }

    /// Stops the job: pending items are cancelled, in-flight attempts are
    /// told to stop their turn and end as `cancelled`.
    pub async fn cancel(&self, user_id: &str, job_id: &str) -> Result<BatchJobResponse, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let from = [
            job_status::DRAFT,
            job_status::TRIAL,
            job_status::RUNNING,
            job_status::PAUSED,
        ];
        if !from.contains(&job.status.as_str()) {
            return Err(invalid(&job, "cancel"));
        }
        self.transition(user_id, &job, &from, job_status::CANCELLED).await?;
        self.repo.cancel_pending_items(user_id, job_id, now_ms()).await?;
        self.runner.cancel(job_id).await;
        // A live driver settles after its in-flight attempts end and raises
        // the summary then (its settle runs under the same driver-map lock
        // `is_driving` takes); with no driver nothing else will. A driver that
        // settles in the gap may report too: the upsert on `batch:{id}` makes
        // that a refresh of the same row, not a second item.
        if !self.runner.is_driving(job_id).await {
            self.runner.report_summary(user_id, job_id).await;
        }
        self.get(user_id, job_id).await
    }

    /// One row per item: seq, status, attempt, target, task session, checks
    /// passed / total, error, started / finished (epoch ms).
    pub async fn export_csv(&self, user_id: &str, job_id: &str) -> Result<String, BatchError> {
        let job = self.job(user_id, job_id).await?;
        let items = self.repo.list_items(user_id, job_id).await?;
        let mut out = String::from(
            "seq,status,attempt,trial,target,task_session_id,checks_passed,checks_total,error,started_at,finished_at\n",
        );
        for item in items {
            let trial = item.seq <= job.trial_size;
            let resp = item_response(item, job.trial_size);
            let passed = resp.check_results.iter().filter(|c| c.ok).count();
            let fields = [
                resp.seq.to_string(),
                resp.status,
                resp.attempt.to_string(),
                trial.to_string(),
                resp.target.to_string(),
                resp.task_session_id.unwrap_or_default(),
                passed.to_string(),
                resp.check_results.len().to_string(),
                resp.error.unwrap_or_default(),
                resp.started_at.map(|t| t.to_string()).unwrap_or_default(),
                resp.finished_at.map(|t| t.to_string()).unwrap_or_default(),
            ];
            let row: Vec<String> = fields.iter().map(|f| csv_field(f)).collect();
            out.push_str(&row.join(","));
            out.push('\n');
        }
        Ok(out)
    }

    /// Boot: attempts that were in flight are failed (retryable) and
    /// `running` jobs are paused so nothing restarts without the user.
    pub async fn recover_on_boot(&self) -> Result<(u64, u64), BatchError> {
        Ok(self.repo.recover_interrupted(INTERRUPTED_ERROR, now_ms()).await?)
    }

    async fn job(&self, user_id: &str, job_id: &str) -> Result<BatchJobRow, BatchError> {
        self.repo
            .get_job(user_id, job_id)
            .await?
            .ok_or_else(|| BatchError::NotFound(job_id.to_string()))
    }

    async fn transition(&self, user_id: &str, job: &BatchJobRow, from: &[&str], to: &str) -> Result<(), BatchError> {
        if !self
            .repo
            .transition_job_status(user_id, &job.id, from, to, now_ms())
            .await?
        {
            return Err(BatchError::InvalidState(format!(
                "batch job '{}' changed state concurrently",
                job.id
            )));
        }
        self.exec.notify(
            user_id,
            BatchEvent::JobUpdated {
                job_id: job.id.clone(),
                status: to.to_string(),
            },
        );
        Ok(())
    }

    /// D8 disk precheck for the repo targets about to run.
    async fn precheck_disk(&self, job: &BatchJobRow, items: &[&BatchItemRow]) -> Result<(), BatchError> {
        let mut repos = BTreeSet::new();
        for item in items {
            if let Ok(target) = serde_json::from_str::<Value>(&item.target)
                && let Ok(TargetLocation::Repo(path)) = target_location(&target)
            {
                repos.insert(path);
            }
        }
        if repos.is_empty() {
            return Ok(());
        }
        let repos: Vec<PathBuf> = repos.into_iter().collect();
        let concurrency = (job.concurrency.max(1) as usize).min(items.len()) as u32;
        self.exec.disk_precheck(&repos, concurrency).await
    }
}

fn is_open(status: &str) -> bool {
    status == item_status::PENDING || status == item_status::RUNNING
}

fn invalid(job: &BatchJobRow, action: &str) -> BatchError {
    BatchError::InvalidState(format!("cannot {action} a batch job in status '{}'", job.status))
}

fn csv_field(value: &str) -> String {
    // Neutralise spreadsheet formula injection, then RFC 4180 quoting.
    let value = if value.starts_with(['=', '+', '-', '@']) && value.parse::<f64>().is_err() {
        format!("'{value}")
    } else {
        value.to_string()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

pub fn item_response(row: BatchItemRow, trial_size: i64) -> BatchItemResponse {
    let check_results: Vec<BatchCheckResult> = row
        .check_results
        .as_deref()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or_default();
    BatchItemResponse {
        trial: row.seq <= trial_size,
        target: serde_json::from_str(&row.target).unwrap_or(Value::String(row.target)),
        id: row.id,
        batch_job_id: row.batch_job_id,
        seq: row.seq,
        task_session_id: row.task_session_id,
        attempt: row.attempt,
        status: row.status,
        check_results,
        error: row.error,
        started_at: row.started_at,
        finished_at: row.finished_at,
    }
}

pub fn job_response(job: BatchJobRow, items: &[BatchItemRow]) -> BatchJobResponse {
    let mut counts = BatchCounts {
        total: items.len() as u32,
        ..Default::default()
    };
    for item in items {
        match item.status.as_str() {
            item_status::PENDING => counts.pending += 1,
            item_status::RUNNING => counts.running += 1,
            item_status::PASSED => counts.passed += 1,
            item_status::FAILED => counts.failed += 1,
            item_status::TIMEOUT => counts.timeout += 1,
            item_status::SKIPPED => counts.skipped += 1,
            item_status::CANCELLED => counts.cancelled += 1,
            _ => {}
        }
    }
    let trial_settled = job.status != job_status::DRAFT
        && items
            .iter()
            .filter(|i| i.seq <= job.trial_size)
            .all(|i| !is_open(&i.status));
    let parse = |s: &str| serde_json::from_str(s).unwrap_or(Value::String(s.to_string()));
    BatchJobResponse {
        template: parse(&job.template),
        checks: parse(&job.checks),
        id: job.id,
        title: job.title,
        project_id: job.project_id,
        agent_type: job.agent_type,
        assistant_id: job.assistant_id,
        mode: job.mode,
        concurrency: job.concurrency,
        item_timeout_sec: job.item_timeout_sec,
        status: job.status,
        trial_size: job.trial_size,
        counts,
        trial_settled,
        created_at: job.created_at,
        updated_at: job.updated_at,
    }
}
