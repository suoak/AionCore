//! Review batches: send unresolved annotations to a conversation as one
//! `[[AION_REVIEW]]` input (design §2.2, §5.1 `review_batches`, §6.1).
//!
//! Delivery goes through the conversation input queue (`submit_input`), so a
//! batch is an ordinary user-originated input: it changes no `@@` session
//! grant and gives no agent cross-conversation access. The sent text is
//! frozen in `review_batches.content` for traceability.

use aionui_common::now_ms;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::error::ReviewError;
use crate::prompt::render_review_block;
use crate::service::{ReviewService, new_id};
use crate::types::{ListReviewThreadsQuery, ReviewThread, ReviewThreadStatus};

/// Most threads one batch may carry.
pub const MAX_BATCH_THREADS: usize = 200;
const MAX_PREFACE_BYTES: usize = 4 * 1024;

/// How a batch enters the target conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDeliveryMode {
    /// Mid-turn into the running turn.
    Steer,
    /// As the next message.
    Followup,
}

impl ReviewDeliveryMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Steer => "steer",
            Self::Followup => "followup",
        }
    }
}

/// Failure of the input sink, mapped to review errors.
#[derive(Debug)]
pub enum ReviewInputError {
    /// The conversation does not exist or is not the user's.
    NotFound,
    /// The conversation cannot take this mode right now (e.g. steer after the
    /// turn ended).
    Unsupported,
    Rejected(String),
}

/// Accepted input: its id and the conversation's receipt, passed through.
pub struct ReviewInputAccepted {
    pub input_id: String,
    pub receipt: serde_json::Value,
}

/// The conversation input queue, as seen by the review domain.
#[async_trait]
pub trait ReviewInputSink: Send + Sync {
    /// Whether a steer would land in a running turn now. `NotFound` when the
    /// conversation is not the user's.
    async fn steer_available(&self, user_id: &str, conversation_id: &str) -> Result<bool, ReviewInputError>;
    /// Submit `content` as user input. `client_key` makes retries idempotent.
    async fn submit(
        &self,
        user_id: &str,
        conversation_id: &str,
        mode: ReviewDeliveryMode,
        content: &str,
        client_key: &str,
    ) -> Result<ReviewInputAccepted, ReviewInputError>;
}

/// `POST /api/review-batches`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendReviewBatchRequest {
    pub target_conversation_id: String,
    /// `auto` (default: steer into a running turn, else followup), `steer`
    /// (falls back to followup when no turn is running) or `followup`.
    #[serde(default)]
    pub delivery: Option<String>,
    /// Threads to send. Empty = every unresolved thread of the scope.
    #[serde(default)]
    pub thread_ids: Vec<String>,
    #[serde(default)]
    pub scope_kind: Option<String>,
    #[serde(default)]
    pub scope_id: Option<String>,
    /// Replaces the default instruction line of the block.
    #[serde(default)]
    pub preface: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ReviewBatch {
    pub id: String,
    pub target_conversation_id: String,
    pub delivery: String,
    pub thread_ids: Vec<String>,
    pub content: String,
    pub input_id: Option<String>,
    pub status: String,
    pub error_message: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SendReviewBatchResponse {
    pub batch: ReviewBatch,
    pub input_receipt: serde_json::Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ListReviewBatchesQuery {
    pub target_conversation_id: String,
}

fn bad(message: impl Into<String>) -> ReviewError {
    ReviewError::BadRequest(message.into())
}

fn map_input_error(error: ReviewInputError) -> ReviewError {
    match error {
        ReviewInputError::NotFound => bad("target conversation not found"),
        ReviewInputError::Unsupported => ReviewError::Conflict {
            code: "REVIEW_DELIVERY_UNSUPPORTED",
            message: "The conversation cannot accept this input right now".into(),
        },
        ReviewInputError::Rejected(message) => ReviewError::Conflict {
            code: "REVIEW_DELIVERY_REJECTED",
            message,
        },
    }
}

impl ReviewService {
    /// Threads to send, freshly re-anchored, unresolved only, in file/line order.
    async fn batch_threads(
        &self,
        user_id: &str,
        req: &SendReviewBatchRequest,
    ) -> Result<Vec<ReviewThread>, ReviewError> {
        let mut threads = if req.thread_ids.is_empty() {
            if req.scope_kind.is_none() || req.scope_id.is_none() {
                return Err(bad("thread_ids or scope_kind + scope_id is required"));
            }
            self.list(
                user_id,
                ListReviewThreadsQuery {
                    scope_kind: req.scope_kind.clone(),
                    scope_id: req.scope_id.clone(),
                    file_path: None,
                    status: None,
                },
            )
            .await?
        } else {
            if req.thread_ids.len() > MAX_BATCH_THREADS {
                return Err(bad(format!("a batch carries at most {MAX_BATCH_THREADS} threads")));
            }
            let mut threads = Vec::with_capacity(req.thread_ids.len());
            for id in &req.thread_ids {
                if threads.iter().any(|thread: &ReviewThread| &thread.id == id) {
                    continue;
                }
                threads.push(self.get(user_id, id).await?);
            }
            threads
        };
        threads.retain(|thread| thread.status != ReviewThreadStatus::Resolved);
        if threads.is_empty() {
            return Err(bad("no unresolved review threads to send"));
        }
        if threads.len() > MAX_BATCH_THREADS {
            return Err(bad(format!("a batch carries at most {MAX_BATCH_THREADS} threads")));
        }
        threads.sort_by(|a, b| {
            (a.file_path.as_str(), a.start_line, a.created_at, a.id.as_str()).cmp(&(
                b.file_path.as_str(),
                b.start_line,
                b.created_at,
                b.id.as_str(),
            ))
        });
        Ok(threads)
    }

    /// `POST /api/review-batches`: render the batch, record it, deliver it
    /// through the conversation input queue, then mark open threads `sent`.
    pub async fn send_batch(
        &self,
        user_id: &str,
        req: SendReviewBatchRequest,
    ) -> Result<SendReviewBatchResponse, ReviewError> {
        let sink = self
            .input_sink
            .clone()
            .ok_or_else(|| ReviewError::Internal("review batches are not wired".into()))?;
        let target = req.target_conversation_id.trim().to_owned();
        if target.is_empty() || target.len() > 256 {
            return Err(bad("target_conversation_id is required"));
        }
        if req
            .preface
            .as_deref()
            .is_some_and(|text| text.len() > MAX_PREFACE_BYTES)
        {
            return Err(bad(format!("preface exceeds {MAX_PREFACE_BYTES} bytes")));
        }
        let requested = match req.delivery.as_deref().unwrap_or("auto") {
            "auto" => None,
            "steer" => Some(ReviewDeliveryMode::Steer),
            "followup" => Some(ReviewDeliveryMode::Followup),
            other => return Err(bad(format!("unknown delivery '{other}'"))),
        };
        // Ownership of the target is checked here, before anything is stored.
        let steer_available = sink.steer_available(user_id, &target).await.map_err(map_input_error)?;
        let mut mode = match requested {
            Some(ReviewDeliveryMode::Followup) => ReviewDeliveryMode::Followup,
            // Steer only lands in a running turn; otherwise it is a followup.
            _ if steer_available => ReviewDeliveryMode::Steer,
            _ => ReviewDeliveryMode::Followup,
        };

        let threads = self.batch_threads(user_id, &req).await?;
        let batch_id = new_id("rb");
        let content = render_review_block(&batch_id, &threads, req.preface.as_deref());
        let thread_ids: Vec<String> = threads.iter().map(|thread| thread.id.clone()).collect();
        let thread_ids_json = serde_json::to_string(&thread_ids)?;
        let created_at = now_ms();
        sqlx::query(
            "INSERT INTO review_batches (id, user_id, target_conversation_id, delivery, thread_ids, content, status, created_at) \
             VALUES (?, ?, ?, ?, ?, ?, 'sending', ?)",
        )
        .bind(&batch_id)
        .bind(user_id)
        .bind(&target)
        .bind(mode.as_str())
        .bind(&thread_ids_json)
        .bind(&content)
        .bind(created_at)
        .execute(&self.pool)
        .await?;

        let mut outcome = sink.submit(user_id, &target, mode, &content, &batch_id).await;
        if mode == ReviewDeliveryMode::Steer && matches!(outcome, Err(ReviewInputError::Unsupported)) {
            // The turn ended between the check and the send: queue it instead.
            mode = ReviewDeliveryMode::Followup;
            outcome = sink.submit(user_id, &target, mode, &content, &batch_id).await;
        }
        let accepted = match outcome {
            Ok(accepted) => accepted,
            Err(error) => {
                let error = map_input_error(error);
                sqlx::query(
                    "UPDATE review_batches SET status = 'failed', delivery = ?, error_message = ? WHERE id = ?",
                )
                .bind(mode.as_str())
                .bind(error.to_string())
                .bind(&batch_id)
                .execute(&self.pool)
                .await?;
                return Err(error);
            }
        };

        let mut tx = self.pool.begin().await?;
        sqlx::query("UPDATE review_batches SET status = 'accepted', delivery = ?, input_id = ? WHERE id = ?")
            .bind(mode.as_str())
            .bind(&accepted.input_id)
            .bind(&batch_id)
            .execute(&mut *tx)
            .await?;
        let now = now_ms();
        for id in &thread_ids {
            // Outdated threads keep their state; open ones become sent.
            sqlx::query(
                "UPDATE review_threads SET status = CASE WHEN status = 'open' THEN 'sent' ELSE status END, \
                 last_batch_id = ?, updated_at = ? WHERE id = ? AND user_id = ? AND status != 'resolved'",
            )
            .bind(&batch_id)
            .bind(now)
            .bind(id)
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        Ok(SendReviewBatchResponse {
            batch: self.load_batch(user_id, &batch_id).await?,
            input_receipt: accepted.receipt,
        })
    }

    /// `GET /api/review-batches?target_conversation_id=`: newest first.
    pub async fn list_batches(
        &self,
        user_id: &str,
        target_conversation_id: &str,
    ) -> Result<Vec<ReviewBatch>, ReviewError> {
        let rows = sqlx::query(&format!(
            "SELECT {BATCH_COLUMNS} FROM review_batches WHERE user_id = ? AND target_conversation_id = ? \
             ORDER BY created_at DESC, id DESC LIMIT 100"
        ))
        .bind(user_id)
        .bind(target_conversation_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(batch_from_row).collect()
    }

    async fn load_batch(&self, user_id: &str, id: &str) -> Result<ReviewBatch, ReviewError> {
        let row = sqlx::query(&format!(
            "SELECT {BATCH_COLUMNS} FROM review_batches WHERE id = ? AND user_id = ?"
        ))
        .bind(id)
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| ReviewError::Internal("review batch disappeared".into()))?;
        batch_from_row(&row)
    }
}

const BATCH_COLUMNS: &str =
    "id, target_conversation_id, delivery, thread_ids, content, input_id, status, error_message, created_at";

fn batch_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ReviewBatch, ReviewError> {
    let thread_ids: String = row.try_get("thread_ids")?;
    Ok(ReviewBatch {
        id: row.try_get("id")?,
        target_conversation_id: row.try_get("target_conversation_id")?,
        delivery: row.try_get("delivery")?,
        thread_ids: serde_json::from_str(&thread_ids)?,
        content: row.try_get("content")?,
        input_id: row.try_get("input_id")?,
        status: row.try_get("status")?,
        error_message: row.try_get("error_message")?,
        created_at: row.try_get("created_at")?,
    })
}

#[cfg(test)]
#[path = "batch_test.rs"]
mod tests;
