use aionui_common::{generate_prefixed_id, now_ms};
use sqlx::{QueryBuilder, Sqlite, SqlitePool};

use crate::audit::{
    AUDIT_GENESIS_HASH, AuditHashInput, clean_summary, compute_audit_hash, is_valid_action, redacted_diff_text,
};
use crate::error::DbError;
use crate::models::AuditEventRow;
use crate::repository::audit_event::{
    AUDIT_LIST_DEFAULT_LIMIT, AUDIT_LIST_MAX_LIMIT, AuditChainVerification, AuditEventFilter, AuditOrder,
    IAuditEventRepository, NewAuditEvent,
};

const COLUMNS: &str = "seq, id, ts, user_id, actor_kind, actor_id, via, action, target_kind, target_id, summary, \
     diff, approver_id, approval_id, request_fingerprint, prev_hash, hash";
/// Attempts when another writer appended between reading the tail and
/// inserting (UNIQUE(prev_hash) violation).
const APPEND_ATTEMPTS: usize = 8;
const VERIFY_PAGE: i64 = 1000;
const OPTIONAL_TEXT_MAX_CHARS: usize = 256;

/// Serialises appends within this process (every repository instance shares
/// it), so the read-tail/insert pair does not race locally. UNIQUE(prev_hash)
/// plus the retry loop remains the guard against any other writer.
static APPEND_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Clone, Debug)]
pub struct SqliteAuditEventRepository {
    pool: SqlitePool,
}

impl SqliteAuditEventRepository {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }
}

fn clip(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(|v| v.chars().take(OPTIONAL_TEXT_MAX_CHARS).collect())
}

#[async_trait::async_trait]
impl IAuditEventRepository for SqliteAuditEventRepository {
    async fn append(&self, event: &NewAuditEvent) -> Result<AuditEventRow, DbError> {
        if !is_valid_action(&event.action) {
            return Err(DbError::Conflict(format!("invalid audit action '{}'", event.action)));
        }
        let ts = event.ts.unwrap_or_else(now_ms);
        let summary = clean_summary(&event.summary);
        let diff = event.diff.as_ref().map(redacted_diff_text);
        let user_id = clip(&event.user_id);
        let actor_id = clip(&event.actor_id);
        let target_kind = clip(&event.target_kind);
        let target_id = clip(&event.target_id);
        let approver_id = clip(&event.approver_id);
        let approval_id = clip(&event.approval_id);
        let request_fingerprint = clip(&event.request_fingerprint);

        let _guard = APPEND_LOCK.lock().await;
        let mut last_error = None;
        for _ in 0..APPEND_ATTEMPTS {
            let id = generate_prefixed_id("aud");
            let prev_hash: String = sqlx::query_scalar("SELECT hash FROM audit_events ORDER BY seq DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?
                .unwrap_or_else(|| AUDIT_GENESIS_HASH.to_owned());
            let hash = compute_audit_hash(&AuditHashInput {
                id: &id,
                ts,
                user_id: user_id.as_deref(),
                actor_kind: event.actor_kind.as_str(),
                actor_id: actor_id.as_deref(),
                via: event.via.as_str(),
                action: &event.action,
                target_kind: target_kind.as_deref(),
                target_id: target_id.as_deref(),
                summary: &summary,
                diff: diff.as_deref(),
                approver_id: approver_id.as_deref(),
                approval_id: approval_id.as_deref(),
                request_fingerprint: request_fingerprint.as_deref(),
                prev_hash: &prev_hash,
            });
            let sql = format!(
                "INSERT INTO audit_events (id, ts, user_id, actor_kind, actor_id, via, action, target_kind, \
                 target_id, summary, diff, approver_id, approval_id, request_fingerprint, prev_hash, hash) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING {COLUMNS}"
            );
            let result = sqlx::query_as::<_, AuditEventRow>(&sql)
                .bind(&id)
                .bind(ts)
                .bind(&user_id)
                .bind(event.actor_kind.as_str())
                .bind(&actor_id)
                .bind(event.via.as_str())
                .bind(&event.action)
                .bind(&target_kind)
                .bind(&target_id)
                .bind(&summary)
                .bind(&diff)
                .bind(&approver_id)
                .bind(&approval_id)
                .bind(&request_fingerprint)
                .bind(&prev_hash)
                .bind(&hash)
                .fetch_one(&self.pool)
                .await;
            match result {
                Ok(row) => return Ok(row),
                Err(error) => {
                    let error = DbError::from(error);
                    if error.is_unique_violation() || error.is_busy() {
                        // Another writer took this tail; re-read and retry.
                        last_error = Some(error);
                        continue;
                    }
                    return Err(error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| DbError::Conflict("audit append contention".into())))
    }

    async fn list(&self, filter: &AuditEventFilter) -> Result<Vec<AuditEventRow>, DbError> {
        let limit = filter
            .limit
            .unwrap_or(AUDIT_LIST_DEFAULT_LIMIT)
            .clamp(1, AUDIT_LIST_MAX_LIMIT);
        let mut qb: QueryBuilder<'_, Sqlite> =
            QueryBuilder::new(format!("SELECT {COLUMNS} FROM audit_events WHERE 1 = 1"));
        if let Some(user_id) = &filter.user_id {
            qb.push(" AND user_id = ").push_bind(user_id.clone());
        }
        if let Some(from_ts) = filter.from_ts {
            qb.push(" AND ts >= ").push_bind(from_ts);
        }
        if let Some(to_ts) = filter.to_ts {
            qb.push(" AND ts <= ").push_bind(to_ts);
        }
        if let Some(action) = &filter.action {
            qb.push(" AND action = ").push_bind(action.clone());
        }
        if let Some(prefix) = &filter.action_prefix {
            // substr comparison: no LIKE wildcards to escape.
            qb.push(" AND substr(action, 1, ")
                .push_bind(prefix.chars().count() as i64)
                .push(") = ")
                .push_bind(prefix.clone());
        }
        if let Some(actor_id) = &filter.actor_id {
            qb.push(" AND actor_id = ").push_bind(actor_id.clone());
        }
        if let Some(actor_kind) = &filter.actor_kind {
            qb.push(" AND actor_kind = ").push_bind(actor_kind.clone());
        }
        if let Some(before) = filter.before_seq {
            qb.push(" AND seq < ").push_bind(before);
        }
        if let Some(after) = filter.after_seq {
            qb.push(" AND seq > ").push_bind(after);
        }
        qb.push(match filter.order {
            AuditOrder::NewestFirst => " ORDER BY seq DESC",
            AuditOrder::OldestFirst => " ORDER BY seq ASC",
        });
        qb.push(" LIMIT ").push_bind(limit);
        Ok(qb.build_query_as::<AuditEventRow>().fetch_all(&self.pool).await?)
    }

    async fn verify_chain(&self) -> Result<AuditChainVerification, DbError> {
        let sql = format!("SELECT {COLUMNS} FROM audit_events WHERE seq > ? ORDER BY seq ASC LIMIT ?");
        let mut expected_prev = AUDIT_GENESIS_HASH.to_owned();
        let mut after = 0_i64;
        let mut checked = 0_i64;
        loop {
            let page = sqlx::query_as::<_, AuditEventRow>(&sql)
                .bind(after)
                .bind(VERIFY_PAGE)
                .fetch_all(&self.pool)
                .await?;
            if page.is_empty() {
                break;
            }
            for row in &page {
                let recomputed = compute_audit_hash(&AuditHashInput::from_row(row));
                if row.prev_hash != expected_prev || row.hash != recomputed {
                    return Ok(AuditChainVerification {
                        ok: false,
                        checked,
                        first_broken_seq: Some(row.seq),
                        head_hash: None,
                    });
                }
                checked += 1;
                expected_prev.clone_from(&row.hash);
                after = row.seq;
            }
        }
        Ok(AuditChainVerification {
            ok: true,
            checked,
            first_broken_seq: None,
            head_hash: (checked > 0).then_some(expected_prev),
        })
    }
}
