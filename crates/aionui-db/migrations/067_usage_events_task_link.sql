-- M7 E5-1: link the usage ledger to task sessions / runs and record turns whose
-- provider reported no usage (usage_status = 'unavailable', never invented).
-- No foreign keys: the ledger outlives deleted tasks so historical spend stays queryable.
ALTER TABLE usage_events ADD COLUMN task_session_id TEXT;
ALTER TABLE usage_events ADD COLUMN run_id TEXT;
ALTER TABLE usage_events ADD COLUMN usage_status TEXT NOT NULL DEFAULT 'reported'
    CHECK (usage_status IN ('reported', 'unavailable'));

CREATE INDEX IF NOT EXISTS idx_usage_events_user_task
    ON usage_events (user_id, task_session_id, recorded_at);
CREATE INDEX IF NOT EXISTS idx_usage_events_user_run
    ON usage_events (user_id, run_id);
