-- M7 E5-4: per-user inbox of things that need attention (approvals, blocked,
-- completed, failed). Producers upsert by dedupe_key; approvals are resolved
-- automatically once handled. Retention: 90 days / 5000 items per user.
-- 068 is held by the @@ grant-persistence line (#170); renumber to main's
-- latest + 1 before merge if needed.
CREATE TABLE IF NOT EXISTS inbox_items (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('approval', 'blocked', 'completed', 'failed')),
    source_kind TEXT NOT NULL CHECK (source_kind IN ('conversation', 'task_run', 'fanout_group', 'team')),
    source_id TEXT NOT NULL,
    conversation_id TEXT,
    task_session_id TEXT,
    run_id TEXT,
    title TEXT NOT NULL,
    summary TEXT,
    dedupe_key TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    read_at INTEGER,
    resolved_at INTEGER,
    UNIQUE (user_id, dedupe_key)
);
CREATE INDEX IF NOT EXISTS idx_inbox_user_unread ON inbox_items (user_id, read_at, created_at DESC);
