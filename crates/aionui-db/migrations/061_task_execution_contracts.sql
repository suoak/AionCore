CREATE TABLE IF NOT EXISTS task_artifacts (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('plan', 'goal')),
    version INTEGER NOT NULL CHECK (version > 0),
    content TEXT NOT NULL,
    content_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('submitted', 'approved', 'rejected', 'superseded')),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    UNIQUE (task_session_id, kind, version)
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_artifacts_hash
    ON task_artifacts(task_session_id, id, content_hash);
CREATE INDEX IF NOT EXISTS idx_task_artifacts_task_kind_version
    ON task_artifacts(task_session_id, kind, version DESC);

CREATE TABLE IF NOT EXISTS task_approvals (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    run_id TEXT,
    approval_type TEXT NOT NULL CHECK (approval_type IN ('plan', 'goal')),
    artifact_id TEXT NOT NULL,
    artifact_hash TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired', 'cancelled')),
    requested_at INTEGER NOT NULL,
    resolved_at INTEGER,
    resolved_by TEXT,
    comment TEXT,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (artifact_id) REFERENCES task_artifacts(id) ON DELETE CASCADE
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_task_approvals_one_pending
    ON task_approvals(task_session_id, approval_type)
    WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_task_approvals_task_requested
    ON task_approvals(task_session_id, requested_at DESC);

CREATE TABLE IF NOT EXISTS task_runs (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    plan_artifact_id TEXT NOT NULL,
    goal_artifact_id TEXT,
    approval_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'completed', 'failed', 'cancelled')),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    error_message TEXT,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE RESTRICT,
    FOREIGN KEY (plan_artifact_id) REFERENCES task_artifacts(id) ON DELETE RESTRICT,
    FOREIGN KEY (goal_artifact_id) REFERENCES task_artifacts(id) ON DELETE RESTRICT,
    FOREIGN KEY (approval_id) REFERENCES task_approvals(id) ON DELETE RESTRICT
);

CREATE INDEX IF NOT EXISTS idx_task_runs_task_started
    ON task_runs(task_session_id, started_at DESC);

CREATE TABLE IF NOT EXISTS task_acceptance_criteria (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    goal_artifact_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    description TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'passed', 'failed', 'needs_verification')),
    evidence TEXT NOT NULL DEFAULT '[]',
    verified_at INTEGER,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (goal_artifact_id) REFERENCES task_artifacts(id) ON DELETE CASCADE,
    UNIQUE (goal_artifact_id, position)
);

CREATE INDEX IF NOT EXISTS idx_task_acceptance_criteria_task
    ON task_acceptance_criteria(task_session_id, position);
