//! User approval for configuration writes requested by agent tokens.
//!
//! When an agent's helper CLI asks to change configuration (MCP servers,
//! providers, settings, custom agents, assistants, skills, cron), the auth
//! middleware parks the exact request here and notifies the app. The user
//! approves or denies it on a confirmation card; only then may the CLI replay
//! the *same* request (method, path, body) once, presenting the approval id.
//!
//! Approvals are in-memory, single-use, bound to (user, conversation,
//! request fingerprint) and expire after [`APPROVAL_TTL`].

#![allow(clippy::disallowed_types)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use aionui_api_types::ApiResponse;
use aionui_common::ApiError;

use crate::agent_scope::{AgentActionKind, redact_secrets};
use crate::middleware::{AgentPrincipal, CurrentUser};

/// Header the helper CLI sends when replaying an approved request.
pub const AGENT_APPROVAL_ID_HEADER: &str = "x-aionui-approval-id";
/// Error code returned when a write needs approval (details carry the id).
pub const AGENT_APPROVAL_REQUIRED_CODE: &str = "AGENT_ACTION_APPROVAL_REQUIRED";
/// How long a pending/approved request stays usable.
pub const APPROVAL_TTL: Duration = Duration::from_secs(10 * 60);
/// WebSocket event announcing a new or resolved approval.
pub const APPROVAL_EVENT: &str = "agent.actionApproval";

const MAX_PENDING_PER_USER: usize = 20;
const SUMMARY_MAX_CHARS: usize = 4_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Denied,
    Consumed,
    Expired,
}

/// User-visible description of a parked request.
#[derive(Debug, Clone, Serialize)]
pub struct AgentApprovalRecord {
    pub id: String,
    pub user_id: String,
    pub conversation_id: String,
    pub kind: AgentActionKind,
    pub method: String,
    pub path: String,
    /// Secret-redacted, truncated request body for the confirmation card.
    pub summary: String,
    /// `true` when `summary` was cut at `SUMMARY_MAX_CHARS` (4000) chars, i.e. the card
    /// does not show the whole request (M8 E9-5: the phone then only lets
    /// the user read it and sends them to the desktop to decide).
    pub summary_truncated: bool,
    pub status: ApprovalStatus,
    pub created_at: i64,
    pub expires_at: i64,
}

struct Entry {
    record: AgentApprovalRecord,
    fingerprint: [u8; 32],
}

/// Callback invoked whenever an approval is created or resolved.
pub type ApprovalNotifier = Arc<dyn Fn(&AgentApprovalRecord) + Send + Sync>;

/// Result of presenting an approval id with a replayed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalCheck {
    /// Approved and now consumed; let the request through.
    Granted,
    Pending,
    Denied,
    /// Unknown id, expired, already used, or bound to a different request.
    Invalid,
}

/// In-memory approval store.
#[derive(Default)]
pub struct AgentActionApprovals {
    entries: Mutex<HashMap<String, Entry>>,
    notifier: OnceLock<ApprovalNotifier>,
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn random_id() -> String {
    let mut buf = [0u8; 16];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("apr_{hex}")
}

/// Fingerprint binding an approval to one exact request.
pub fn request_fingerprint(method: &str, path_and_query: &str, body: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(method.as_bytes());
    hasher.update([0]);
    hasher.update(path_and_query.as_bytes());
    hasher.update([0]);
    hasher.update(body);
    hasher.finalize().into()
}

/// Secret-redacted, pretty-printed body and whether it had to be truncated.
fn summarize_body(body: &[u8]) -> (String, bool) {
    if body.is_empty() {
        return (String::new(), false);
    }
    let text = match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(mut value) => {
            redact_secrets(&mut value);
            serde_json::to_string_pretty(&value).unwrap_or_default()
        }
        Err(_) => "[non-JSON body]".to_owned(),
    };
    if text.chars().count() > SUMMARY_MAX_CHARS {
        let mut cut: String = text.chars().take(SUMMARY_MAX_CHARS).collect();
        cut.push_str("\n…");
        (cut, true)
    } else {
        (text, false)
    }
}

impl AgentActionApprovals {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install the notifier (once; later calls are ignored).
    pub fn set_notifier(&self, notifier: ApprovalNotifier) {
        let _ = self.notifier.set(notifier);
    }

    fn notify(&self, record: &AgentApprovalRecord) {
        if let Some(notifier) = self.notifier.get() {
            notifier(record);
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Entry>> {
        self.entries.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn prune(entries: &mut HashMap<String, Entry>, now: i64) {
        entries.retain(|_, entry| entry.record.expires_at > now && entry.record.status != ApprovalStatus::Consumed);
    }

    /// Park a request and notify the user. Returns `None` when the user
    /// already has too many pending requests (flood guard).
    pub fn request(
        &self,
        user_id: &str,
        conversation_id: &str,
        kind: AgentActionKind,
        method: &str,
        path_and_query: &str,
        body: &[u8],
    ) -> Option<AgentApprovalRecord> {
        let now = now_ms();
        let record = {
            let mut entries = self.lock();
            Self::prune(&mut entries, now);
            let pending = entries
                .values()
                .filter(|e| e.record.user_id == user_id && e.record.status == ApprovalStatus::Pending)
                .count();
            if pending >= MAX_PENDING_PER_USER {
                return None;
            }
            let ttl_ms = i64::try_from(APPROVAL_TTL.as_millis()).unwrap_or(i64::MAX);
            let (summary, summary_truncated) = summarize_body(body);
            let record = AgentApprovalRecord {
                id: random_id(),
                user_id: user_id.to_owned(),
                conversation_id: conversation_id.to_owned(),
                kind,
                method: method.to_owned(),
                path: path_and_query.to_owned(),
                summary,
                summary_truncated,
                status: ApprovalStatus::Pending,
                created_at: now,
                expires_at: now.saturating_add(ttl_ms),
            };
            entries.insert(
                record.id.clone(),
                Entry {
                    record: record.clone(),
                    fingerprint: request_fingerprint(method, path_and_query, body),
                },
            );
            record
        };
        self.notify(&record);
        Some(record)
    }

    /// Check (and on success consume) an approval presented with a replay.
    pub fn check_and_consume(
        &self,
        id: &str,
        user_id: &str,
        conversation_id: &str,
        fingerprint: &[u8; 32],
    ) -> ApprovalCheck {
        let now = now_ms();
        let mut entries = self.lock();
        let Some(entry) = entries.get_mut(id) else {
            return ApprovalCheck::Invalid;
        };
        if entry.record.user_id != user_id
            || entry.record.conversation_id != conversation_id
            || &entry.fingerprint != fingerprint
            || entry.record.expires_at <= now
        {
            return ApprovalCheck::Invalid;
        }
        match entry.record.status {
            ApprovalStatus::Approved => {
                entry.record.status = ApprovalStatus::Consumed;
                ApprovalCheck::Granted
            }
            ApprovalStatus::Pending => ApprovalCheck::Pending,
            ApprovalStatus::Denied => ApprovalCheck::Denied,
            ApprovalStatus::Consumed | ApprovalStatus::Expired => ApprovalCheck::Invalid,
        }
    }

    /// Read one approval visible to `user_id`.
    pub fn get(&self, id: &str, user_id: &str) -> Option<AgentApprovalRecord> {
        let now = now_ms();
        let entries = self.lock();
        let entry = entries.get(id).filter(|entry| entry.record.user_id == user_id)?;
        let mut record = entry.record.clone();
        if record.expires_at <= now && record.status == ApprovalStatus::Pending {
            record.status = ApprovalStatus::Expired;
        }
        Some(record)
    }

    /// Pending approvals for `user_id`, oldest first.
    pub fn list_pending(&self, user_id: &str) -> Vec<AgentApprovalRecord> {
        let now = now_ms();
        let mut entries = self.lock();
        Self::prune(&mut entries, now);
        let mut pending: Vec<_> = entries
            .values()
            .filter(|e| e.record.user_id == user_id && e.record.status == ApprovalStatus::Pending)
            .map(|e| e.record.clone())
            .collect();
        pending.sort_by_key(|record| record.created_at);
        pending
    }

    /// Resolve a pending approval. Returns the updated record.
    pub fn resolve(&self, id: &str, user_id: &str, approve: bool) -> Option<AgentApprovalRecord> {
        let now = now_ms();
        let record = {
            let mut entries = self.lock();
            let entry = entries.get_mut(id).filter(|entry| entry.record.user_id == user_id)?;
            if entry.record.status != ApprovalStatus::Pending || entry.record.expires_at <= now {
                return None;
            }
            entry.record.status = if approve {
                ApprovalStatus::Approved
            } else {
                ApprovalStatus::Denied
            };
            entry.record.clone()
        };
        self.notify(&record);
        Some(record)
    }
}

fn reject_agent_principal(principal: Option<&Extension<AgentPrincipal>>) -> Result<(), ApiError> {
    if principal.is_some() {
        return Err(ApiError::coded(
            StatusCode::FORBIDDEN,
            "AGENT_SCOPE_DENIED",
            "Agents cannot resolve their own approval requests.",
            None,
        ));
    }
    Ok(())
}

async fn list_handler(
    State(store): State<Arc<AgentActionApprovals>>,
    Extension(user): Extension<CurrentUser>,
    principal: Option<Extension<AgentPrincipal>>,
) -> Result<Json<ApiResponse<Vec<AgentApprovalRecord>>>, ApiError> {
    let mut pending = store.list_pending(&user.id);
    if let Some(Extension(principal)) = principal {
        pending.retain(|record| record.conversation_id == principal.conversation_id);
    }
    Ok(Json(ApiResponse::ok(pending)))
}

async fn get_handler(
    State(store): State<Arc<AgentActionApprovals>>,
    Extension(user): Extension<CurrentUser>,
    principal: Option<Extension<AgentPrincipal>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentApprovalRecord>>, ApiError> {
    let record = store
        .get(&id, &user.id)
        .filter(|record| {
            principal
                .as_ref()
                .is_none_or(|Extension(p)| p.conversation_id == record.conversation_id)
        })
        .ok_or_else(|| ApiError::NotFound("Approval not found".into()))?;
    Ok(Json(ApiResponse::ok(record)))
}

async fn resolve_handler(
    store: Arc<AgentActionApprovals>,
    user: CurrentUser,
    principal: Option<Extension<AgentPrincipal>>,
    id: String,
    approve: bool,
) -> Result<Json<ApiResponse<AgentApprovalRecord>>, ApiError> {
    reject_agent_principal(principal.as_ref())?;
    let record = store.resolve(&id, &user.id, approve).ok_or_else(|| {
        ApiError::coded(
            StatusCode::CONFLICT,
            "APPROVAL_NOT_PENDING",
            "Approval is not pending.",
            None,
        )
    })?;
    Ok(Json(ApiResponse::ok(record)))
}

async fn approve_handler(
    State(store): State<Arc<AgentActionApprovals>>,
    Extension(user): Extension<CurrentUser>,
    principal: Option<Extension<AgentPrincipal>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentApprovalRecord>>, ApiError> {
    resolve_handler(store, user, principal, id, true).await
}

async fn deny_handler(
    State(store): State<Arc<AgentActionApprovals>>,
    Extension(user): Extension<CurrentUser>,
    principal: Option<Extension<AgentPrincipal>>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<AgentApprovalRecord>>, ApiError> {
    resolve_handler(store, user, principal, id, false).await
}

/// `/api/agent-approvals*` routes. Mount behind `auth_middleware`.
///
/// - `GET  /api/agent-approvals` — pending approvals (agents see only their
///   own conversation's)
/// - `GET  /api/agent-approvals/{id}` — status (the CLI polls this)
/// - `POST /api/agent-approvals/{id}/approve` | `/deny` — user only
pub fn agent_approval_routes(store: Arc<AgentActionApprovals>) -> Router {
    Router::new()
        .route("/api/agent-approvals", get(list_handler))
        .route("/api/agent-approvals/{id}", get(get_handler))
        .route("/api/agent-approvals/{id}/approve", post(approve_handler))
        .route("/api/agent-approvals/{id}/deny", post(deny_handler))
        .with_state(store)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn store() -> AgentActionApprovals {
        AgentActionApprovals::new()
    }

    #[test]
    fn approval_is_single_use_and_bound_to_the_request() {
        let store = store();
        let body = br#"{"name":"github","command":"npx"}"#;
        let record = store
            .request("u1", "c1", AgentActionKind::Mcp, "POST", "/api/mcp/servers", body)
            .unwrap();
        let fp = request_fingerprint("POST", "/api/mcp/servers", body);

        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c1", &fp),
            ApprovalCheck::Pending
        );
        assert!(store.resolve(&record.id, "u1", true).is_some());

        let other = request_fingerprint("POST", "/api/mcp/servers", br#"{"name":"evil"}"#);
        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c1", &other),
            ApprovalCheck::Invalid
        );
        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c2", &fp),
            ApprovalCheck::Invalid
        );
        assert_eq!(
            store.check_and_consume(&record.id, "u2", "c1", &fp),
            ApprovalCheck::Invalid
        );

        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c1", &fp),
            ApprovalCheck::Granted
        );
        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c1", &fp),
            ApprovalCheck::Invalid
        );
    }

    #[test]
    fn denied_requests_stay_denied_and_cannot_be_reapproved() {
        let store = store();
        let record = store
            .request("u1", "c1", AgentActionKind::Cron, "POST", "/api/cron/jobs", b"{}")
            .unwrap();
        assert!(store.resolve(&record.id, "u1", false).is_some());
        assert!(store.resolve(&record.id, "u1", true).is_none());
        let fp = request_fingerprint("POST", "/api/cron/jobs", b"{}");
        assert_eq!(
            store.check_and_consume(&record.id, "u1", "c1", &fp),
            ApprovalCheck::Denied
        );
    }

    #[test]
    fn other_users_cannot_see_or_resolve() {
        let store = store();
        let record = store
            .request("u1", "c1", AgentActionKind::Settings, "PATCH", "/api/settings", b"{}")
            .unwrap();
        assert!(store.get(&record.id, "u2").is_none());
        assert!(store.resolve(&record.id, "u2", true).is_none());
        assert!(store.list_pending("u2").is_empty());
        assert_eq!(store.list_pending("u1").len(), 1);
    }

    #[test]
    fn summary_is_redacted_and_notifier_fires() {
        let store = store();
        let fired = Arc::new(AtomicUsize::new(0));
        let counter = fired.clone();
        store.set_notifier(Arc::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
        }));
        let record = store
            .request(
                "u1",
                "c1",
                AgentActionKind::Provider,
                "POST",
                "/api/providers",
                br#"{"base_url":"https://evil","api_key":"sk-secret"}"#,
            )
            .unwrap();
        assert!(record.summary.contains("https://evil"));
        assert!(!record.summary.contains("sk-secret"));
        assert!(!record.summary_truncated);
        store.resolve(&record.id, "u1", true);
        assert_eq!(fired.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn long_summary_is_truncated_and_flagged() {
        let store = store();
        let long = format!(r#"{{"command":"{}"}}"#, "x".repeat(SUMMARY_MAX_CHARS + 10));
        let record = store
            .request(
                "u1",
                "c1",
                AgentActionKind::Mcp,
                "POST",
                "/api/mcp/servers",
                long.as_bytes(),
            )
            .unwrap();
        assert!(record.summary_truncated);
        assert!(record.summary.ends_with('…'));
        assert_eq!(record.summary.chars().count(), SUMMARY_MAX_CHARS + 2);
        let json = serde_json::to_value(&record).unwrap();
        assert_eq!(json["summary_truncated"], serde_json::Value::Bool(true));

        let empty = store
            .request("u1", "c1", AgentActionKind::Mcp, "POST", "/api/mcp/servers", b"")
            .unwrap();
        assert!(!empty.summary_truncated);
        assert_eq!(serde_json::to_value(&empty).unwrap()["summary_truncated"], false);
    }

    #[test]
    fn pending_requests_are_capped_per_user() {
        let store = store();
        for _ in 0..MAX_PENDING_PER_USER {
            assert!(
                store
                    .request("u1", "c1", AgentActionKind::Mcp, "POST", "/api/mcp/servers", b"{}")
                    .is_some()
            );
        }
        assert!(
            store
                .request("u1", "c1", AgentActionKind::Mcp, "POST", "/api/mcp/servers", b"{}")
                .is_none()
        );
        assert!(
            store
                .request("u2", "c9", AgentActionKind::Mcp, "POST", "/api/mcp/servers", b"{}")
                .is_some()
        );
    }
}
