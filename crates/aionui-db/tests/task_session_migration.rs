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
