//! Review thread CRUD and lazy re-anchoring (design §5, §6.1).

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use aionui_common::now_ms;
use sqlx::{Row, SqlitePool};

use crate::anchor::{self, Anchor, AnchorError, Outcome};
use crate::error::ReviewError;
use crate::resolver::ScopeContentResolver;
use crate::types::{
    AddReviewCommentRequest, CreateReviewThreadRequest, DiffContext, ListReviewThreadsQuery, ReviewComment,
    ReviewCommentAuthor, ReviewScopeKind, ReviewSide, ReviewThread, ReviewThreadStatus, UpdateReviewThreadRequest,
};

/// Markdown body limit per comment (design §5.1).
pub const MAX_BODY_BYTES: usize = 8 * 1024;
/// Largest selection a single thread may cover.
pub const MAX_SELECTION_LINES: u32 = 2_000;
/// Files above this size are anchored from the diff context only.
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SCOPE_ID_LEN: usize = 4_096;
const MAX_FILE_PATH_LEN: usize = 1_024;
const MAX_REF_ID_LEN: usize = 256;
const WORKTREE_BASE: &str = "WORKTREE";
const ANCHOR_STALE: &str = "ANCHOR_STALE";

pub struct ReviewService {
    pool: SqlitePool,
    resolver: Arc<dyn ScopeContentResolver>,
}

/// The diff side as it is on disk right now.
enum SideText {
    Text(String),
    Missing,
    Binary,
    /// Too large, outside the scope root, or unreadable: keep what we have.
    Unavailable,
}

struct ThreadRow {
    id: String,
    scope_kind: ReviewScopeKind,
    scope_id: String,
    conversation_id: Option<String>,
    task_session_id: Option<String>,
    run_id: Option<String>,
    file_path: String,
    side: ReviewSide,
    start_line: u32,
    end_line: u32,
    anchor_json: String,
    anchor: Anchor,
    status: ReviewThreadStatus,
    last_batch_id: Option<String>,
    created_at: i64,
    updated_at: i64,
    resolved_at: Option<i64>,
}

impl ThreadRow {
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Self, ReviewError> {
        let anchor_json: String = row.try_get("anchor")?;
        let anchor = serde_json::from_str(&anchor_json)?;
        Ok(Self {
            id: row.try_get("id")?,
            scope_kind: ReviewScopeKind::parse(&row.try_get::<String, _>("scope_kind")?)?,
            scope_id: row.try_get("scope_id")?,
            conversation_id: row.try_get("conversation_id")?,
            task_session_id: row.try_get("task_session_id")?,
            run_id: row.try_get("run_id")?,
            file_path: row.try_get("file_path")?,
            side: ReviewSide::parse(&row.try_get::<String, _>("side")?)?,
            start_line: u32::try_from(row.try_get::<i64, _>("start_line")?).unwrap_or(1),
            end_line: u32::try_from(row.try_get::<i64, _>("end_line")?).unwrap_or(1),
            anchor_json,
            anchor,
            status: ReviewThreadStatus::parse(&row.try_get::<String, _>("status")?)?,
            last_batch_id: row.try_get("last_batch_id")?,
            created_at: row.try_get("created_at")?,
            updated_at: row.try_get("updated_at")?,
            resolved_at: row.try_get("resolved_at")?,
        })
    }

    /// Status a thread returns to when it is found again: `sent` once it was
    /// part of a batch, else `open`.
    fn live_status(&self) -> ReviewThreadStatus {
        if self.last_batch_id.is_some() {
            ReviewThreadStatus::Sent
        } else {
            ReviewThreadStatus::Open
        }
    }

    fn into_thread(self, comments: Vec<ReviewComment>) -> ReviewThread {
        ReviewThread {
            id: self.id,
            scope_kind: self.scope_kind,
            scope_id: self.scope_id,
            conversation_id: self.conversation_id,
            task_session_id: self.task_session_id,
            run_id: self.run_id,
            file_path: self.file_path,
            side: self.side,
            start_line: self.start_line,
            end_line: self.end_line,
            status: self.status,
            moved_fuzzy: self.anchor.moved_fuzzy,
            anchor: self.anchor,
            last_batch_id: self.last_batch_id,
            comments,
            created_at: self.created_at,
            updated_at: self.updated_at,
            resolved_at: self.resolved_at,
        }
    }
}

const THREAD_COLUMNS: &str = "id, scope_kind, scope_id, conversation_id, task_session_id, run_id, file_path, side, \
     start_line, end_line, anchor, status, last_batch_id, created_at, updated_at, resolved_at";

fn new_id(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::now_v7().simple())
}

fn bad(message: impl Into<String>) -> ReviewError {
    ReviewError::BadRequest(message.into())
}

fn stale(message: impl Into<String>) -> ReviewError {
    ReviewError::Conflict {
        code: ANCHOR_STALE,
        message: message.into(),
    }
}

/// Repo-relative POSIX path; no absolute paths, drive letters or `..`.
pub(crate) fn normalize_file_path(raw: &str) -> Result<String, ReviewError> {
    let path = raw.trim().replace('\\', "/");
    let invalid = || bad("file_path must be a repository-relative path");
    if path.is_empty() || path.len() > MAX_FILE_PATH_LEN || path.contains('\0') || path.starts_with('/') {
        return Err(invalid());
    }
    if path.as_bytes().get(1) == Some(&b':') {
        return Err(invalid());
    }
    if path
        .split('/')
        .any(|segment| segment.is_empty() || segment == "." || segment == "..")
    {
        return Err(invalid());
    }
    Ok(path)
}

fn validate_body(body: &str) -> Result<String, ReviewError> {
    if body.trim().is_empty() {
        return Err(bad("body must not be empty"));
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(bad(format!("body exceeds {MAX_BODY_BYTES} bytes")));
    }
    Ok(body.to_owned())
}

fn validate_range(start: u32, end: u32) -> Result<(), ReviewError> {
    if start == 0 || end < start {
        return Err(bad("start_line must be ≥ 1 and end_line ≥ start_line"));
    }
    if end - start >= MAX_SELECTION_LINES {
        return Err(bad(format!("a thread may cover at most {MAX_SELECTION_LINES} lines")));
    }
    Ok(())
}

fn optional_ref(value: Option<String>, field: &str) -> Result<Option<String>, ReviewError> {
    match value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()) {
        Some(v) if v.len() > MAX_REF_ID_LEN || v.contains('\0') => Err(bad(format!("invalid {field}"))),
        other => Ok(other),
    }
}

fn validate_scope_id(scope_id: &str) -> Result<(), ReviewError> {
    if scope_id.trim().is_empty() || scope_id.len() > MAX_SCOPE_ID_LEN || scope_id.contains('\0') {
        return Err(bad("invalid scope_id"));
    }
    Ok(())
}

/// Read the current new-side file under `root`, refusing paths that escape
/// it through symlinks.
async fn read_side(root: &Path, file_path: &str) -> SideText {
    let joined = root.join(file_path);
    let canonical = match tokio::fs::canonicalize(&joined).await {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return SideText::Missing,
        Err(_) => return SideText::Unavailable,
    };
    if !canonical.starts_with(root) {
        return SideText::Unavailable;
    }
    match tokio::fs::metadata(&canonical).await {
        Ok(meta) if meta.is_file() && meta.len() <= MAX_FILE_BYTES => {}
        Ok(meta) if meta.is_dir() => return SideText::Missing,
        _ => return SideText::Unavailable,
    }
    match tokio::fs::read(&canonical).await {
        Ok(bytes) if bytes.iter().take(8_000).any(|b| *b == 0) => SideText::Binary,
        Ok(bytes) => SideText::Text(String::from_utf8_lossy(&bytes).into_owned()),
        Err(_) => SideText::Unavailable,
    }
}

/// Whether a thread's new side is the live working tree (lazily re-anchored).
fn tracks_worktree(side: ReviewSide, anchor_base: Option<&str>) -> bool {
    side == ReviewSide::New && anchor_base.is_none_or(|base| base == WORKTREE_BASE)
}

/// Build the anchor for a new or moved selection. Returns the anchor and the
/// final (possibly corrected) line range.
fn build_anchor(
    side_text: Option<&SideText>,
    ctx: &DiffContext,
    base: Option<String>,
    start: u32,
    end: u32,
) -> Result<(Anchor, u32, u32), ReviewError> {
    let client = Anchor::from_context(
        &ctx.before,
        &ctx.lines,
        &ctx.after,
        base.clone(),
        ctx.hunk_header.clone(),
    );
    match side_text {
        Some(SideText::Text(text)) => {
            let (start, end) = if client.lines.is_empty() || client.matches_at(text, start, end) {
                (start, end)
            } else {
                // The diff the user looked at is older than the file: follow
                // the selection like a lazy re-anchor would, or refuse.
                match anchor::reanchor(&client, start, text) {
                    Outcome::Moved {
                        start_line,
                        end_line,
                        fuzzy: false,
                    } => (start_line, end_line),
                    _ => return Err(stale("the diff is out of date; refresh it and try again")),
                }
            };
            let anchor =
                Anchor::from_text(text, start, end, base, ctx.hunk_header.clone()).map_err(|error| match error {
                    AnchorError::OutOfRange { .. } => stale(error.to_string()),
                    AnchorError::InvalidRange { .. } => bad(error.to_string()),
                })?;
            Ok((anchor, start, end))
        }
        Some(SideText::Binary) => Err(bad("binary files cannot be annotated")),
        Some(SideText::Missing) => Err(stale("the file no longer exists; refresh the diff")),
        Some(SideText::Unavailable) | None => {
            if client.lines.len() as u64 != u64::from(end - start) + 1 {
                return Err(bad("diff_context.lines must contain exactly the selected lines"));
            }
            Ok((client, start, end))
        }
    }
}

/// What lazy re-anchoring decided for one thread.
struct Relocation {
    start_line: u32,
    end_line: u32,
    anchor: Anchor,
    status: ReviewThreadStatus,
}

fn relocate(row: &ThreadRow, side: &SideText) -> Option<Relocation> {
    let outdated = || Relocation {
        start_line: row.start_line,
        end_line: row.end_line,
        anchor: row.anchor.clone(),
        status: ReviewThreadStatus::Outdated,
    };
    let next = match side {
        SideText::Missing => outdated(),
        SideText::Binary | SideText::Unavailable => return None,
        SideText::Text(text) => match anchor::reanchor(&row.anchor, row.start_line, text) {
            Outcome::Unchanged => Relocation {
                start_line: row.start_line,
                end_line: row.end_line,
                anchor: row.anchor.clone(),
                status: if row.status == ReviewThreadStatus::Outdated {
                    row.live_status()
                } else {
                    row.status
                },
            },
            Outcome::Moved {
                start_line,
                end_line,
                fuzzy,
            } => match row.anchor.rebased(text, start_line, end_line, fuzzy) {
                Ok(anchor) => Relocation {
                    start_line,
                    end_line,
                    anchor,
                    status: if row.status == ReviewThreadStatus::Outdated {
                        row.live_status()
                    } else {
                        row.status
                    },
                },
                Err(_) => outdated(),
            },
            Outcome::Outdated => outdated(),
        },
    };
    let changed = next.start_line != row.start_line
        || next.end_line != row.end_line
        || next.status != row.status
        || next.anchor != row.anchor;
    changed.then_some(next)
}

impl ReviewService {
    pub fn new(pool: SqlitePool, resolver: Arc<dyn ScopeContentResolver>) -> Self {
        Self { pool, resolver }
    }

    async fn ensure_owned(&self, table: &str, id: &str, user_id: &str, field: &str) -> Result<(), ReviewError> {
        let sql = format!("SELECT COUNT(*) FROM {table} WHERE id = ? AND user_id = ?");
        let count: i64 = sqlx::query_scalar(&sql)
            .bind(id)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        if count == 0 {
            return Err(bad(format!("{field} not found")));
        }
        Ok(())
    }

    async fn side_text(
        &self,
        user_id: &str,
        row_kind: ReviewScopeKind,
        scope_id: &str,
        file_path: &str,
    ) -> Option<SideText> {
        let root = self.resolver.scope_root(user_id, row_kind, scope_id).await?;
        Some(read_side(&root, file_path).await)
    }

    /// `POST /api/review-threads`.
    pub async fn create(&self, user_id: &str, req: CreateReviewThreadRequest) -> Result<ReviewThread, ReviewError> {
        let kind = ReviewScopeKind::parse(&req.scope_kind)?;
        let side = ReviewSide::parse(&req.side)?;
        validate_scope_id(&req.scope_id)?;
        let file_path = normalize_file_path(&req.file_path)?;
        validate_range(req.start_line, req.end_line)?;
        let body = validate_body(&req.body)?;
        let conversation_id = optional_ref(req.conversation_id, "conversation_id")?;
        let task_session_id = optional_ref(req.task_session_id, "task_session_id")?;
        let run_id = optional_ref(req.run_id, "run_id")?;
        let scope_id = self
            .resolver
            .canonical_scope_id(user_id, kind, &req.scope_id)
            .await
            .ok_or_else(|| bad("scope_id is not an accessible repository directory"))?;
        if let Some(id) = &conversation_id {
            self.ensure_owned("conversations", id, user_id, "conversation_id")
                .await?;
        }
        if let Some(id) = &task_session_id {
            self.ensure_owned("task_sessions", id, user_id, "task_session_id")
                .await?;
        }

        let ctx = req.diff_context;
        let base = ctx.base.clone().or_else(|| {
            (side == ReviewSide::New && kind != ReviewScopeKind::TaskRun).then(|| WORKTREE_BASE.to_owned())
        });
        let side_text = if tracks_worktree(side, base.as_deref()) {
            self.side_text(user_id, kind, &scope_id, &file_path).await
        } else {
            None
        };
        let (anchor, start_line, end_line) =
            build_anchor(side_text.as_ref(), &ctx, base, req.start_line, req.end_line)?;

        let id = new_id("rt");
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_threads (id, user_id, scope_kind, scope_id, conversation_id, task_session_id, run_id, \
             file_path, side, start_line, end_line, anchor, status, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'open', ?, ?)",
        )
        .bind(&id)
        .bind(user_id)
        .bind(kind.as_str())
        .bind(&scope_id)
        .bind(&conversation_id)
        .bind(&task_session_id)
        .bind(&run_id)
        .bind(&file_path)
        .bind(side.as_str())
        .bind(i64::from(start_line))
        .bind(i64::from(end_line))
        .bind(serde_json::to_string(&anchor)?)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at) VALUES (?, ?, 'user', ?, ?, ?)",
        )
        .bind(new_id("rc"))
        .bind(&id)
        .bind(&body)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        tracing::info!(thread_id = %id, scope_kind = kind.as_str(), "review thread created");
        self.load(user_id, &id).await
    }

    async fn fetch_row(&self, user_id: &str, id: &str) -> Result<ThreadRow, ReviewError> {
        let sql = format!("SELECT {THREAD_COLUMNS} FROM review_threads WHERE id = ? AND user_id = ?");
        let row = sqlx::query(&sql)
            .bind(id)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or(ReviewError::NotFound)?;
        ThreadRow::from_row(&row)
    }

    async fn comments_for(&self, thread_ids: &[String]) -> Result<HashMap<String, Vec<ReviewComment>>, ReviewError> {
        let mut out: HashMap<String, Vec<ReviewComment>> = HashMap::new();
        for chunk in thread_ids.chunks(200) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT id, thread_id, author, body, created_at, updated_at FROM review_comments \
                 WHERE thread_id IN ({placeholders}) ORDER BY created_at, id"
            );
            let mut query = sqlx::query(&sql);
            for id in chunk {
                query = query.bind(id);
            }
            for row in query.fetch_all(&self.pool).await? {
                let comment = ReviewComment {
                    id: row.try_get("id")?,
                    thread_id: row.try_get("thread_id")?,
                    author: ReviewCommentAuthor::parse(&row.try_get::<String, _>("author")?)?,
                    body: row.try_get("body")?,
                    created_at: row.try_get("created_at")?,
                    updated_at: row.try_get("updated_at")?,
                };
                out.entry(comment.thread_id.clone()).or_default().push(comment);
            }
        }
        Ok(out)
    }

    async fn hydrate(&self, rows: Vec<ThreadRow>) -> Result<Vec<ReviewThread>, ReviewError> {
        let ids: Vec<String> = rows.iter().map(|row| row.id.clone()).collect();
        let mut comments = self.comments_for(&ids).await?;
        Ok(rows
            .into_iter()
            .map(|row| {
                let list = comments.remove(&row.id).unwrap_or_default();
                row.into_thread(list)
            })
            .collect())
    }

    /// Re-anchor unresolved working-tree threads against the files as they
    /// are now, persisting every change (design §5.2 "lazy").
    async fn reanchor_rows(&self, user_id: &str, rows: &mut [ThreadRow]) -> Result<(), ReviewError> {
        let mut files: HashMap<(ReviewScopeKind, String, String), Option<SideText>> = HashMap::new();
        for row in rows.iter_mut() {
            if row.status == ReviewThreadStatus::Resolved || !tracks_worktree(row.side, row.anchor.base.as_deref()) {
                continue;
            }
            let key = (row.scope_kind, row.scope_id.clone(), row.file_path.clone());
            if !files.contains_key(&key) {
                let text = self
                    .side_text(user_id, row.scope_kind, &row.scope_id, &row.file_path)
                    .await;
                files.insert(key.clone(), text);
            }
            let Some(Some(side)) = files.get(&key) else {
                continue;
            };
            let Some(next) = relocate(row, side) else {
                continue;
            };
            let anchor_json = serde_json::to_string(&next.anchor)?;
            // Optimistic: skip if the user edited the thread meanwhile.
            let result = sqlx::query(
                "UPDATE review_threads SET start_line = ?, end_line = ?, anchor = ?, status = ? \
                 WHERE id = ? AND user_id = ? AND anchor = ? AND status = ?",
            )
            .bind(i64::from(next.start_line))
            .bind(i64::from(next.end_line))
            .bind(&anchor_json)
            .bind(next.status.as_str())
            .bind(&row.id)
            .bind(user_id)
            .bind(&row.anchor_json)
            .bind(row.status.as_str())
            .execute(&self.pool)
            .await?;
            if result.rows_affected() == 1 {
                row.start_line = next.start_line;
                row.end_line = next.end_line;
                row.anchor = next.anchor;
                row.anchor_json = anchor_json;
                row.status = next.status;
            }
        }
        Ok(())
    }

    async fn load(&self, user_id: &str, id: &str) -> Result<ReviewThread, ReviewError> {
        let row = self.fetch_row(user_id, id).await?;
        let mut threads = self.hydrate(vec![row]).await?;
        threads.pop().ok_or(ReviewError::NotFound)
    }

    /// `GET /api/review-threads/{id}` (re-anchored).
    pub async fn get(&self, user_id: &str, id: &str) -> Result<ReviewThread, ReviewError> {
        let mut rows = vec![self.fetch_row(user_id, id).await?];
        self.reanchor_rows(user_id, &mut rows).await?;
        let mut threads = self.hydrate(rows).await?;
        threads.pop().ok_or(ReviewError::NotFound)
    }

    /// `GET /api/review-threads?scope_kind=&scope_id=&file_path=&status=`.
    /// Re-anchors first, so a `status` filter sees fresh state.
    pub async fn list(&self, user_id: &str, query: ListReviewThreadsQuery) -> Result<Vec<ReviewThread>, ReviewError> {
        let kind = ReviewScopeKind::parse(
            query
                .scope_kind
                .as_deref()
                .ok_or_else(|| bad("scope_kind is required"))?,
        )?;
        let raw_scope = query.scope_id.ok_or_else(|| bad("scope_id is required"))?;
        validate_scope_id(&raw_scope)?;
        let scope_id = self
            .resolver
            .canonical_scope_id(user_id, kind, &raw_scope)
            .await
            .unwrap_or(raw_scope);
        let file_path = query.file_path.as_deref().map(normalize_file_path).transpose()?;
        let status = query.status.as_deref().map(ReviewThreadStatus::parse).transpose()?;

        let mut sql = format!(
            "SELECT {THREAD_COLUMNS} FROM review_threads WHERE user_id = ? AND scope_kind = ? AND scope_id = ?"
        );
        if file_path.is_some() {
            sql.push_str(" AND file_path = ?");
        }
        sql.push_str(" ORDER BY file_path, start_line, created_at, id");
        let mut q = sqlx::query(&sql).bind(user_id).bind(kind.as_str()).bind(&scope_id);
        if let Some(path) = &file_path {
            q = q.bind(path);
        }
        let mut rows = q
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(ThreadRow::from_row)
            .collect::<Result<Vec<_>, _>>()?;
        self.reanchor_rows(user_id, &mut rows).await?;
        if let Some(status) = status {
            rows.retain(|row| row.status == status);
        }
        rows.sort_by(|a, b| {
            (a.file_path.as_str(), a.start_line, a.created_at, a.id.as_str()).cmp(&(
                b.file_path.as_str(),
                b.start_line,
                b.created_at,
                b.id.as_str(),
            ))
        });
        self.hydrate(rows).await
    }

    /// `PATCH /api/review-threads/{id}`: edit the first comment and/or the range.
    pub async fn update(
        &self,
        user_id: &str,
        id: &str,
        req: UpdateReviewThreadRequest,
    ) -> Result<ReviewThread, ReviewError> {
        let row = self.fetch_row(user_id, id).await?;
        if req.body.is_none() && req.start_line.is_none() && req.end_line.is_none() {
            return Err(bad("nothing to update"));
        }
        let body = req.body.as_deref().map(validate_body).transpose()?;
        let range = if req.start_line.is_some() || req.end_line.is_some() {
            let start = req.start_line.unwrap_or(row.start_line);
            let end = req.end_line.unwrap_or(row.end_line);
            validate_range(start, end)?;
            let ctx = req.diff_context.unwrap_or_default();
            let base = ctx.base.clone().or_else(|| row.anchor.base.clone());
            let side_text = if tracks_worktree(row.side, base.as_deref()) {
                self.side_text(user_id, row.scope_kind, &row.scope_id, &row.file_path)
                    .await
            } else {
                None
            };
            let ctx = DiffContext {
                hunk_header: ctx.hunk_header.or_else(|| row.anchor.hunk_header.clone()),
                ..ctx
            };
            Some(build_anchor(side_text.as_ref(), &ctx, base, start, end)?)
        } else {
            None
        };

        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        if let Some((anchor, start, end)) = range {
            let status = if row.status == ReviewThreadStatus::Outdated {
                row.live_status()
            } else {
                row.status
            };
            sqlx::query(
                "UPDATE review_threads SET start_line = ?, end_line = ?, anchor = ?, status = ?, updated_at = ? \
                 WHERE id = ? AND user_id = ?",
            )
            .bind(i64::from(start))
            .bind(i64::from(end))
            .bind(serde_json::to_string(&anchor)?)
            .bind(status.as_str())
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }

        if let Some(body) = body {
            let result = sqlx::query(
                "UPDATE review_comments SET body = ?, updated_at = ? WHERE id = (\
                 SELECT id FROM review_comments WHERE thread_id = ? AND author = 'user' \
                 ORDER BY created_at, id LIMIT 1)",
            )
            .bind(&body)
            .bind(now)
            .bind(id)
            .execute(&mut *tx)
            .await?;
            if result.rows_affected() == 0 {
                return Err(ReviewError::Internal("review thread has no user comment".into()));
            }
            sqlx::query("UPDATE review_threads SET updated_at = ? WHERE id = ? AND user_id = ?")
                .bind(now)
                .bind(id)
                .bind(user_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.load(user_id, id).await
    }

    /// `POST /api/review-threads/{id}/resolve` (idempotent).
    pub async fn resolve(&self, user_id: &str, id: &str) -> Result<ReviewThread, ReviewError> {
        let row = self.fetch_row(user_id, id).await?;
        if row.status != ReviewThreadStatus::Resolved {
            let now = now_ms();
            sqlx::query(
                "UPDATE review_threads SET status = 'resolved', resolved_at = ?, updated_at = ? WHERE id = ? AND user_id = ?",
            )
            .bind(now)
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        }
        self.load(user_id, id).await
    }

    /// `POST /api/review-threads/{id}/reopen`: resolved → open. Other states
    /// are already unresolved, so this is a no-op for them.
    pub async fn reopen(&self, user_id: &str, id: &str) -> Result<ReviewThread, ReviewError> {
        let row = self.fetch_row(user_id, id).await?;
        if row.status == ReviewThreadStatus::Resolved {
            sqlx::query(
                "UPDATE review_threads SET status = 'open', resolved_at = NULL, updated_at = ? WHERE id = ? AND user_id = ?",
            )
            .bind(now_ms())
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        }
        // Re-anchor right away: the file may have moved on while resolved.
        self.get(user_id, id).await
    }

    /// `POST /api/review-threads/{id}/comments` — a user reply.
    pub async fn add_comment(
        &self,
        user_id: &str,
        id: &str,
        req: AddReviewCommentRequest,
    ) -> Result<ReviewThread, ReviewError> {
        self.fetch_row(user_id, id).await?;
        let body = validate_body(&req.body)?;
        let now = now_ms();
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO review_comments (id, thread_id, author, body, created_at, updated_at) VALUES (?, ?, 'user', ?, ?, ?)",
        )
        .bind(new_id("rc"))
        .bind(id)
        .bind(&body)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE review_threads SET updated_at = ? WHERE id = ? AND user_id = ?")
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.load(user_id, id).await
    }

    /// `DELETE /api/review-threads/{id}` (comments cascade).
    pub async fn delete(&self, user_id: &str, id: &str) -> Result<(), ReviewError> {
        let result = sqlx::query("DELETE FROM review_threads WHERE id = ? AND user_id = ?")
            .bind(id)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        if result.rows_affected() == 0 {
            return Err(ReviewError::NotFound);
        }
        tracing::info!(thread_id = %id, "review thread deleted");
        Ok(())
    }
}

#[cfg(test)]
#[path = "service_test.rs"]
mod tests;
