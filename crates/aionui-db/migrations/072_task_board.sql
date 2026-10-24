-- M8 E6-2: work-item board on top of task_sessions.
--
-- The board column (board_status) is the user-facing workflow state and is
-- deliberately separate from the run state in task_sessions.status. All
-- additions are ADD COLUMN with defaults or new tables; no existing table is
-- rebuilt (D6-4: inbox_items is untouched, board items reuse
-- source_kind='task_run' + task_session_id).
ALTER TABLE task_sessions ADD COLUMN board_status TEXT NOT NULL DEFAULT 'backlog'
    CHECK (board_status IN ('backlog', 'todo', 'in_progress', 'in_review', 'done', 'cancelled'));
ALTER TABLE task_sessions ADD COLUMN assignee_kind TEXT
    CHECK (assignee_kind IS NULL OR assignee_kind IN ('user', 'agent', 'squad'));
ALTER TABLE task_sessions ADD COLUMN assignee_id TEXT;
-- 0 none / 1 low / 2 medium / 3 high / 4 urgent
ALTER TABLE task_sessions ADD COLUMN priority INTEGER NOT NULL DEFAULT 0
    CHECK (priority BETWEEN 0 AND 4);
ALTER TABLE task_sessions ADD COLUMN labels TEXT NOT NULL DEFAULT '[]'
    CHECK (json_valid(labels) AND json_type(labels) = 'array');
-- Lexicographic rank inside a column; NULL sorts last (never ranked yet).
ALTER TABLE task_sessions ADD COLUMN board_rank TEXT;
-- Read-only issue import source, e.g. `github:owner/repo#123`.
ALTER TABLE task_sessions ADD COLUMN external_ref TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_sessions_external_ref
    ON task_sessions (user_id, external_ref) WHERE external_ref IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_task_sessions_board
    ON task_sessions (user_id, board_status, board_rank);

-- D6-3: existing tasks land in the column matching their run state.
UPDATE task_sessions SET board_status = CASE
    WHEN status = 'completed' THEN 'done'
    WHEN status IN ('running', 'waiting_approval', 'paused') THEN 'in_progress'
    WHEN status IN ('failed', 'cancelled') THEN 'cancelled'
    ELSE 'backlog'
END;

-- Append-only timeline of comments and board events for a work item.
CREATE TABLE IF NOT EXISTS task_comments (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    author_kind TEXT NOT NULL CHECK (author_kind IN ('user', 'agent', 'system')),
    author_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('comment', 'status_change', 'assignment', 'review_decision')),
    body TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    edited_at INTEGER,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_task_comments_task_created
    ON task_comments (task_session_id, created_at, id);
