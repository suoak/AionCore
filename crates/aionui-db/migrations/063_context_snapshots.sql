CREATE TABLE IF NOT EXISTS context_snapshots (
    id TEXT PRIMARY KEY NOT NULL,
    task_session_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    query TEXT NOT NULL,
    scope TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('planning', 'execution', 'verification')),
    result_refs TEXT NOT NULL DEFAULT '[]',
    snapshot_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (run_id) REFERENCES task_runs(id) ON DELETE CASCADE,
    UNIQUE (run_id, snapshot_hash)
);

CREATE INDEX IF NOT EXISTS idx_context_snapshots_task_created
    ON context_snapshots(task_session_id, created_at, id);
CREATE INDEX IF NOT EXISTS idx_context_snapshots_run_created
    ON context_snapshots(run_id, created_at, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_context_snapshots_task_id
    ON context_snapshots(task_session_id, id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_task_artifacts_task_id_context
    ON task_artifacts(task_session_id, id);

CREATE TABLE IF NOT EXISTS context_snapshot_artifacts (
    task_session_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (snapshot_id, artifact_id),
    FOREIGN KEY (task_session_id) REFERENCES task_sessions(id) ON DELETE CASCADE,
    FOREIGN KEY (task_session_id, snapshot_id)
        REFERENCES context_snapshots(task_session_id, id) ON DELETE CASCADE,
    FOREIGN KEY (task_session_id, artifact_id)
        REFERENCES task_artifacts(task_session_id, id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_context_snapshot_artifacts_artifact
    ON context_snapshot_artifacts(artifact_id, created_at, snapshot_id);
