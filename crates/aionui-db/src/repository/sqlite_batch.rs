use aionui_common::{TimestampMs, generate_prefixed_id};
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{BatchItemRow, BatchJobRow};
use crate::repository::batch::{
    BatchJobListFilter, CreateBatchItemParams, CreateBatchJobParams, FinishBatchItemParams, IBatchRepository,
};

const JOB_COLUMNS: &str = "id, user_id, title, project_id, template, agent_type, assistant_id, mode, \
     concurrency, item_timeout_sec, checks, status, trial_size, created_at, updated_at";

const ITEM_COLUMNS: &str = "id, batch_job_id, seq, target, task_session_id, attempt, status, check_results, error, \
     started_at, finished_at";

#[derive(Clone, Debug)]
pub struct SqliteBatchRepository {
    pool: SqlitePool,
}

impl SqliteBatchRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    async fn owned_job_exists(&self, user_id: &str, batch_job_id: &str) -> Result<bool, DbError> {
        let found: Option<i64> = sqlx::query_scalar("SELECT 1 FROM batch_jobs WHERE id = ? AND user_id = ?")
            .bind(batch_job_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }
}

#[async_trait::async_trait]
impl IBatchRepository for SqliteBatchRepository {
    async fn create_job(&self, params: &CreateBatchJobParams<'_>) -> Result<BatchJobRow, DbError> {
        let id = generate_prefixed_id("batch");
        let sql = format!(
            "INSERT INTO batch_jobs (\
                id, user_id, title, project_id, template, agent_type, assistant_id, mode, \
                concurrency, item_timeout_sec, checks, status, trial_size, created_at, updated_at\
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) \
            RETURNING {JOB_COLUMNS}"
        );
        Ok(sqlx::query_as::<_, BatchJobRow>(&sql)
            .bind(&id)
            .bind(params.user_id)
            .bind(params.title)
            .bind(params.project_id)
            .bind(params.template)
            .bind(params.agent_type)
            .bind(params.assistant_id)
            .bind(params.mode)
            .bind(params.concurrency)
            .bind(params.item_timeout_sec)
            .bind(params.checks)
            .bind(params.status)
            .bind(params.trial_size)
            .bind(params.created_at)
            .bind(params.created_at)
            .fetch_one(&self.pool)
            .await?)
    }

    async fn get_job(&self, user_id: &str, id: &str) -> Result<Option<BatchJobRow>, DbError> {
        let sql = format!("SELECT {JOB_COLUMNS} FROM batch_jobs WHERE id = ? AND user_id = ?");
        Ok(sqlx::query_as::<_, BatchJobRow>(&sql)
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_jobs(&self, user_id: &str, filter: &BatchJobListFilter) -> Result<Vec<BatchJobRow>, DbError> {
        let mut sql = format!("SELECT {JOB_COLUMNS} FROM batch_jobs WHERE user_id = ?");
        if filter.before_updated_at.is_some() {
            sql.push_str(" AND updated_at < ?");
        }
        sql.push_str(" ORDER BY updated_at DESC, id DESC LIMIT ?");
        let mut query = sqlx::query_as::<_, BatchJobRow>(&sql).bind(user_id);
        if let Some(before) = filter.before_updated_at {
            query = query.bind(before);
        }
        Ok(query.bind(filter.limit.clamp(1, 500)).fetch_all(&self.pool).await?)
    }

    async fn set_job_status(
        &self,
        user_id: &str,
        id: &str,
        status: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query("UPDATE batch_jobs SET status = ?, updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(status)
            .bind(updated_at)
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn insert_items(
        &self,
        user_id: &str,
        batch_job_id: &str,
        items: &[CreateBatchItemParams<'_>],
    ) -> Result<Vec<BatchItemRow>, DbError> {
        if !self.owned_job_exists(user_id, batch_job_id).await? {
            return Err(DbError::NotFound(format!("batch job '{batch_job_id}'")));
        }
        let mut tx = self.pool.begin().await?;
        let mut out = Vec::with_capacity(items.len());
        let sql = format!(
            "INSERT INTO batch_items (\
                id, batch_job_id, seq, target, task_session_id, attempt, status, \
                check_results, error, started_at, finished_at\
            ) VALUES (?, ?, ?, ?, NULL, 0, ?, NULL, NULL, NULL, NULL) \
            RETURNING {ITEM_COLUMNS}"
        );
        for item in items {
            let id = generate_prefixed_id("bitem");
            let row = sqlx::query_as::<_, BatchItemRow>(&sql)
                .bind(&id)
                .bind(batch_job_id)
                .bind(item.seq)
                .bind(item.target)
                .bind(item.status)
                .fetch_one(&mut *tx)
                .await?;
            out.push(row);
        }
        tx.commit().await?;
        Ok(out)
    }

    async fn get_item(&self, user_id: &str, item_id: &str) -> Result<Option<BatchItemRow>, DbError> {
        let sql = "SELECT i.id, i.batch_job_id, i.seq, i.target, i.task_session_id, i.attempt, i.status,              i.check_results, i.error, i.started_at, i.finished_at              FROM batch_items i              INNER JOIN batch_jobs j ON j.id = i.batch_job_id              WHERE i.id = ? AND j.user_id = ?";
        Ok(sqlx::query_as::<_, BatchItemRow>(sql)
            .bind(item_id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list_items(&self, user_id: &str, batch_job_id: &str) -> Result<Vec<BatchItemRow>, DbError> {
        if !self.owned_job_exists(user_id, batch_job_id).await? {
            return Err(DbError::NotFound(format!("batch job '{batch_job_id}'")));
        }
        let sql = format!("SELECT {ITEM_COLUMNS} FROM batch_items WHERE batch_job_id = ? ORDER BY seq ASC, id ASC");
        Ok(sqlx::query_as::<_, BatchItemRow>(&sql)
            .bind(batch_job_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn set_item_status(
        &self,
        user_id: &str,
        item_id: &str,
        status: &str,
        error: Option<&str>,
        started_at: Option<TimestampMs>,
        finished_at: Option<TimestampMs>,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE batch_items SET status = ?, error = ?, \
             started_at = COALESCE(?, started_at), finished_at = COALESCE(?, finished_at) \
             WHERE id = ? AND batch_job_id IN (SELECT id FROM batch_jobs WHERE user_id = ?)",
        )
        .bind(status)
        .bind(error)
        .bind(started_at)
        .bind(finished_at)
        .bind(item_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn transition_job_status(
        &self,
        user_id: &str,
        id: &str,
        from: &[&str],
        to: &str,
        updated_at: TimestampMs,
    ) -> Result<bool, DbError> {
        if from.is_empty() {
            return Ok(false);
        }
        let sql = format!(
            "UPDATE batch_jobs SET status = ?, updated_at = ? WHERE id = ? AND user_id = ? AND status IN ({})",
            placeholders(from.len())
        );
        let mut query = sqlx::query(&sql).bind(to).bind(updated_at).bind(id).bind(user_id);
        for status in from {
            query = query.bind(*status);
        }
        Ok(query.execute(&self.pool).await?.rows_affected() > 0)
    }

    async fn start_item(
        &self,
        user_id: &str,
        item_id: &str,
        task_session_id: &str,
        started_at: TimestampMs,
    ) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE batch_items SET status = 'running', task_session_id = ?, attempt = attempt + 1, \
             check_results = NULL, error = NULL, started_at = ?, finished_at = NULL \
             WHERE id = ? AND status = 'pending' \
             AND batch_job_id IN (SELECT id FROM batch_jobs WHERE user_id = ?)",
        )
        .bind(task_session_id)
        .bind(started_at)
        .bind(item_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn finish_item(&self, user_id: &str, params: &FinishBatchItemParams<'_>) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE batch_items SET status = ?, check_results = ?, error = ?, finished_at = ? \
             WHERE id = ? AND status = 'running' \
             AND batch_job_id IN (SELECT id FROM batch_jobs WHERE user_id = ?)",
        )
        .bind(params.status)
        .bind(params.check_results)
        .bind(params.error)
        .bind(params.finished_at)
        .bind(params.item_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn reset_items(
        &self,
        user_id: &str,
        batch_job_id: &str,
        item_ids: &[String],
        from: &[&str],
    ) -> Result<u64, DbError> {
        if !self.owned_job_exists(user_id, batch_job_id).await? {
            return Err(DbError::NotFound(format!("batch job '{batch_job_id}'")));
        }
        if item_ids.is_empty() || from.is_empty() {
            return Ok(0);
        }
        let mut total = 0;
        let mut tx = self.pool.begin().await?;
        // Chunked to stay far below SQLite's bound-parameter limit.
        for chunk in item_ids.chunks(200) {
            let sql = format!(
                "UPDATE batch_items SET status = 'pending', check_results = NULL, error = NULL, \
                 started_at = NULL, finished_at = NULL \
                 WHERE batch_job_id = ? AND id IN ({}) AND status IN ({})",
                placeholders(chunk.len()),
                placeholders(from.len())
            );
            let mut query = sqlx::query(&sql).bind(batch_job_id);
            for id in chunk {
                query = query.bind(id);
            }
            for status in from {
                query = query.bind(*status);
            }
            total += query.execute(&mut *tx).await?.rows_affected();
        }
        tx.commit().await?;
        Ok(total)
    }

    async fn cancel_pending_items(
        &self,
        user_id: &str,
        batch_job_id: &str,
        finished_at: TimestampMs,
    ) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE batch_items SET status = 'cancelled', finished_at = ? \
             WHERE batch_job_id = ? AND status = 'pending' \
             AND batch_job_id IN (SELECT id FROM batch_jobs WHERE user_id = ?)",
        )
        .bind(finished_at)
        .bind(batch_job_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    async fn recover_interrupted(&self, error: &str, now: TimestampMs) -> Result<(u64, u64), DbError> {
        let mut tx = self.pool.begin().await?;
        let items = sqlx::query(
            "UPDATE batch_items SET status = 'failed', error = ?, finished_at = ? WHERE status = 'running'",
        )
        .bind(error)
        .bind(now)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let jobs = sqlx::query("UPDATE batch_jobs SET status = 'paused', updated_at = ? WHERE status = 'running'")
            .bind(now)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        Ok((items, jobs))
    }
}

fn placeholders(n: usize) -> String {
    vec!["?"; n].join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    fn job<'a>(user: &'a str, title: &'a str, at: TimestampMs) -> CreateBatchJobParams<'a> {
        CreateBatchJobParams {
            user_id: user,
            title,
            project_id: None,
            template: r#"{"prompt":"check {{target}}"}"#,
            agent_type: Some("claude"),
            assistant_id: None,
            mode: "agent",
            concurrency: 4,
            item_timeout_sec: 1800,
            checks: r#"[{"type":"file_exists","path":"release-check.json"}]"#,
            status: "draft",
            trial_size: 1,
            created_at: at,
        }
    }

    #[tokio::test]
    async fn create_list_and_owner_scope() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteBatchRepository::new(db.pool().clone());

        let a = repo.create_job(&job("u1", "A", 10)).await.unwrap();
        let b = repo.create_job(&job("u1", "B", 20)).await.unwrap();
        let _other = repo.create_job(&job("u2", "X", 30)).await.unwrap();

        assert!(a.id.starts_with("batch_"));
        assert_eq!(a.status, "draft");
        assert_eq!(a.concurrency, 4);

        let listed = repo
            .list_jobs(
                "u1",
                &BatchJobListFilter {
                    before_updated_at: None,
                    limit: 10,
                },
            )
            .await
            .unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, b.id);
        assert_eq!(listed[1].id, a.id);
        assert!(repo.get_job("u2", &a.id).await.unwrap().is_none());

        assert!(repo.set_job_status("u1", &a.id, "trial", 40).await.unwrap());
        assert!(!repo.set_job_status("u2", &a.id, "running", 41).await.unwrap());
        assert_eq!(repo.get_job("u1", &a.id).await.unwrap().unwrap().status, "trial");
    }

    #[tokio::test]
    async fn items_are_job_scoped_and_ordered() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteBatchRepository::new(db.pool().clone());
        let job_row = repo.create_job(&job("u1", "J", 1)).await.unwrap();

        let items = repo
            .insert_items(
                "u1",
                &job_row.id,
                &[
                    CreateBatchItemParams {
                        seq: 2,
                        target: r#"{"repo":"b"}"#,
                        status: "pending",
                    },
                    CreateBatchItemParams {
                        seq: 1,
                        target: r#"{"repo":"a"}"#,
                        status: "pending",
                    },
                ],
            )
            .await
            .unwrap();
        assert_eq!(items.len(), 2);
        assert!(items[0].id.starts_with("bitem_"));

        let listed = repo.list_items("u1", &job_row.id).await.unwrap();
        assert_eq!(listed.iter().map(|i| i.seq).collect::<Vec<_>>(), vec![1, 2]);

        assert!(
            repo.insert_items("u2", &job_row.id, &[])
                .await
                .unwrap_err()
                .to_string()
                .contains("batch job")
        );

        let first = &listed[0];
        assert!(
            repo.set_item_status("u1", &first.id, "running", None, Some(5), None)
                .await
                .unwrap()
        );
        assert!(
            !repo
                .set_item_status("u2", &first.id, "failed", Some("nope"), None, Some(6))
                .await
                .unwrap()
        );
        let got = repo.get_item("u1", &first.id).await.unwrap().unwrap();
        assert_eq!(got.status, "running");
        assert_eq!(got.started_at, Some(5));
        assert!(repo.get_item("u2", &first.id).await.unwrap().is_none());
    }

    async fn job_with_items(repo: &SqliteBatchRepository, user: &str, n: i64) -> (BatchJobRow, Vec<BatchItemRow>) {
        let job_row = repo.create_job(&job(user, "J", 1)).await.unwrap();
        let targets: Vec<String> = (1..=n).map(|i| format!(r#"{{"workspace":"/w{i}"}}"#)).collect();
        let params: Vec<CreateBatchItemParams<'_>> = targets
            .iter()
            .enumerate()
            .map(|(i, t)| CreateBatchItemParams {
                seq: i as i64 + 1,
                target: t,
                status: "pending",
            })
            .collect();
        let items = repo.insert_items(user, &job_row.id, &params).await.unwrap();
        (job_row, items)
    }

    #[tokio::test]
    async fn item_attempt_lifecycle_and_retry_reset() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteBatchRepository::new(db.pool().clone());
        let (job_row, items) = job_with_items(&repo, "u1", 3).await;
        let first = &items[0].id;

        assert!(!repo.start_item("u2", first, "ts_x", 5).await.unwrap());
        assert!(repo.start_item("u1", first, "ts_1", 5).await.unwrap());
        assert!(
            !repo.start_item("u1", first, "ts_dup", 6).await.unwrap(),
            "only pending can start"
        );
        let finish = FinishBatchItemParams {
            item_id: first,
            status: "failed",
            check_results: Some(r#"[{"kind":"file_exists","ok":false}]"#),
            error: Some("checks failed"),
            finished_at: 9,
        };
        assert!(repo.finish_item("u1", &finish).await.unwrap());
        assert!(
            !repo.finish_item("u1", &finish).await.unwrap(),
            "only running can finish"
        );
        let got = repo.get_item("u1", first).await.unwrap().unwrap();
        assert_eq!((got.status.as_str(), got.attempt), ("failed", 1));
        assert_eq!(got.task_session_id.as_deref(), Some("ts_1"));
        assert!(got.check_results.is_some());

        let ids = vec![first.clone(), items[1].id.clone()];
        assert_eq!(
            repo.reset_items("u1", &job_row.id, &ids, &["failed", "timeout"])
                .await
                .unwrap(),
            1,
            "pending item 2 is not reset"
        );
        assert!(repo.reset_items("u2", &job_row.id, &ids, &["failed"]).await.is_err());
        let got = repo.get_item("u1", first).await.unwrap().unwrap();
        assert_eq!(got.status, "pending");
        assert!(got.check_results.is_none() && got.error.is_none() && got.finished_at.is_none());
        assert_eq!(
            got.task_session_id.as_deref(),
            Some("ts_1"),
            "pointer kept until next start"
        );

        assert!(repo.start_item("u1", first, "ts_2", 10).await.unwrap());
        let got = repo.get_item("u1", first).await.unwrap().unwrap();
        assert_eq!((got.attempt, got.task_session_id.as_deref()), (2, Some("ts_2")));
    }

    #[tokio::test]
    async fn transitions_cancel_and_boot_recovery() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteBatchRepository::new(db.pool().clone());
        let (job_row, items) = job_with_items(&repo, "u1", 3).await;

        assert!(
            !repo
                .transition_job_status("u1", &job_row.id, &["trial"], "running", 2)
                .await
                .unwrap()
        );
        assert!(
            repo.transition_job_status("u1", &job_row.id, &["draft"], "running", 2)
                .await
                .unwrap()
        );
        assert!(
            !repo
                .transition_job_status("u2", &job_row.id, &["running"], "paused", 3)
                .await
                .unwrap()
        );

        assert!(repo.start_item("u1", &items[0].id, "ts_1", 3).await.unwrap());
        assert_eq!(repo.cancel_pending_items("u2", &job_row.id, 4).await.unwrap(), 0);
        assert_eq!(repo.cancel_pending_items("u1", &job_row.id, 4).await.unwrap(), 2);

        assert_eq!(repo.recover_interrupted("interrupted", 5).await.unwrap(), (1, 1));
        let got = repo.get_item("u1", &items[0].id).await.unwrap().unwrap();
        assert_eq!(
            (got.status.as_str(), got.error.as_deref()),
            ("failed", Some("interrupted"))
        );
        assert_eq!(repo.get_job("u1", &job_row.id).await.unwrap().unwrap().status, "paused");
    }

    #[tokio::test]
    async fn status_check_rejects_unknown_values() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteBatchRepository::new(db.pool().clone());
        let mut params = job("u1", "bad", 1);
        params.status = "nope";
        assert!(repo.create_job(&params).await.is_err());
    }
}
