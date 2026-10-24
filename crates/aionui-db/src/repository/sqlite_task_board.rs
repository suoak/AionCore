//! SQLite board repository (M8 E6-2): board fields on task_sessions plus the
//! append-only task_comments timeline.

use aionui_common::TimestampMs;
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::error::DbError;
use crate::models::{
    BOARD_ASSIGNEE_KINDS, BOARD_MAX_PRIORITY, BOARD_STATUSES, TASK_COMMENT_AUTHOR_KINDS, TASK_COMMENT_EDIT_WINDOW_MS,
    TASK_COMMENT_KINDS, TaskBoardItemRow, TaskCommentRow,
};
use crate::repository::task_board::{
    AppendTaskCommentParams, BOARD_STALE_CONFLICT_PREFIX, ITaskBoardRepository, TaskBoardFilter, UpdateTaskBoardParams,
};

const BOARD_COLUMNS: &str = "id, user_id, title, project_id, conversation_id, mode, status, board_status, \
     assignee_kind, assignee_id, priority, labels, board_rank, external_ref, created_at, updated_at";

/// Column order for `ORDER BY`: the display order of [`BOARD_STATUSES`].
const BOARD_ORDER: &str = "CASE board_status WHEN 'backlog' THEN 0 WHEN 'todo' THEN 1 WHEN 'in_progress' THEN 2 \
     WHEN 'in_review' THEN 3 WHEN 'done' THEN 4 ELSE 5 END, board_rank IS NULL, board_rank, updated_at DESC, id";

#[derive(Clone, Debug)]
pub struct SqliteTaskBoardRepository {
    pool: SqlitePool,
}

impl SqliteTaskBoardRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn invalid(message: impl Into<String>) -> DbError {
    DbError::Conflict(message.into())
}

fn validate_update(params: &UpdateTaskBoardParams<'_>) -> Result<(), DbError> {
    if let Some(status) = params.board_status
        && !BOARD_STATUSES.contains(&status)
    {
        return Err(invalid(format!("Invalid board_status '{status}'")));
    }
    if let Some(Some(assignee)) = params.assignee {
        if !BOARD_ASSIGNEE_KINDS.contains(&assignee.kind) {
            return Err(invalid(format!("Invalid assignee kind '{}'", assignee.kind)));
        }
        if assignee.id.trim().is_empty() {
            return Err(invalid("Assignee id must not be empty"));
        }
    }
    if let Some(priority) = params.priority
        && !(0..=BOARD_MAX_PRIORITY).contains(&priority)
    {
        return Err(invalid(format!("Invalid priority {priority}")));
    }
    if let Some(labels) = params.labels
        && labels.iter().any(|label| label.trim().is_empty())
    {
        return Err(invalid("Labels must not be empty"));
    }
    if let Some(Some(rank)) = params.board_rank
        && rank.is_empty()
    {
        return Err(invalid("board_rank must not be empty"));
    }
    Ok(())
}

#[async_trait::async_trait]
impl ITaskBoardRepository for SqliteTaskBoardRepository {
    async fn list_board(&self, user_id: &str, filter: &TaskBoardFilter<'_>) -> Result<Vec<TaskBoardItemRow>, DbError> {
        let mut query =
            QueryBuilder::<Sqlite>::new(format!("SELECT {BOARD_COLUMNS} FROM task_sessions WHERE user_id = "));
        query.push_bind(user_id);
        if let Some(project_id) = filter.project_id {
            query.push(" AND project_id = ").push_bind(project_id);
        }
        if let Some(status) = filter.board_status {
            query.push(" AND board_status = ").push_bind(status);
        }
        if let Some(kind) = filter.assignee_kind {
            query.push(" AND assignee_kind = ").push_bind(kind);
        }
        if let Some(id) = filter.assignee_id {
            query.push(" AND assignee_id = ").push_bind(id);
        }
        if let Some(label) = filter.label {
            query
                .push(" AND EXISTS (SELECT 1 FROM json_each(task_sessions.labels) WHERE json_each.value = ")
                .push_bind(label)
                .push(")");
        }
        query.push(format!(" ORDER BY {BOARD_ORDER}"));
        Ok(query.build_query_as::<TaskBoardItemRow>().fetch_all(&self.pool).await?)
    }

    async fn get_board_item(&self, user_id: &str, task_session_id: &str) -> Result<Option<TaskBoardItemRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskBoardItemRow>(&format!(
            "SELECT {BOARD_COLUMNS} FROM task_sessions WHERE user_id = ? AND id = ?"
        ))
        .bind(user_id)
        .bind(task_session_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn update_board(
        &self,
        user_id: &str,
        task_session_id: &str,
        params: &UpdateTaskBoardParams<'_>,
    ) -> Result<TaskBoardItemRow, DbError> {
        validate_update(params)?;
        let mut query = QueryBuilder::<Sqlite>::new("UPDATE task_sessions SET updated_at = ");
        query.push_bind(params.updated_at);
        if let Some(status) = params.board_status {
            query.push(", board_status = ").push_bind(status);
        }
        if let Some(rank) = params.board_rank {
            query.push(", board_rank = ").push_bind(rank);
        }
        if let Some(assignee) = params.assignee {
            query
                .push(", assignee_kind = ")
                .push_bind(assignee.map(|a| a.kind))
                .push(", assignee_id = ")
                .push_bind(assignee.map(|a| a.id));
        }
        if let Some(priority) = params.priority {
            query.push(", priority = ").push_bind(priority);
        }
        if let Some(labels) = params.labels {
            let json = serde_json::to_string(labels).map_err(|e| invalid(format!("Invalid labels: {e}")))?;
            query.push(", labels = ").push_bind(json);
        }
        query
            .push(" WHERE id = ")
            .push_bind(task_session_id)
            .push(" AND user_id = ")
            .push_bind(user_id);
        if let Some(expected) = params.expected_updated_at {
            query.push(" AND updated_at = ").push_bind(expected);
        }
        let result = query.build().execute(&self.pool).await?;
        if result.rows_affected() == 0 {
            if let Some(expected) = params.expected_updated_at
                && let Some(current) = self.get_board_item(user_id, task_session_id).await?
            {
                return Err(DbError::Conflict(format!(
                    "{BOARD_STALE_CONFLICT_PREFIX}: expected updated_at {expected}, current {}",
                    current.updated_at
                )));
            }
            return Err(DbError::NotFound(format!("Task session '{task_session_id}' not found")));
        }
        self.get_board_item(user_id, task_session_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Task session '{task_session_id}' not found")))
    }

    async fn set_external_ref(
        &self,
        user_id: &str,
        task_session_id: &str,
        external_ref: &str,
        updated_at: TimestampMs,
    ) -> Result<TaskBoardItemRow, DbError> {
        if external_ref.trim().is_empty() {
            return Err(invalid("external_ref must not be empty"));
        }
        let result =
            sqlx::query("UPDATE task_sessions SET external_ref = ?, updated_at = ? WHERE id = ? AND user_id = ?")
                .bind(external_ref)
                .bind(updated_at)
                .bind(task_session_id)
                .bind(user_id)
                .execute(&self.pool)
                .await
                .map_err(|error| {
                    let error = DbError::Query(error);
                    if error.is_unique_violation() {
                        DbError::Conflict(format!("external_ref '{external_ref}' is already imported"))
                    } else {
                        error
                    }
                })?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("Task session '{task_session_id}' not found")));
        }
        self.get_board_item(user_id, task_session_id)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("Task session '{task_session_id}' not found")))
    }

    async fn find_by_external_ref(
        &self,
        user_id: &str,
        external_ref: &str,
    ) -> Result<Option<TaskBoardItemRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskBoardItemRow>(&format!(
            "SELECT {BOARD_COLUMNS} FROM task_sessions WHERE user_id = ? AND external_ref = ?"
        ))
        .bind(user_id)
        .bind(external_ref)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn append_comment(&self, params: &AppendTaskCommentParams<'_>) -> Result<TaskCommentRow, DbError> {
        if !TASK_COMMENT_AUTHOR_KINDS.contains(&params.author_kind) {
            return Err(invalid(format!("Invalid author_kind '{}'", params.author_kind)));
        }
        if !TASK_COMMENT_KINDS.contains(&params.kind) {
            return Err(invalid(format!("Invalid comment kind '{}'", params.kind)));
        }
        if params.body.trim().is_empty() {
            return Err(invalid("Comment body must not be empty"));
        }
        let id = aionui_common::generate_prefixed_id("tcomment");
        // Insert only when the task belongs to this user, so a foreign task id
        // is indistinguishable from a missing one.
        let result = sqlx::query(
            "INSERT INTO task_comments (id, task_session_id, user_id, author_kind, author_id, kind, body, created_at) \
             SELECT ?, id, user_id, ?, ?, ?, ?, ? FROM task_sessions WHERE id = ? AND user_id = ?",
        )
        .bind(&id)
        .bind(params.author_kind)
        .bind(params.author_id)
        .bind(params.kind)
        .bind(params.body)
        .bind(params.created_at)
        .bind(params.task_session_id)
        .bind(params.user_id)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(DbError::NotFound(format!(
                "Task session '{}' not found",
                params.task_session_id
            )));
        }
        Ok(TaskCommentRow {
            id,
            task_session_id: params.task_session_id.to_owned(),
            user_id: params.user_id.to_owned(),
            author_kind: params.author_kind.to_owned(),
            author_id: params.author_id.to_owned(),
            kind: params.kind.to_owned(),
            body: params.body.to_owned(),
            created_at: params.created_at,
            edited_at: None,
        })
    }

    async fn list_comments(&self, user_id: &str, task_session_id: &str) -> Result<Vec<TaskCommentRow>, DbError> {
        Ok(sqlx::query_as::<_, TaskCommentRow>(
            "SELECT * FROM task_comments WHERE user_id = ? AND task_session_id = ? ORDER BY created_at, id",
        )
        .bind(user_id)
        .bind(task_session_id)
        .fetch_all(&self.pool)
        .await?)
    }

    async fn edit_comment(
        &self,
        user_id: &str,
        task_session_id: &str,
        comment_id: &str,
        author_id: &str,
        body: &str,
        edited_at: TimestampMs,
    ) -> Result<TaskCommentRow, DbError> {
        if body.trim().is_empty() {
            return Err(invalid("Comment body must not be empty"));
        }
        let updated = sqlx::query_as::<_, TaskCommentRow>(
            "UPDATE task_comments SET body = ?, edited_at = ? \
             WHERE id = ? AND task_session_id = ? AND user_id = ? AND author_id = ? AND kind = 'comment' \
             AND ? - created_at <= ? RETURNING *",
        )
        .bind(body)
        .bind(edited_at)
        .bind(comment_id)
        .bind(task_session_id)
        .bind(user_id)
        .bind(author_id)
        .bind(edited_at)
        .bind(TASK_COMMENT_EDIT_WINDOW_MS)
        .fetch_optional(&self.pool)
        .await?;
        updated.ok_or_else(|| {
            DbError::Conflict(format!(
                "Comment '{comment_id}' cannot be edited (not the author, not a comment, or edit window passed)"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::task_board::BoardAssignee;
    use crate::repository::task_session::CreateTaskSessionParams;
    use crate::{ITaskSessionRepository, SqliteTaskSessionRepository, init_database_memory};

    const USER: &str = "system_default_user";

    async fn setup() -> (crate::Database, SqliteTaskSessionRepository, SqliteTaskBoardRepository) {
        let db = init_database_memory().await.unwrap();
        let tasks = SqliteTaskSessionRepository::new(db.pool().clone());
        let board = SqliteTaskBoardRepository::new(db.pool().clone());
        (db, tasks, board)
    }

    async fn create_task(tasks: &SqliteTaskSessionRepository, title: &str) -> String {
        tasks
            .create(&CreateTaskSessionParams {
                user_id: USER,
                title,
                project_id: None,
                conversation_id: None,
                mode: "agent",
                objective: "do it",
                acceptance_criteria: "[]",
                status: "draft",
                agent_type: "codex",
                agent_session_id: None,
            })
            .await
            .unwrap()
            .id
    }

    #[tokio::test]
    async fn new_task_defaults_to_unassigned_backlog() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        let item = board.get_board_item(USER, &id).await.unwrap().unwrap();
        assert_eq!(item.board_status, "backlog");
        assert_eq!(item.assignee_kind, None);
        assert_eq!(item.assignee_id, None);
        assert_eq!(item.priority, 0);
        assert_eq!(item.labels, "[]");
        assert_eq!(item.board_rank, None);
        // The existing task row still loads through the task-session repo.
        assert_eq!(tasks.get(USER, &id).await.unwrap().unwrap().status, "draft");
    }

    #[tokio::test]
    async fn expected_updated_at_guards_the_update() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        let current = board.get_board_item(USER, &id).await.unwrap().unwrap();
        let stale = board
            .update_board(
                USER,
                &id,
                &UpdateTaskBoardParams {
                    priority: Some(1),
                    updated_at: current.updated_at + 5,
                    expected_updated_at: Some(current.updated_at - 1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&stale, DbError::Conflict(message) if message.starts_with(BOARD_STALE_CONFLICT_PREFIX)),
            "{stale:?}"
        );
        let unchanged = board.get_board_item(USER, &id).await.unwrap().unwrap();
        assert_eq!(unchanged.priority, 0);
        let item = board
            .update_board(
                USER,
                &id,
                &UpdateTaskBoardParams {
                    priority: Some(1),
                    updated_at: current.updated_at + 5,
                    expected_updated_at: Some(current.updated_at),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(item.priority, 1);
        assert_eq!(item.updated_at, current.updated_at + 5);
        // A missing row is still NotFound, not a stale conflict.
        let missing = board
            .update_board(
                USER,
                "nope",
                &UpdateTaskBoardParams {
                    priority: Some(1),
                    updated_at: 1,
                    expected_updated_at: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(missing, DbError::NotFound(_)));
    }

    #[tokio::test]
    async fn partial_update_changes_only_given_fields_and_clears_assignee() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        let labels = vec!["bug".to_owned(), "ui".to_owned()];
        let item = board
            .update_board(
                USER,
                &id,
                &UpdateTaskBoardParams {
                    board_status: Some("todo"),
                    board_rank: Some(Some("m")),
                    assignee: Some(Some(BoardAssignee {
                        kind: "agent",
                        id: "codex",
                    })),
                    priority: Some(3),
                    labels: Some(&labels),
                    updated_at: 10,
                    expected_updated_at: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(item.board_status, "todo");
        assert_eq!(item.assignee_kind.as_deref(), Some("agent"));
        assert_eq!(item.priority, 3);
        assert_eq!(item.labels, r#"["bug","ui"]"#);
        assert_eq!(item.updated_at, 10);

        let item = board
            .update_board(
                USER,
                &id,
                &UpdateTaskBoardParams {
                    assignee: Some(None),
                    updated_at: 11,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(item.assignee_kind, None);
        assert_eq!(item.assignee_id, None);
        assert_eq!(item.board_status, "todo");
        assert_eq!(item.priority, 3);
        assert_eq!(item.board_rank.as_deref(), Some("m"));
    }

    #[tokio::test]
    async fn invalid_values_are_rejected_before_writing() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        for params in [
            UpdateTaskBoardParams {
                board_status: Some("shipped"),
                ..Default::default()
            },
            UpdateTaskBoardParams {
                priority: Some(5),
                ..Default::default()
            },
            UpdateTaskBoardParams {
                assignee: Some(Some(BoardAssignee { kind: "robot", id: "x" })),
                ..Default::default()
            },
            UpdateTaskBoardParams {
                assignee: Some(Some(BoardAssignee { kind: "user", id: " " })),
                ..Default::default()
            },
        ] {
            assert!(matches!(
                board.update_board(USER, &id, &params).await,
                Err(DbError::Conflict(_))
            ));
        }
        assert_eq!(
            board.get_board_item(USER, &id).await.unwrap().unwrap().board_status,
            "backlog"
        );
        // The schema CHECK is a second line of defense.
        let raw = sqlx::query("UPDATE task_sessions SET board_status = 'shipped' WHERE id = ?")
            .bind(&id)
            .execute(&board.pool)
            .await;
        assert!(raw.is_err());
    }

    #[tokio::test]
    async fn other_users_items_are_invisible_and_unwritable() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        assert!(board.get_board_item("other", &id).await.unwrap().is_none());
        assert!(matches!(
            board
                .update_board(
                    "other",
                    &id,
                    &UpdateTaskBoardParams {
                        board_status: Some("done"),
                        ..Default::default()
                    }
                )
                .await,
            Err(DbError::NotFound(_))
        ));
        assert!(matches!(
            board
                .append_comment(&AppendTaskCommentParams {
                    user_id: "other",
                    task_session_id: &id,
                    author_kind: "user",
                    author_id: "other",
                    kind: "comment",
                    body: "hi",
                    created_at: 1,
                })
                .await,
            Err(DbError::NotFound(_))
        ));
        assert!(
            board
                .list_board("other", &TaskBoardFilter::default())
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn board_lists_in_column_then_rank_order_with_filters() {
        let (_db, tasks, board) = setup().await;
        let a = create_task(&tasks, "a").await;
        let b = create_task(&tasks, "b").await;
        let c = create_task(&tasks, "c").await;
        let labels = vec!["bug".to_owned()];
        for (id, status, rank) in [(&a, "todo", "b"), (&b, "todo", "a"), (&c, "backlog", "z")] {
            board
                .update_board(
                    USER,
                    id,
                    &UpdateTaskBoardParams {
                        board_status: Some(status),
                        board_rank: Some(Some(rank)),
                        labels: (id == &a).then_some(labels.as_slice()),
                        updated_at: 5,
                        ..Default::default()
                    },
                )
                .await
                .unwrap();
        }
        let all: Vec<String> = board
            .list_board(USER, &TaskBoardFilter::default())
            .await
            .unwrap()
            .into_iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(all, vec![c.clone(), b.clone(), a.clone()]);

        let todo = board
            .list_board(
                USER,
                &TaskBoardFilter {
                    board_status: Some("todo"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(todo.len(), 2);
        let bugs = board
            .list_board(
                USER,
                &TaskBoardFilter {
                    label: Some("bug"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(bugs.len(), 1);
        assert_eq!(bugs[0].id, a);
    }

    #[tokio::test]
    async fn external_ref_is_unique_per_user() {
        let (_db, tasks, board) = setup().await;
        let a = create_task(&tasks, "a").await;
        let b = create_task(&tasks, "b").await;
        board.set_external_ref(USER, &a, "github:o/r#1", 2).await.unwrap();
        assert!(matches!(
            board.set_external_ref(USER, &b, "github:o/r#1", 3).await,
            Err(DbError::Conflict(_))
        ));
        let found = board.find_by_external_ref(USER, "github:o/r#1").await.unwrap().unwrap();
        assert_eq!(found.id, a);
        assert!(
            board
                .find_by_external_ref("other", "github:o/r#1")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn comments_append_in_order_and_only_author_edits_within_window() {
        let (_db, tasks, board) = setup().await;
        let id = create_task(&tasks, "a").await;
        let first = board
            .append_comment(&AppendTaskCommentParams {
                user_id: USER,
                task_session_id: &id,
                author_kind: "user",
                author_id: USER,
                kind: "comment",
                body: "please fix",
                created_at: 1_000,
            })
            .await
            .unwrap();
        board
            .append_comment(&AppendTaskCommentParams {
                user_id: USER,
                task_session_id: &id,
                author_kind: "agent",
                author_id: "codex",
                kind: "assignment",
                body: "已开工：plan",
                created_at: 2_000,
            })
            .await
            .unwrap();
        let listed = board.list_comments(USER, &id).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, first.id);
        assert_eq!(listed[1].kind, "assignment");

        let edited = board
            .edit_comment(USER, &id, &first.id, USER, "please fix soon", 1_000 + 60_000)
            .await
            .unwrap();
        assert_eq!(edited.body, "please fix soon");
        assert_eq!(edited.edited_at, Some(61_000));

        // Not the author / outside the window / not a plain comment.
        assert!(matches!(
            board.edit_comment(USER, &id, &first.id, "codex", "x", 2_000).await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            board
                .edit_comment(USER, &id, &first.id, USER, "x", 1_000 + TASK_COMMENT_EDIT_WINDOW_MS + 1)
                .await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            board.edit_comment(USER, &id, &listed[1].id, "codex", "x", 2_001).await,
            Err(DbError::Conflict(_))
        ));
        assert!(matches!(
            board
                .append_comment(&AppendTaskCommentParams {
                    user_id: USER,
                    task_session_id: &id,
                    author_kind: "user",
                    author_id: USER,
                    kind: "shout",
                    body: "x",
                    created_at: 3,
                })
                .await,
            Err(DbError::Conflict(_))
        ));
    }
}
