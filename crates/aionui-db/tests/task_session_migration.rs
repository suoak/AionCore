use sqlx::sqlite::SqlitePoolOptions;

#[tokio::test]
async fn migration_060_applies_to_the_059_foreign_key_shape() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL UNIQUE);
         CREATE TABLE conversations (id TEXT PRIMARY KEY NOT NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../migrations/060_task_sessions.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'task_sessions'")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(tables, ["task_sessions"]);

    sqlx::query("INSERT INTO users (id) VALUES ('user-1')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO projects (project_id) VALUES ('project-1')")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO task_sessions
         (id, user_id, title, project_id, mode, objective, acceptance_criteria, status, agent_type, created_at, updated_at)
         VALUES ('task-valid', 'user-1', 'title', 'project-1', 'plan', '', '[]', 'ready', 'codex', 1, 1)",
    )
    .execute(&pool)
    .await
    .expect("the business project_id relation must be accepted");

    sqlx::query(
        "INSERT INTO task_sessions
         (id, user_id, title, mode, objective, acceptance_criteria, status, agent_type, created_at, updated_at)
         VALUES ('task-1', 'missing-user', 'title', 'plan', '', '[]', 'ready', 'codex', 1, 1)",
    )
    .execute(&pool)
    .await
    .expect_err("the 059 user relation must be enforced");
}

#[tokio::test]
async fn migration_061_adds_the_plan_goal_contract_tables() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL UNIQUE);
         CREATE TABLE conversations (id TEXT PRIMARY KEY NOT NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/060_task_sessions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/061_task_execution_contracts.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name IN
         ('task_artifacts', 'task_approvals', 'task_runs', 'task_acceptance_criteria')
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        tables,
        [
            "task_acceptance_criteria",
            "task_approvals",
            "task_artifacts",
            "task_runs"
        ],
    );
}

#[tokio::test]
async fn migration_062_preserves_execution_runs_and_adds_trace_storage() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL UNIQUE);
         CREATE TABLE conversations (id TEXT PRIMARY KEY NOT NULL);
         INSERT INTO users (id) VALUES ('user-1');
         INSERT INTO conversations (id) VALUES ('conversation-1');",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/060_task_sessions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/061_task_execution_contracts.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(
        "INSERT INTO task_sessions
         (id, user_id, title, conversation_id, mode, objective, acceptance_criteria, status, agent_type, created_at, updated_at)
         VALUES ('task-1', 'user-1', 'trace', 'conversation-1', 'plan', '', '[]', 'running', 'aionrs', 1, 1);
         INSERT INTO task_artifacts
         (id, task_session_id, kind, version, content, content_hash, status, created_at, updated_at)
         VALUES ('artifact-1', 'task-1', 'plan', 1, 'plan', 'hash', 'approved', 1, 1);
         INSERT INTO task_approvals
         (id, task_session_id, approval_type, artifact_id, artifact_hash, status, requested_at, resolved_at, resolved_by)
         VALUES ('approval-1', 'task-1', 'plan', 'artifact-1', 'hash', 'approved', 1, 2, 'user-1');
         INSERT INTO task_runs
         (id, task_session_id, conversation_id, plan_artifact_id, approval_id, status, started_at)
         VALUES ('run-1', 'task-1', 'conversation-1', 'artifact-1', 'approval-1', 'running', 3);",
    )
    .execute(&pool)
    .await
    .unwrap();

    sqlx::raw_sql(include_str!("../migrations/062_task_execution_trace.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let preserved: (String, String, Option<String>, Option<String>) =
        sqlx::query_as("SELECT id, run_kind, plan_artifact_id, approval_id FROM task_runs WHERE id = 'run-1'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(preserved.0, "run-1");
    assert_eq!(preserved.1, "execution");
    assert_eq!(preserved.2.as_deref(), Some("artifact-1"));
    assert_eq!(preserved.3.as_deref(), Some("approval-1"));

    sqlx::query(
        "INSERT INTO task_runs
         (id, task_session_id, conversation_id, run_kind, status, started_at)
         VALUES ('planning-run', 'task-1', 'conversation-1', 'planning', 'running', 4)",
    )
    .execute(&pool)
    .await
    .expect("planning runs must exist before an artifact or approval exists");

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table'
         AND name IN ('task_trace_events', 'task_checkpoints', 'task_evidence') ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(tables, ["task_checkpoints", "task_evidence", "task_trace_events"]);
}

#[tokio::test]
async fn migration_063_adds_restart_persistent_context_snapshots_and_artifact_links() {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect("sqlite::memory:")
        .await
        .unwrap();
    sqlx::raw_sql(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE users (id TEXT PRIMARY KEY NOT NULL);
         CREATE TABLE projects (id INTEGER PRIMARY KEY AUTOINCREMENT, project_id TEXT NOT NULL UNIQUE);
         CREATE TABLE conversations (id TEXT PRIMARY KEY NOT NULL);",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::raw_sql(include_str!("../migrations/060_task_sessions.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/061_task_execution_contracts.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/062_task_execution_trace.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../migrations/063_context_snapshots.sql"))
        .execute(&pool)
        .await
        .unwrap();

    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table'
         AND name IN ('context_snapshots', 'context_snapshot_artifacts') ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(tables, ["context_snapshot_artifacts", "context_snapshots"]);
}
