#![allow(clippy::disallowed_types)]
//! `/api/audit/*` (M8 E12-2): read and export the append-only audit ledger.
//!
//! Signed-in user only: agent and device tokens get 403 here (they are also
//! absent from the agent / device route whitelists, so they are refused
//! before reaching a handler). Until roles land (E12-3) a user only sees
//! events recorded with their own `user_id`.
//!
//! - `GET /api/audit/events?from&to&action&action_prefix&actor&actor_kind&before_seq&limit`
//!   newest first; `next_before_seq` pages backwards.
//! - `GET /api/audit/verify` — re-hash the whole chain.
//! - `GET /api/audit/export.csv?from&to&action&action_prefix&actor&actor_kind`
//!   chain order, with the chain verification in `X-Audit-Chain-*` headers.
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;
use aionui_db::{
    AUDIT_LIST_MAX_LIMIT, AuditActorKind, AuditChainVerification, AuditEventFilter, AuditEventRow, AuditOrder,
    IAuditEventRepository,
};

use crate::device::DevicePrincipal;
use crate::middleware::{AgentPrincipal, CurrentUser};

/// Most rows one CSV export carries; narrow the time range for more.
pub const AUDIT_EXPORT_MAX_ROWS: usize = 100_000;

#[derive(Clone)]
pub struct AuditRouterState {
    pub repo: Arc<dyn IAuditEventRepository>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AuditQuery {
    /// Inclusive lower bound, epoch ms.
    pub from: Option<i64>,
    /// Inclusive upper bound, epoch ms.
    pub to: Option<i64>,
    pub action: Option<String>,
    pub action_prefix: Option<String>,
    /// Actor id (user id, conversation id, device id).
    pub actor: Option<String>,
    pub actor_kind: Option<String>,
    pub before_seq: Option<i64>,
    pub limit: Option<i64>,
}

/// One event as returned by the API (`diff` parsed back to JSON).
#[derive(Debug, Serialize)]
pub struct AuditEventView {
    pub seq: i64,
    pub id: String,
    pub ts: i64,
    pub user_id: Option<String>,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub via: String,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub summary: String,
    pub diff: Option<Value>,
    pub approver_id: Option<String>,
    pub approval_id: Option<String>,
    pub request_fingerprint: Option<String>,
    pub prev_hash: String,
    pub hash: String,
}

impl From<AuditEventRow> for AuditEventView {
    fn from(row: AuditEventRow) -> Self {
        let diff = row
            .diff
            .as_deref()
            .map(|text| serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_owned())));
        Self {
            seq: row.seq,
            id: row.id,
            ts: row.ts,
            user_id: row.user_id,
            actor_kind: row.actor_kind,
            actor_id: row.actor_id,
            via: row.via,
            action: row.action,
            target_kind: row.target_kind,
            target_id: row.target_id,
            summary: row.summary,
            diff,
            approver_id: row.approver_id,
            approval_id: row.approval_id,
            request_fingerprint: row.request_fingerprint,
            prev_hash: row.prev_hash,
            hash: row.hash,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct AuditEventPage {
    pub items: Vec<AuditEventView>,
    /// Pass as `before_seq` for the next (older) page; absent on the last.
    pub next_before_seq: Option<i64>,
}

fn ensure_user_session(
    device: &Option<Extension<DevicePrincipal>>,
    agent: &Option<Extension<AgentPrincipal>>,
) -> Result<(), ApiError> {
    if device.is_some() || agent.is_some() {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "AUDIT_REQUIRES_USER",
            "The audit log is only available to the signed-in user.",
            None,
        ));
    }
    Ok(())
}

fn db_unavailable(error: impl std::fmt::Display) -> ApiError {
    tracing::error!(error = %error, "audit store unavailable");
    ApiError::Internal("Audit store unavailable".into())
}

fn non_empty(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_owned)
}

fn build_filter(user: &CurrentUser, query: &AuditQuery) -> Result<AuditEventFilter, ApiError> {
    let actor_kind = non_empty(&query.actor_kind);
    if let Some(kind) = &actor_kind
        && AuditActorKind::parse(kind).is_none()
    {
        return Err(ApiError::BadRequest(
            "actor_kind must be one of user, agent, device, system".into(),
        ));
    }
    if let (Some(from), Some(to)) = (query.from, query.to)
        && from > to
    {
        return Err(ApiError::BadRequest("from must not be after to".into()));
    }
    if let Some(limit) = query.limit
        && !(1..=AUDIT_LIST_MAX_LIMIT).contains(&limit)
    {
        return Err(ApiError::BadRequest(format!(
            "limit must be between 1 and {AUDIT_LIST_MAX_LIMIT}"
        )));
    }
    Ok(AuditEventFilter {
        user_id: Some(user.id.clone()),
        from_ts: query.from,
        to_ts: query.to,
        action: non_empty(&query.action),
        action_prefix: non_empty(&query.action_prefix),
        actor_id: non_empty(&query.actor),
        actor_kind,
        before_seq: query.before_seq,
        after_seq: None,
        order: AuditOrder::NewestFirst,
        limit: query.limit,
    })
}

async fn list_events_handler(
    State(state): State<AuditRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Query(query): Query<AuditQuery>,
) -> Result<Json<ApiResponse<AuditEventPage>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let filter = build_filter(&user, &query)?;
    let limit = filter.limit.unwrap_or(aionui_db::AUDIT_LIST_DEFAULT_LIMIT);
    let rows = state.repo.list(&filter).await.map_err(db_unavailable)?;
    let next_before_seq = if rows.len() as i64 == limit {
        rows.last().map(|row| row.seq)
    } else {
        None
    };
    Ok(Json(ApiResponse::ok(AuditEventPage {
        items: rows.into_iter().map(AuditEventView::from).collect(),
        next_before_seq,
    })))
}

async fn verify_handler(
    State(state): State<AuditRouterState>,
    Extension(_user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<AuditChainVerification>>, ApiError> {
    ensure_user_session(&device, &agent)?;
    let verification = state.repo.verify_chain().await.map_err(db_unavailable)?;
    Ok(Json(ApiResponse::ok(verification)))
}

/// RFC 4180 field with spreadsheet formula injection neutralised.
fn csv_field(value: &str) -> String {
    let value = if value.starts_with(['=', '+', '-', '@', '\t', '\r']) && value.parse::<f64>().is_err() {
        format!("'{value}")
    } else {
        value.to_owned()
    };
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value
    }
}

const CSV_HEADER: &[&str] = &[
    "seq",
    "id",
    "ts",
    "user_id",
    "actor_kind",
    "actor_id",
    "via",
    "action",
    "target_kind",
    "target_id",
    "summary",
    "diff",
    "approver_id",
    "approval_id",
    "request_fingerprint",
    "prev_hash",
    "hash",
];

fn csv_row(row: &AuditEventRow) -> String {
    let opt = |v: &Option<String>| v.clone().unwrap_or_default();
    let fields = [
        row.seq.to_string(),
        row.id.clone(),
        row.ts.to_string(),
        opt(&row.user_id),
        row.actor_kind.clone(),
        opt(&row.actor_id),
        row.via.clone(),
        row.action.clone(),
        opt(&row.target_kind),
        opt(&row.target_id),
        row.summary.clone(),
        opt(&row.diff),
        opt(&row.approver_id),
        opt(&row.approval_id),
        opt(&row.request_fingerprint),
        row.prev_hash.clone(),
        row.hash.clone(),
    ];
    fields.iter().map(|f| csv_field(f)).collect::<Vec<_>>().join(",")
}

async fn export_csv_handler(
    State(state): State<AuditRouterState>,
    Extension(user): Extension<CurrentUser>,
    device: Option<Extension<DevicePrincipal>>,
    agent: Option<Extension<AgentPrincipal>>,
    Query(query): Query<AuditQuery>,
) -> Result<Response, ApiError> {
    ensure_user_session(&device, &agent)?;
    let mut filter = build_filter(&user, &query)?;
    filter.before_seq = None;
    filter.order = AuditOrder::OldestFirst;
    filter.limit = Some(AUDIT_LIST_MAX_LIMIT);

    let mut out = CSV_HEADER.join(",");
    out.push_str("\r\n");
    let mut exported = 0_usize;
    let mut truncated = false;
    loop {
        let page = state.repo.list(&filter).await.map_err(db_unavailable)?;
        let Some(last) = page.last() else { break };
        filter.after_seq = Some(last.seq);
        for row in &page {
            if exported == AUDIT_EXPORT_MAX_ROWS {
                truncated = true;
                break;
            }
            out.push_str(&csv_row(row));
            out.push_str("\r\n");
            exported += 1;
        }
        if truncated || (page.len() as i64) < AUDIT_LIST_MAX_LIMIT {
            break;
        }
    }
    let verification = state.repo.verify_chain().await.map_err(db_unavailable)?;

    let mut response = (
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "attachment; filename=\"audit-events.csv\""),
            (header::CACHE_CONTROL, "no-store"),
        ],
        out,
    )
        .into_response();
    let headers = response.headers_mut();
    headers.insert(
        "x-audit-chain-ok",
        HeaderValue::from_static(if verification.ok { "true" } else { "false" }),
    );
    if let Ok(value) = HeaderValue::from_str(&verification.checked.to_string()) {
        headers.insert("x-audit-chain-checked", value);
    }
    if let Some(seq) = verification.first_broken_seq
        && let Ok(value) = HeaderValue::from_str(&seq.to_string())
    {
        headers.insert("x-audit-chain-broken-seq", value);
    }
    if let Some(head) = verification.head_hash.as_deref()
        && let Ok(value) = HeaderValue::from_str(head)
    {
        headers.insert("x-audit-chain-head", value);
    }
    if let Ok(value) = HeaderValue::from_str(&exported.to_string()) {
        headers.insert("x-audit-exported-rows", value);
    }
    if truncated {
        headers.insert("x-audit-export-truncated", HeaderValue::from_static("true"));
    }
    tracing::info!(
        target: "audit",
        action = "audit.export",
        user_id = %user.id,
        rows = exported,
        chain_ok = verification.ok,
        "audit log exported"
    );
    Ok(response)
}

/// `/api/audit/*`. Mount behind `auth_middleware`.
pub fn audit_routes(state: AuditRouterState) -> Router {
    Router::new()
        .route("/api/audit/events", get(list_events_handler))
        .route("/api/audit/verify", get(verify_handler))
        .route("/api/audit/export.csv", get(export_csv_handler))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_routes_require_a_user_session() {
        let device = Some(Extension(DevicePrincipal {
            device_id: "d1".into(),
            scopes: vec!["mobile_default".into()],
        }));
        let agent = Some(Extension(AgentPrincipal {
            conversation_id: "c1".into(),
        }));
        assert!(ensure_user_session(&None, &None).is_ok());
        assert!(ensure_user_session(&device, &None).is_err());
        assert!(ensure_user_session(&None, &agent).is_err());
    }

    #[test]
    fn csv_fields_are_quoted_and_neutralised() {
        assert_eq!(csv_field("plain"), "plain");
        assert_eq!(csv_field("a,b"), "\"a,b\"");
        assert_eq!(csv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_field("=cmd()"), "'=cmd()");
        assert_eq!(csv_field("-5"), "-5");
        assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    }

    #[test]
    fn filter_is_scoped_to_the_user_and_validated() {
        let user = CurrentUser::local_default();
        let filter = build_filter(&user, &AuditQuery::default()).unwrap();
        assert_eq!(filter.user_id.as_deref(), Some(user.id.as_str()));
        let bad_kind = AuditQuery {
            actor_kind: Some("robot".into()),
            ..Default::default()
        };
        assert!(build_filter(&user, &bad_kind).is_err());
        let reversed = AuditQuery {
            from: Some(2),
            to: Some(1),
            ..Default::default()
        };
        assert!(build_filter(&user, &reversed).is_err());
        let too_many = AuditQuery {
            limit: Some(AUDIT_LIST_MAX_LIMIT + 1),
            ..Default::default()
        };
        assert!(build_filter(&user, &too_many).is_err());
        let blank = AuditQuery {
            action: Some("  ".into()),
            ..Default::default()
        };
        assert!(build_filter(&user, &blank).unwrap().action.is_none());
    }
}
