use aionui_common::TimestampMs;
use sqlx::SqlitePool;

use crate::error::DbError;
use crate::models::{
    SKILL_ORIGIN_APPS, SKILL_REVIEW_STATUSES, SKILL_SOURCE_KINDS, SKILL_TRUST_LEVELS, SKILL_VERSION_CREATORS,
    SKILL_VISIBILITIES, SkillGovernanceRow, SkillSourceRow, SkillVersionRow,
};
use crate::repository::skill_source::{
    AppendSkillVersionOutcome, AppendSkillVersionParams, ISkillSourceRepository, UpsertSkillSourceParams,
};

/// SQLite-backed implementation of [`ISkillSourceRepository`].
#[derive(Clone, Debug)]
pub struct SqliteSkillSourceRepository {
    pool: SqlitePool,
}

impl SqliteSkillSourceRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn check(field: &str, value: &str, allowed: &[&str]) -> Result<(), DbError> {
    if allowed.contains(&value) {
        Ok(())
    } else {
        Err(DbError::Conflict(format!("invalid {field}: {value}")))
    }
}

const SOURCE_COLUMNS: &str = "id, user_id, skill_id, kind, locator, ref, subpath, origin_app, last_fetched_at, \
                              last_commit, trust, created_at, updated_at";
const VERSION_COLUMNS: &str =
    "id, skill_id, version_no, content_hash, manifest, has_scripts, source_ref, created_by, created_at";

#[async_trait::async_trait]
impl ISkillSourceRepository for SqliteSkillSourceRepository {
    async fn upsert_source(&self, p: UpsertSkillSourceParams<'_>) -> Result<SkillSourceRow, DbError> {
        check("skill source kind", p.kind, SKILL_SOURCE_KINDS)?;
        check("skill trust", p.trust, SKILL_TRUST_LEVELS)?;
        if let Some(app) = p.origin_app {
            check("skill origin_app", app, SKILL_ORIGIN_APPS)?;
        }
        let sql = format!(
            "INSERT INTO skill_sources ({SOURCE_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, NULL, NULL, ?, ?, ?) \
             ON CONFLICT(skill_id) DO UPDATE SET \
               user_id = excluded.user_id, kind = excluded.kind, locator = excluded.locator, ref = excluded.ref, \
               subpath = excluded.subpath, origin_app = excluded.origin_app, trust = excluded.trust, \
               updated_at = excluded.updated_at \
             RETURNING {SOURCE_COLUMNS}"
        );
        let row = sqlx::query_as::<_, SkillSourceRow>(&sql)
            .bind(p.id)
            .bind(p.user_id)
            .bind(p.skill_id)
            .bind(p.kind)
            .bind(p.locator)
            .bind(p.git_ref)
            .bind(p.subpath)
            .bind(p.origin_app)
            .bind(p.trust)
            .bind(p.now)
            .bind(p.now)
            .fetch_one(&self.pool)
            .await?;
        Ok(row)
    }

    async fn find_source(&self, skill_id: &str) -> Result<Option<SkillSourceRow>, DbError> {
        let sql = format!("SELECT {SOURCE_COLUMNS} FROM skill_sources WHERE skill_id = ?");
        Ok(sqlx::query_as::<_, SkillSourceRow>(&sql)
            .bind(skill_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn record_fetch(
        &self,
        skill_id: &str,
        fetched_at: TimestampMs,
        last_commit: Option<&str>,
    ) -> Result<SkillSourceRow, DbError> {
        let sql = format!(
            "UPDATE skill_sources SET last_fetched_at = ?, last_commit = COALESCE(?, last_commit), updated_at = ? \
             WHERE skill_id = ? RETURNING {SOURCE_COLUMNS}"
        );
        sqlx::query_as::<_, SkillSourceRow>(&sql)
            .bind(fetched_at)
            .bind(last_commit)
            .bind(fetched_at)
            .bind(skill_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| DbError::NotFound(format!("skill source for {skill_id}")))
    }

    async fn append_version(&self, p: AppendSkillVersionParams<'_>) -> Result<AppendSkillVersionOutcome, DbError> {
        check("skill version created_by", p.created_by, SKILL_VERSION_CREATORS)?;
        let mut tx = self.pool.begin().await?;

        let current_status: Option<String> = sqlx::query_scalar("SELECT review_status FROM skills WHERE id = ?")
            .bind(p.skill_id)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(current_status) = current_status else {
            return Err(DbError::NotFound(format!("skill {}", p.skill_id)));
        };

        let latest_sql =
            format!("SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ? ORDER BY version_no DESC LIMIT 1");
        let latest = sqlx::query_as::<_, SkillVersionRow>(&latest_sql)
            .bind(p.skill_id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(latest) = latest.as_ref().filter(|v| v.content_hash == p.content_hash) {
            tx.commit().await?;
            return Ok(AppendSkillVersionOutcome {
                version: latest.clone(),
                created: false,
                review_status: current_status,
            });
        }
        let version_no = latest.map_or(1, |v| v.version_no + 1);

        let insert_sql = format!(
            "INSERT INTO skill_versions ({VERSION_COLUMNS}) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING {VERSION_COLUMNS}"
        );
        let version = sqlx::query_as::<_, SkillVersionRow>(&insert_sql)
            .bind(p.id)
            .bind(p.skill_id)
            .bind(version_no)
            .bind(p.content_hash)
            .bind(p.manifest)
            .bind(p.has_scripts)
            .bind(p.source_ref)
            .bind(p.created_by)
            .bind(p.now)
            .fetch_one(&mut *tx)
            .await?;

        // D10-7: any new snapshot that ships scripts needs a fresh human review,
        // whoever created it (user, import or evolution).
        let review_status = if p.has_scripts {
            sqlx::query("UPDATE skills SET review_status = 'pending', updated_at = ? WHERE id = ?")
                .bind(p.now)
                .bind(p.skill_id)
                .execute(&mut *tx)
                .await?;
            "pending".to_string()
        } else {
            current_status
        };
        tx.commit().await?;
        Ok(AppendSkillVersionOutcome {
            version,
            created: true,
            review_status,
        })
    }

    async fn list_versions(&self, skill_id: &str) -> Result<Vec<SkillVersionRow>, DbError> {
        let sql = format!("SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ? ORDER BY version_no DESC");
        Ok(sqlx::query_as::<_, SkillVersionRow>(&sql)
            .bind(skill_id)
            .fetch_all(&self.pool)
            .await?)
    }

    async fn find_version(&self, skill_id: &str, version_no: i64) -> Result<Option<SkillVersionRow>, DbError> {
        let sql = format!("SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ? AND version_no = ?");
        Ok(sqlx::query_as::<_, SkillVersionRow>(&sql)
            .bind(skill_id)
            .bind(version_no)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn latest_version(&self, skill_id: &str) -> Result<Option<SkillVersionRow>, DbError> {
        let sql =
            format!("SELECT {VERSION_COLUMNS} FROM skill_versions WHERE skill_id = ? ORDER BY version_no DESC LIMIT 1");
        Ok(sqlx::query_as::<_, SkillVersionRow>(&sql)
            .bind(skill_id)
            .fetch_optional(&self.pool)
            .await?)
    }

    async fn governance(&self, skill_id: &str) -> Result<Option<SkillGovernanceRow>, DbError> {
        Ok(sqlx::query_as::<_, SkillGovernanceRow>(
            "SELECT id, visibility, team_id, review_status FROM skills WHERE id = ?",
        )
        .bind(skill_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    async fn set_review_status(&self, skill_id: &str, status: &str, now: TimestampMs) -> Result<(), DbError> {
        check("skill review_status", status, SKILL_REVIEW_STATUSES)?;
        let res = sqlx::query("UPDATE skills SET review_status = ?, updated_at = ? WHERE id = ?")
            .bind(status)
            .bind(now)
            .bind(skill_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("skill {skill_id}")));
        }
        Ok(())
    }

    async fn set_visibility(
        &self,
        skill_id: &str,
        visibility: &str,
        team_id: Option<&str>,
        now: TimestampMs,
    ) -> Result<(), DbError> {
        check("skill visibility", visibility, SKILL_VISIBILITIES)?;
        let team_id = match (visibility, team_id.map(str::trim).filter(|t| !t.is_empty())) {
            ("team", None) => return Err(DbError::Conflict("team visibility requires team_id".into())),
            ("team", Some(t)) => Some(t),
            _ => None,
        };
        let res = sqlx::query("UPDATE skills SET visibility = ?, team_id = ?, updated_at = ? WHERE id = ?")
            .bind(visibility)
            .bind(team_id)
            .bind(now)
            .bind(skill_id)
            .execute(&self.pool)
            .await?;
        if res.rows_affected() == 0 {
            return Err(DbError::NotFound(format!("skill {skill_id}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repository::skill::{ISkillRepository, UpsertSkillParams};
    use crate::{SqliteSkillRepository, init_database_memory};

    async fn setup() -> (
        crate::Database,
        SqliteSkillRepository,
        SqliteSkillSourceRepository,
        String,
    ) {
        let db = init_database_memory().await.unwrap();
        let skills = SqliteSkillRepository::new(db.pool().clone());
        let repo = SqliteSkillSourceRepository::new(db.pool().clone());
        let skill = skills
            .upsert(UpsertSkillParams {
                name: "imported",
                description: None,
                path: "/tmp/imported",
                source: "user",
                enabled: true,
            })
            .await
            .unwrap();
        (db, skills, repo, skill.id)
    }

    fn version<'a>(id: &'a str, skill_id: &'a str, hash: &'a str, has_scripts: bool) -> AppendSkillVersionParams<'a> {
        AppendSkillVersionParams {
            id,
            skill_id,
            content_hash: hash,
            manifest: "{}",
            has_scripts,
            source_ref: None,
            created_by: "import",
            now: 10,
        }
    }

    #[tokio::test]
    async fn new_skill_columns_default_to_private_and_unreviewed() {
        let (_db, _skills, repo, skill_id) = setup().await;
        let gov = repo.governance(&skill_id).await.unwrap().unwrap();
        assert_eq!(gov.visibility, "private");
        assert_eq!(gov.team_id, None);
        assert_eq!(gov.review_status, "none");
    }

    #[tokio::test]
    async fn source_upsert_keeps_one_row_per_skill_and_records_fetches() {
        let (_db, _skills, repo, skill_id) = setup().await;
        let first = repo
            .upsert_source(UpsertSkillSourceParams {
                id: "src_1",
                user_id: Some("system_default_user"),
                skill_id: &skill_id,
                kind: "git",
                locator: "https://example.com/skills.git",
                git_ref: Some("main"),
                subpath: Some("skills/foo"),
                origin_app: None,
                trust: "untrusted",
                now: 1,
            })
            .await
            .unwrap();
        let second = repo
            .upsert_source(UpsertSkillSourceParams {
                id: "src_2",
                user_id: Some("system_default_user"),
                skill_id: &skill_id,
                kind: "git",
                locator: "https://example.com/skills.git",
                git_ref: Some("v2"),
                subpath: Some("skills/foo"),
                origin_app: Some("claude"),
                trust: "untrusted",
                now: 2,
            })
            .await
            .unwrap();
        assert_eq!(second.id, first.id);
        assert_eq!(second.git_ref.as_deref(), Some("v2"));
        assert_eq!(second.origin_app.as_deref(), Some("claude"));
        assert_eq!(second.created_at, 1);

        let fetched = repo.record_fetch(&skill_id, 5, Some("abc123")).await.unwrap();
        assert_eq!(fetched.last_fetched_at, Some(5));
        assert_eq!(fetched.last_commit.as_deref(), Some("abc123"));
        let kept = repo.record_fetch(&skill_id, 6, None).await.unwrap();
        assert_eq!(kept.last_commit.as_deref(), Some("abc123"));

        let bad = repo
            .upsert_source(UpsertSkillSourceParams {
                id: "src_3",
                user_id: None,
                skill_id: &skill_id,
                kind: "ftp",
                locator: "x",
                git_ref: None,
                subpath: None,
                origin_app: None,
                trust: "untrusted",
                now: 3,
            })
            .await;
        assert!(bad.is_err());
    }

    #[tokio::test]
    async fn versions_are_numbered_deduplicated_and_append_only() {
        let (db, _skills, repo, skill_id) = setup().await;
        let v1 = repo
            .append_version(version("v_1", &skill_id, "h1", false))
            .await
            .unwrap();
        assert!(v1.created);
        assert_eq!(v1.version.version_no, 1);
        assert_eq!(v1.review_status, "none");

        let same = repo
            .append_version(version("v_dup", &skill_id, "h1", false))
            .await
            .unwrap();
        assert!(!same.created);
        assert_eq!(same.version.id, "v_1");

        let v2 = repo
            .append_version(version("v_2", &skill_id, "h2", false))
            .await
            .unwrap();
        assert_eq!(v2.version.version_no, 2);
        let listed: Vec<i64> = repo
            .list_versions(&skill_id)
            .await
            .unwrap()
            .into_iter()
            .map(|v| v.version_no)
            .collect();
        assert_eq!(listed, vec![2, 1]);
        assert_eq!(repo.latest_version(&skill_id).await.unwrap().unwrap().id, "v_2");
        assert_eq!(
            repo.find_version(&skill_id, 1).await.unwrap().unwrap().content_hash,
            "h1"
        );

        let update = sqlx::query("UPDATE skill_versions SET content_hash = 'x' WHERE id = 'v_1'")
            .execute(db.pool())
            .await;
        assert!(update.is_err(), "skill_versions must reject UPDATE");
        let delete = sqlx::query("DELETE FROM skill_versions WHERE id = 'v_1'")
            .execute(db.pool())
            .await;
        assert!(delete.is_err(), "skill_versions must reject DELETE");

        let missing = repo.append_version(version("v_x", "no_such_skill", "h", false)).await;
        assert!(matches!(missing, Err(DbError::NotFound(_))));
    }

    #[tokio::test]
    async fn script_versions_force_pending_review_and_hide_the_skill() {
        let (_db, skills, repo, skill_id) = setup().await;
        assert!(skills.find_by_name("imported").await.unwrap().is_some());

        let v1 = repo
            .append_version(version("v_1", &skill_id, "h1", true))
            .await
            .unwrap();
        assert_eq!(v1.review_status, "pending");
        assert!(
            skills.find_by_name("imported").await.unwrap().is_none(),
            "pending skills are not resolvable"
        );
        assert!(skills.list().await.unwrap().iter().all(|s| s.id != skill_id));

        repo.set_review_status(&skill_id, "approved", 20).await.unwrap();
        assert!(skills.find_by_name("imported").await.unwrap().is_some());

        // A changed script version goes back to pending, even after approval.
        let v2 = repo
            .append_version(version("v_2", &skill_id, "h2", true))
            .await
            .unwrap();
        assert_eq!(v2.review_status, "pending");
        assert!(skills.find_by_name("imported").await.unwrap().is_none());

        repo.set_review_status(&skill_id, "rejected", 30).await.unwrap();
        assert!(skills.find_by_name("imported").await.unwrap().is_none());
        assert!(repo.set_review_status(&skill_id, "maybe", 31).await.is_err());

        assert!(!crate::evolution_auto_apply_allowed(true));
        assert!(crate::evolution_auto_apply_allowed(false));
    }

    #[tokio::test]
    async fn team_visibility_requires_team_id() {
        let (_db, _skills, repo, skill_id) = setup().await;
        assert!(repo.set_visibility(&skill_id, "team", None, 1).await.is_err());
        repo.set_visibility(&skill_id, "team", Some("team_1"), 2).await.unwrap();
        let gov = repo.governance(&skill_id).await.unwrap().unwrap();
        assert_eq!(
            (gov.visibility.as_str(), gov.team_id.as_deref()),
            ("team", Some("team_1"))
        );
        repo.set_visibility(&skill_id, "private", Some("team_1"), 3)
            .await
            .unwrap();
        assert_eq!(repo.governance(&skill_id).await.unwrap().unwrap().team_id, None);
        assert!(repo.set_visibility("nope", "private", None, 4).await.is_err());
    }
}
