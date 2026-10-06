ALTER TABLE task_runs RENAME TO task_runs_m2;

CREATE TABLE task_runs (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    conversation_id TEXT NOT NULL,
    run_kind TEXT NOT NULL DEFAULT 'execution' CHECK (run_kind IN ('planning', 'execution')),
    plan_artifact_id TEXT,
    goal_artifact_id TEXT,
    approval_id TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'paused', 'completed', 'failed', 'cancelled')),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    error_message TEXT,
    agent_id TEXT,
    agent_runtime TEXT,
    model TEXT,
    mode TEXT,
    planning_isolation TEXT,
    result_summary TEXT,
    usage TEXT,
    next_trace_sequence INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE RESTRICT,
    FOREIGN KEY (plan_artifact_id) REFERENCES task_artifacts(id) ON DELETE RESTRICT,
    FOREIGN KEY (goal_artifact_id) REFERENCES task_artifacts(id) ON DELETE RESTRICT,
    FOREIGN KEY (approval_id) REFERENCES task_approvals(id) ON DELETE RESTRICT,
    CHECK (
        (run_kind = 'planning' AND goal_artifact_id IS NULL)
        OR
        (run_kind = 'execution' AND plan_artifact_id IS NOT NULL AND approval_id IS NOT NULL)
    )
);

INSERT INTO task_runs (
    id, task_session_id, conversation_id, run_kind, plan_artifact_id, goal_artifact_id, approval_id,
    status, started_at, finished_at, error_message
)
SELECT
    id, task_session_id, conversation_id, 'execution', plan_artifact_id, goal_artifact_id, approval_id,
    status, started_at, finished_at, error_message
FROM task_runs_m2;

DROP TABLE task_runs_m2;

CREATE INDEX idx_task_runs_task_started
    ON task_runs(task_session_id, started_at DESC);

CREATE TABLE IF NOT EXISTS task_trace_events (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    event_type TEXT NOT NULL,
    timestamp INTEGER NOT NULL,
    payload TEXT NOT NULL DEFAULT '{}',
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES task_runs(id) ON DELETE CASCADE,
    UNIQUE (run_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_task_trace_events_run_sequence
    ON task_trace_events(run_id, sequence);
CREATE INDEX IF NOT EXISTS idx_task_trace_events_task_timestamp
    ON task_trace_events(task_session_id, timestamp);

CREATE TABLE IF NOT EXISTS task_checkpoints (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    checkpoint_type TEXT NOT NULL CHECK (
        checkpoint_type IN ('plan_submitted', 'before_execution', 'after_mutation', 'before_verification', 'before_completion')
    ),
    sequence INTEGER NOT NULL,
    artifact_id TEXT,
    state TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES task_runs(id) ON DELETE CASCADE,
    FOREIGN KEY (artifact_id) REFERENCES task_artifacts(id) ON DELETE SET NULL,
    UNIQUE (run_id, sequence)
);

CREATE INDEX IF NOT EXISTS idx_task_checkpoints_run_sequence
    ON task_checkpoints(run_id, sequence);

CREATE TABLE IF NOT EXISTS task_evidence (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    trace_event_id TEXT,
    criterion_id TEXT,
    kind TEXT NOT NULL CHECK (
        kind IN ('tool', 'command', 'file', 'diff', 'artifact', 'test', 'policy', 'approval', 'user', 'mcp', 'knowledge')
    ),
    summary TEXT NOT NULL,
    reference TEXT,
    metadata TEXT NOT NULL DEFAULT '{}',
    created_at INTEGER NOT NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES task_runs(id) ON DELETE CASCADE,
    FOREIGN KEY (trace_event_id) REFERENCES task_trace_events(id) ON DELETE SET NULL,
    FOREIGN KEY (criterion_id) REFERENCES task_acceptance_criteria(id) ON DELETE SET NULL
);

CREATE INDEX IF NOT EXISTS idx_task_evidence_run_created
    ON task_evidence(run_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_task_evidence_criterion
    ON task_evidence(criterion_id, created_at);
