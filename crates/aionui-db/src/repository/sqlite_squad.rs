use aionui_common::TimestampMs;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{SQUAD_LEADER_MODES, SQUAD_VISIBILITIES, SquadTemplateRow};
use crate::repository::squad::{
    CreateSquadTemplateParams, ISquadTemplateRepository, SQUAD_NAME_CONFLICT_PREFIX, SQUAD_STALE_CONFLICT_PREFIX,
    UpdateSquadTemplateParams,
};

/// SQLite-backed implementation of [`ISquadTemplateRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSquadTemplateRepository {
    pool: SqlitePool,
}

impl SqliteSquadTemplateRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

const COLUMNS: &str = "id, user_id, name, description, leader_assistant_id, leader_mode, members, visibility, \
                       team_id, created_at, updated_at, deleted_at";

fn check(field: &str, value: &str, allowed: &[&str]) -> Result<(), DbError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(DbError::Conflict(format!("invalid {field}: {value}")))
    }
}

fn map_unique(err: sqlx::Error, name: &str) -> DbError {
    if err.as_database_error().is_some_and(|d| d.is_unique_violation()) {
        DbError::Conflict(format!("{SQUAD_NAME_CONFLICT_PREFIX}: {name}"))
    } else {
        DbError::Query(err)
    }
}

#[async_trait::async_trait]
impl ISquadTemplateRepository for SqliteSquadTemplateRepository {
    async fn create(&self, p: CreateSquadTemplateParams<'_>) -> Result<SquadTemplateRow, DbError> {
        check("leader_mode", p.leader_mode, SQUAD_LEADER_MODES)?;
        check("visibility", p.visibility, SQUAD_VISIBILITIES)?;
        sqlx::query(
            "INSERT INTO squad_templates (id, user_id, name, description, leader_assistant_id, leader_mode, members, \
             visibility, team_id, created_at, updated_at, deleted_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, ?, ?, NULL)",
        )
        .bind(p.id)
        .bind(p.user_id)
        .bind(p.name)
        .bind(p.description)
        .bind(p.leader_assistant_id)
        .bind(p.leader_mode)
        .bind(p.members)
        .bind(p.visibility)
        .bind(p.now)
        .bind(p.now)
        .execute(&self.pool)
        .await
        .map_err(|e| map_unique(e, p.name))?;
        self.get(p.user_id, p.id, false)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("squad template {}", p.id)))
    }

    async fn get(&self, user_id: &str, id: &str, include_deleted: bool) -> Result<Option<SquadTemplateRow>, DbError> {
        let sql = if include_deleted {
            format!("SELECT {COLUMNS} FROM squad_templates WHERE user_id = ? AND id = ?")
        } else {
            format!("SELECT {COLUMNS} FROM squad_templates WHERE user_id = ? AND id = ? AND deleted_at IS NULL")
        };
        Ok(sqlx::query_as::<_, SquadTemplateRow>(&sql)
            .bind(user_id)
            .bind(id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn list(&self, user_id: &str, include_deleted: bool) -> Result<Vec<SquadTemplateRow>, DbError> {
        let sql = if include_deleted {
            format!("SELECT {COLUMNS} FROM squad_templates WHERE user_id = ? ORDER BY updated_at DESC, id")
        } else {
            format!(
                "SELECT {COLUMNS} FROM squad_templates WHERE user_id = ? AND deleted_at IS NULL \
                 ORDER BY updated_at DESC, id"
            )
        };
        Ok(sqlx::query_as::<_, SquadTemplateRow>(&sql)
            .bind(user_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn update(&self, p: UpdateSquadTemplateParams<'_>) -> Result<SquadTemplateRow, DbError> {
        check("leader_mode", p.leader_mode, SQUAD_LEADER_MODES)?;
        check("visibility", p.visibility, SQUAD_VISIBILITIES)?;
        let Some(current) = self.get(p.user_id, p.id, false).await? else {
            return Err(DbError::NotFound(format!("squad template {}", p.id)));
        };
        if current.updated_at != p.expected_updated_at {
            return Err(DbError::Conflict(format!(
                "{SQUAD_STALE_CONFLICT_PREFIX}: expected {}, current {}",
                p.expected_updated_at, current.updated_at
            )));
        }
        // Strictly increasing so two edits within one millisecond still
        // invalidate each other's `expected_updated_at`.
        let next = p.now.max(current.updated_at + 1);
        let res = sqlx::query(
            "UPDATE squad_templates SET name = ?, description = ?, leader_assistant_id = ?, leader_mode = ?, \
             members = ?, visibility = ?, updated_at = ? \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL AND updated_at = ?",
        )
        .bind(p.name)
        .bind(p.description)
        .bind(p.leader_assistant_id)
        .bind(p.leader_mode)
        .bind(p.members)
        .bind(p.visibility)
        .bind(next)
        .bind(p.user_id)
        .bind(p.id)
        .bind(p.expected_updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| map_unique(e, p.name))?;
        if res.rows_affected() == 0 {
            return Err(DbError::Conflict(format!(
                "{SQUAD_STALE_CONFLICT_PREFIX}: concurrent update"
            )));
        }
        self.get(p.user_id, p.id, false)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("squad template {}", p.id)))
    }

    async fn soft_delete(&self, user_id: &str, id: &str, now: TimestampMs) -> Result<Option<TimestampMs>, DbError> {
        let res = sqlx::query(
            "UPDATE squad_templates SET deleted_at = ?, updated_at = MAX(updated_at + 1, ?) \
             WHERE user_id = ? AND id = ? AND deleted_at IS NULL",
        )
        .bind(now)
        .bind(now)
        .bind(user_id)
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok((res.rows_affected() > 0).then_some(now))
    }

    async fn instance_team_ids(&self, template_id: &str) -> Result<Vec<String>, DbError> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM teams WHERE squad_template_id = ? ORDER BY id")
            .bind(template_id)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn bind_team_instance(&self, user_id: &str, team_id: &str, template_id: &str) -> Result<(), DbError> {
        let done = sqlx::query("UPDATE teams SET squad_template_id = ? WHERE id = ? AND user_id = ?")
            .bind(template_id)
            .bind(team_id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        if done.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("team {team_id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init_database_memory;

    fn params<'a>(id: &'a str, user: &'a str, name: &'a str) -> CreateSquadTemplateParams<'a> {
        CreateSquadTemplateParams {
            id,
            user_id: user,
            name,
            description: "",
            leader_assistant_id: "lead",
            leader_mode: "route_only",
            members: r#"[{"assistant_id":"a1","role_title":"fe","role_description":"","max_parallel":1}]"#,
            visibility: "private",
            now: 100,
        }
    }

    #[tokio::test]
    async fn bind_team_instance_is_owner_scoped_and_visible_to_team_repo() {
        use crate::repository::team::ITeamRepository;
        let db = init_database_memory().await.unwrap();
        let repo = SqliteSquadTemplateRepository::new(db.pool().clone());
        repo.create(params("s1", "u1", "web")).await.unwrap();
        sqlx::query(
            "INSERT INTO teams (id, user_id, name, agents, created_at, updated_at) VALUES ('t1', 'u1', 'n', '[]', 1, 1)",
        )
        .execute(db.pool())
        .await
        .unwrap();
        let teams = crate::SqliteTeamRepository::new(db.pool().clone());
        assert_eq!(teams.team_squad_template_id("t1").await.unwrap(), None);
        assert!(matches!(
            repo.bind_team_instance("u2", "t1", "s1").await,
            Err(DbError::NotFound(_))
        ));
        repo.bind_team_instance("u1", "t1", "s1").await.unwrap();
        assert_eq!(teams.team_squad_template_id("t1").await.unwrap().as_deref(), Some("s1"));
        assert_eq!(repo.instance_team_ids("s1").await.unwrap(), vec!["t1".to_string()]);
    }

    #[tokio::test]
    async fn crud_soft_delete_stale_and_name_conflict() {
        let db = init_database_memory().await.unwrap();
        let repo = SqliteSquadTemplateRepository::new(db.pool().clone());
        let row = repo.create(params("s1", "u1", "web")).await.unwrap();
        assert_eq!(row.leader_mode, "route_only");
        assert_eq!(row.deleted_at, None);

        // Same live name for the same user conflicts; other users are free.
        let err = repo.create(params("s2", "u1", "web")).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(ref m) if m.starts_with(SQUAD_NAME_CONFLICT_PREFIX)));
        repo.create(params("s3", "u2", "web")).await.unwrap();
        assert!(repo.get("u2", "s1", false).await.unwrap().is_none(), "user scoped");

        let upd = |expected| UpdateSquadTemplateParams {
            user_id: "u1",
            id: "s1",
            expected_updated_at: expected,
            name: "web2",
            description: "d",
            leader_assistant_id: "lead",
            leader_mode: "route_and_work",
            members: "[]",
            visibility: "private",
            now: 100,
        };
        let err = repo.update(upd(1)).await.unwrap_err();
        assert!(matches!(err, DbError::Conflict(ref m) if m.starts_with(SQUAD_STALE_CONFLICT_PREFIX)));
        let row2 = repo.update(upd(row.updated_at)).await.unwrap();
        assert!(row2.updated_at > row.updated_at, "strictly increasing");
        assert_eq!(row2.leader_mode, "route_and_work");
        // Re-using the old token is now stale.
        assert!(repo.update(upd(row.updated_at)).await.is_err());

        assert_eq!(repo.soft_delete("u1", "s1", 500).await.unwrap(), Some(500));
        assert_eq!(repo.soft_delete("u1", "s1", 600).await.unwrap(), None);
        assert!(repo.get("u1", "s1", false).await.unwrap().is_none());
        let kept = repo.get("u1", "s1", true).await.unwrap().expect("row kept");
        assert_eq!(kept.deleted_at, Some(500));
        assert!(repo.list("u1", false).await.unwrap().is_empty());
        assert_eq!(repo.list("u1", true).await.unwrap().len(), 1);
        // The name is free again once the old template is soft-deleted.
        repo.create(params("s4", "u1", "web2")).await.unwrap();
        assert!(repo.instance_team_ids("s4").await.unwrap().is_empty());
    }
}
