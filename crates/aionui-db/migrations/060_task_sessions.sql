CREATE TABLE IF NOT EXISTS task_sessions (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    title TEXT NOT NULL,
    project_id TEXT,
    conversation_id TEXT,
    mode TEXT NOT NULL CHECK (mode IN ('agent', 'plan', 'goal')),
    objective TEXT NOT NULL DEFAULT '',
    acceptance_criteria TEXT NOT NULL DEFAULT '[]',
    status TEXT NOT NULL CHECK (
        status IN ('draft', 'ready', 'running', 'waiting_approval', 'paused', 'completed', 'failed', 'cancelled')
    ),
    agent_type TEXT NOT NULL,
    agent_session_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (project_id) REFERENCES projects(project_id) ON DELETE SET NULL,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_task_sessions_user_updated
    ON task_sessions(user_id, updated_at DESC);

CREATE INDEX IF NOT EXISTS idx_task_sessions_conversation
    ON task_sessions(user_id, conversation_id, updated_at DESC);
