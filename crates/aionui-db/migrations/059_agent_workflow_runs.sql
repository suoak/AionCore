CREATE TABLE IF NOT EXISTS agent_workflow_runs (
    id TEXT PRIMARY KEY NOT NULL,
    assistant_definition_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    status TEXT NOT NULL,
    state_json TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    FOREIGN KEY (assistant_definition_id) REFERENCES assistant_definitions(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_agent_workflow_runs_owner
    ON agent_workflow_runs(user_id, assistant_definition_id, created_at DESC);
