-- M7 E3: WorkMate-managed git worktrees and fan-out groups.
-- Additive only; existing conversations/task sessions are untouched.

CREATE TABLE IF NOT EXISTS fanout_groups (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    task_session_id TEXT,
    title TEXT NOT NULL DEFAULT '',
    source_repo_path TEXT NOT NULL,
    repo_common_dir TEXT NOT NULL,
    base_ref TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    prompt TEXT NOT NULL,
    max_parallel INTEGER NOT NULL DEFAULT 2 CHECK (max_parallel BETWEEN 1 AND 5),
    status TEXT NOT NULL CHECK (status IN
        ('preparing', 'running', 'comparing', 'adopted', 'discarded', 'cancelled', 'failed')),
    winner_slot TEXT,
    adopt_result TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_fanout_groups_user_updated ON fanout_groups(user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS task_worktrees (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    fanout_group_id TEXT,
    slot TEXT,
    slot_spec TEXT NOT NULL DEFAULT '{}',
    task_session_id TEXT,
    conversation_id TEXT,
    source_repo_path TEXT NOT NULL,
    repo_common_dir TEXT NOT NULL,
    path TEXT NOT NULL,
    branch TEXT NOT NULL,
    base_ref TEXT NOT NULL,
    base_commit TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN
        ('creating', 'ready', 'failed', 'unbound', 'removing', 'removed', 'orphaned')),
    phase TEXT,
    error_code TEXT,
    error_message TEXT,
    size_bytes INTEGER,
    branch_preserved INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    removed_at INTEGER,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (fanout_group_id) REFERENCES fanout_groups(id) ON DELETE SET NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE SET NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS uq_task_worktrees_live_path
    ON task_worktrees(path) WHERE removed_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_task_worktrees_group ON task_worktrees(fanout_group_id, slot);
CREATE INDEX IF NOT EXISTS idx_task_worktrees_conversation ON task_worktrees(conversation_id);
CREATE INDEX IF NOT EXISTS idx_task_worktrees_user_updated ON task_worktrees(user_id, updated_at DESC);

ALTER TABLE task_sessions ADD COLUMN fanout_group_id TEXT
    REFERENCES fanout_groups(id) ON DELETE SET NULL;
