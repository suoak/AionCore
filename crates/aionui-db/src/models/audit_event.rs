use aionui_common::TimestampMs;
use serde::Serialize;

/// One row of the append-only audit ledger (M8 E12-2, migration 079).
///
/// `diff` is the redacted JSON text written by the ledger: secret values were
/// replaced by a fingerprint before insert, so the row is safe to serialize.
#[derive(Debug, Clone, sqlx::FromRow, PartialEq, Serialize)]
pub struct AuditEventRow {
    /// Position in the hash chain (1-based, gap-free unless a write failed).
    pub seq: i64,
    pub id: String,
    pub ts: TimestampMs,
    pub user_id: Option<String>,
    /// `user` | `agent` | `device` | `system`.
    pub actor_kind: String,
    /// User id, conversation id (agent), device id, or a system component.
    pub actor_id: Option<String>,
    /// `desktop` | `webui` | `mobile` | `helper` | `cron` | `batch` | `system`.
    pub via: String,
    /// Dotted action name, e.g. `config.provider.update`.
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub summary: String,
    pub diff: Option<String>,
    pub approver_id: Option<String>,
    pub approval_id: Option<String>,
    pub request_fingerprint: Option<String>,
    pub prev_hash: String,
    pub hash: String,
}
