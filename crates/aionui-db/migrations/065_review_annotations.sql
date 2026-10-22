-- M7 E4: diff annotations (review threads / comments), annotation batches sent
-- to an agent, and inline SCM deliveries (commit / push / PR).
-- Design: E4-diff-annotation-commit-pr.md §5.1. New tables only, no backfill;
-- older UI builds are unaffected.
--
-- E4-1 uses review_threads + review_comments. review_batches (E4-3) and
-- scm_deliveries (E4-5) ship with the same migration so the schema lands once.
CREATE TABLE IF NOT EXISTS review_threads (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('scm', 'worktree', 'task_run')),
    scope_id TEXT NOT NULL,            -- scm: canonical repo workdir; worktree: task_worktrees.id; task_run: run id
    conversation_id TEXT,              -- default send-back target
    task_session_id TEXT,
    run_id TEXT,
    file_path TEXT NOT NULL,           -- repo-relative path, POSIX separators
    side TEXT NOT NULL CHECK (side IN ('new', 'old')),
    start_line INTEGER NOT NULL CHECK (start_line > 0),
    end_line INTEGER NOT NULL CHECK (end_line >= start_line),
    anchor TEXT NOT NULL,              -- JSON, see design §5.2
    status TEXT NOT NULL CHECK (status IN ('open', 'sent', 'resolved', 'outdated')),
    last_batch_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    resolved_at INTEGER,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE SET NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE SET NULL
);
CREATE INDEX IF NOT EXISTS idx_review_threads_scope ON review_threads(user_id, scope_kind, scope_id, status);
CREATE INDEX IF NOT EXISTS idx_review_threads_file ON review_threads(scope_id, file_path);

CREATE TABLE IF NOT EXISTS review_comments (
    id TEXT PRIMARY KEY NOT NULL,
    thread_id TEXT NOT NULL,
    author TEXT NOT NULL CHECK (author IN ('user', 'agent')),
    body TEXT NOT NULL,                -- Markdown, max 8 KB
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (thread_id) REFERENCES review_threads(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_review_comments_thread ON review_comments(thread_id, created_at);

CREATE TABLE IF NOT EXISTS review_batches (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    target_conversation_id TEXT NOT NULL,
    delivery TEXT NOT NULL CHECK (delivery IN ('steer', 'followup', 'new_conversation')),
    thread_ids TEXT NOT NULL,          -- JSON array (snapshot at send time)
    content TEXT NOT NULL,             -- full text actually sent (frozen, auditable)
    input_id TEXT,                     -- submit_input receipt
    status TEXT NOT NULL CHECK (status IN ('sending', 'accepted', 'failed')),
    error_message TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE,
    FOREIGN KEY (target_conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_review_batches_target ON review_batches(target_conversation_id, created_at DESC);

CREATE TABLE IF NOT EXISTS scm_deliveries (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    repo_path TEXT NOT NULL,
    branch TEXT NOT NULL,
    commit_sha TEXT,
    remote TEXT,
    provider TEXT CHECK (provider IN ('github', 'gitlab', 'other')),
    pr_url TEXT,
    pr_number TEXT,
    worktree_id TEXT,
    task_session_id TEXT,
    run_id TEXT,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_scm_deliveries_repo ON scm_deliveries(user_id, repo_path, created_at DESC);
