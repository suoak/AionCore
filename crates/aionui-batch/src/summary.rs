//! End-of-job Inbox summary (design §2.2): one item per job, keyed
//! `batch:{id}` so a re-run refreshes it instead of stacking a second one.

use aionui_api_types::{batch_item_status as item_status, batch_job_status as job_status};
use aionui_db::{BatchItemRow, BatchJobRow};

/// Inbox dedupe key of a job's summary.
pub fn batch_inbox_key(job_id: &str) -> String {
    format!("batch:{job_id}")
}

/// Item counts at the moment the job settled.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchSummaryCounts {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub timeout: u32,
    pub cancelled: u32,
    pub skipped: u32,
}

/// The summary of a job that reached `completed`, `failed` or `cancelled`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchSummary {
    pub job_id: String,
    pub job_title: String,
    /// Terminal job status.
    pub status: String,
    pub counts: BatchSummaryCounts,
    /// `batch:{job_id}`.
    pub dedupe_key: String,
    /// Inbox kind: `failed` when the job failed or any item failed / timed
    /// out, else `completed` (also for a cancelled job with no failures).
    pub kind: &'static str,
    pub title: String,
    pub text: String,
}

/// Terminal job statuses that get a summary.
pub fn is_terminal(status: &str) -> bool {
    matches!(
        status,
        job_status::COMPLETED | job_status::FAILED | job_status::CANCELLED
    )
}

/// Builds the summary of a settled job; `None` while it is not terminal.
pub fn summarize(job: &BatchJobRow, items: &[BatchItemRow]) -> Option<BatchSummary> {
    if !is_terminal(&job.status) {
        return None;
    }
    let mut c = BatchSummaryCounts {
        total: items.len() as u32,
        ..Default::default()
    };
    for item in items {
        match item.status.as_str() {
            item_status::PASSED => c.passed += 1,
            item_status::FAILED => c.failed += 1,
            item_status::TIMEOUT => c.timeout += 1,
            item_status::CANCELLED => c.cancelled += 1,
            item_status::SKIPPED => c.skipped += 1,
            _ => {}
        }
    }
    let failed = job.status == job_status::FAILED || c.failed + c.timeout > 0;
    let verb = match job.status.as_str() {
        job_status::COMPLETED => "completed",
        job_status::FAILED => "failed",
        _ => "cancelled",
    };
    let mut text = format!("{} passed · {} failed · {} timed out", c.passed, c.failed, c.timeout);
    if c.cancelled > 0 {
        text.push_str(&format!(" · {} cancelled", c.cancelled));
    }
    if c.skipped > 0 {
        text.push_str(&format!(" · {} skipped", c.skipped));
    }
    text.push_str(&format!(" (of {})", c.total));
    Some(BatchSummary {
        job_id: job.id.clone(),
        job_title: job.title.clone(),
        status: job.status.clone(),
        counts: c,
        dedupe_key: batch_inbox_key(&job.id),
        kind: if failed { "failed" } else { "completed" },
        title: format!("Batch {verb}: {}", job.title),
        text,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(status: &str) -> BatchJobRow {
        BatchJobRow {
            id: "bj_1".into(),
            user_id: "u1".into(),
            title: "Release check".into(),
            project_id: None,
            template: "{}".into(),
            agent_type: None,
            assistant_id: Some("a".into()),
            mode: "agent".into(),
            concurrency: 4,
            item_timeout_sec: 1800,
            checks: "[]".into(),
            status: status.into(),
            trial_size: 1,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn items(statuses: &[&str]) -> Vec<BatchItemRow> {
        statuses
            .iter()
            .enumerate()
            .map(|(i, s)| BatchItemRow {
                id: format!("bi_{i}"),
                batch_job_id: "bj_1".into(),
                seq: i as i64 + 1,
                target: "{}".into(),
                task_session_id: None,
                attempt: 1,
                status: (*s).into(),
                check_results: None,
                error: None,
                started_at: None,
                finished_at: None,
            })
            .collect()
    }

    #[test]
    fn non_terminal_jobs_have_no_summary() {
        for status in ["draft", "trial", "running", "paused"] {
            assert!(summarize(&job(status), &items(&["passed"])).is_none(), "{status}");
        }
    }

    #[test]
    fn completed_job_counts_and_key() {
        let s = summarize(&job("completed"), &items(&["passed", "passed", "skipped"])).unwrap();
        assert_eq!(s.dedupe_key, "batch:bj_1");
        assert_eq!(s.kind, "completed");
        assert_eq!(s.title, "Batch completed: Release check");
        assert_eq!(s.text, "2 passed · 0 failed · 0 timed out · 1 skipped (of 3)");
        assert_eq!(
            s.counts,
            BatchSummaryCounts {
                total: 3,
                passed: 2,
                skipped: 1,
                ..Default::default()
            }
        );
    }

    #[test]
    fn failed_job_is_a_failure() {
        let s = summarize(&job("failed"), &items(&["passed", "failed", "timeout"])).unwrap();
        assert_eq!(s.kind, "failed");
        assert_eq!(s.title, "Batch failed: Release check");
        assert_eq!(s.text, "1 passed · 1 failed · 1 timed out (of 3)");
    }

    #[test]
    fn cancelled_job_kind_follows_its_items() {
        let clean = summarize(&job("cancelled"), &items(&["passed", "cancelled"])).unwrap();
        assert_eq!((clean.kind, clean.status.as_str()), ("completed", "cancelled"));
        assert_eq!(clean.title, "Batch cancelled: Release check");
        assert_eq!(clean.text, "1 passed · 0 failed · 0 timed out · 1 cancelled (of 2)");
        let dirty = summarize(&job("cancelled"), &items(&["timeout", "cancelled"])).unwrap();
        assert_eq!(dirty.kind, "failed");
    }
}
