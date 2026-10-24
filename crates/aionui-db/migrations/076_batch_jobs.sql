-- M8 E8-1: batch jobs + items (data layer only).
-- Numbered as main's highest (075 teams.work_item_id) + 1.
-- M8-plan reserved 075 for batch and 076 for autopilot columns; those shift to
-- 076 (this file) and 077 respectively. E9 user_devices → 078+.
--
-- Product decisions D8-1..D8-8 are NOT enforced here:
--   - no aionui-batch crate (D8-1)
--   - no trial/scale gate (D8-2)
--   - no mode CHECK restricting to agent (D8-3)
--   - no concurrency/timeout caps (D8-4)
--   - checks JSON is opaque storage; validators land in E8-2 (D8-6)
-- Column vocabularies below are storage enums from E8-batch-autopilot.md §2.1.

CREATE TABLE IF NOT EXISTS batch_jobs (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT NOT NULL,
    project_id TEXT,
    -- Prompt template JSON (may contain {{target}} placeholders).
    template TEXT NOT NULL,
    agent_type TEXT,
    assistant_id TEXT,
    -- Session mode string; product may later restrict to "agent" (D8-3).
    mode TEXT NOT NULL,
    concurrency INTEGER NOT NULL,
    item_timeout_sec INTEGER NOT NULL,
    -- JSON array of check specs; opaque until E8-2.
    checks TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL CHECK (
        status IN (
            'draft',
            'trial',
            'running',
            'paused',
            'completed',
            'failed',
            'cancelled'
        )
    ),
    trial_size INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_batch_jobs_user_updated
    ON batch_jobs (user_id, updated_at DESC, id DESC);

CREATE TABLE IF NOT EXISTS batch_items (
    id TEXT PRIMARY KEY NOT NULL,
    batch_job_id TEXT NOT NULL REFERENCES batch_jobs (id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    -- Target JSON, e.g. {"repo":"…","path":"…"}.
    target TEXT NOT NULL,
    task_session_id TEXT,
    attempt INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL CHECK (
        status IN (
            'pending',
            'running',
            'passed',
            'failed',
            'timeout',
            'skipped',
            'cancelled'
        )
    ),
    -- JSON array of per-check results; opaque until E8-2.
    check_results TEXT,
    error TEXT,
    started_at INTEGER,
    finished_at INTEGER,
    UNIQUE (batch_job_id, seq)
);

CREATE INDEX IF NOT EXISTS idx_batch_items_job_status
    ON batch_items (batch_job_id, status, seq);
