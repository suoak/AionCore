-- M7 E5-5: bounded retry and per-run timeout for approved execution runs.
--
-- A retry is a NEW task_runs row linked to the failed run it replaces via
-- retry_of_run_id; the approval's single run_id claim is transferred to it in
-- the same transaction (see SqliteTaskSessionRepository::create_retry_run).
ALTER TABLE task_runs ADD COLUMN attempt INTEGER NOT NULL DEFAULT 1 CHECK (attempt BETWEEN 1 AND 3);
ALTER TABLE task_runs ADD COLUMN retry_of_run_id TEXT;
ALTER TABLE task_runs ADD COLUMN timeout_ms INTEGER CHECK (timeout_ms IS NULL OR timeout_ms > 0);

-- A failed run can be retried at most once: the retry chain is linear.
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_runs_retry_of
    ON task_runs (retry_of_run_id) WHERE retry_of_run_id IS NOT NULL;

-- Retry policy: default 1 attempt (no retry), explicit opt-in up to 3.
ALTER TABLE task_sessions ADD COLUMN retry_max_attempts INTEGER NOT NULL DEFAULT 1
    CHECK (retry_max_attempts BETWEEN 1 AND 3);
ALTER TABLE task_sessions ADD COLUMN retry_timeout_ms INTEGER
    CHECK (retry_timeout_ms IS NULL OR retry_timeout_ms > 0);
