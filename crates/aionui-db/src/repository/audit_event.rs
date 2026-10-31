use serde_json::Value;

use crate::error::DbError;
use crate::models::AuditEventRow;

/// Who performed an audited action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditActorKind {
    User,
    Agent,
    Device,
    System,
}

impl AuditActorKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Agent => "agent",
            Self::Device => "device",
            Self::System => "system",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "user" => Some(Self::User),
            "agent" => Some(Self::Agent),
            "device" => Some(Self::Device),
            "system" => Some(Self::System),
            _ => None,
        }
    }
}

/// Which surface the action came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditVia {
    Desktop,
    WebUi,
    Mobile,
    Helper,
    Cron,
    Batch,
    System,
}

impl AuditVia {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::WebUi => "webui",
            Self::Mobile => "mobile",
            Self::Helper => "helper",
            Self::Cron => "cron",
            Self::Batch => "batch",
            Self::System => "system",
        }
    }
}

/// An event to append. Built with [`NewAuditEvent::new`] and the `with_*`
/// setters; the ledger mints `id`, `ts` (when unset), `seq` and the hashes.
///
/// `diff` may contain secrets: the ledger redacts it before insert
/// (see [`crate::audit::redact_diff`]), callers do not have to.
#[derive(Debug, Clone, PartialEq)]
pub struct NewAuditEvent {
    pub ts: Option<i64>,
    pub user_id: Option<String>,
    pub actor_kind: AuditActorKind,
    pub actor_id: Option<String>,
    pub via: AuditVia,
    pub action: String,
    pub target_kind: Option<String>,
    pub target_id: Option<String>,
    pub summary: String,
    pub diff: Option<Value>,
    pub approver_id: Option<String>,
    pub approval_id: Option<String>,
    pub request_fingerprint: Option<String>,
}

impl NewAuditEvent {
    pub fn new(action: impl Into<String>, actor_kind: AuditActorKind, via: AuditVia) -> Self {
        Self {
            ts: None,
            user_id: None,
            actor_kind,
            actor_id: None,
            via,
            action: action.into(),
            target_kind: None,
            target_id: None,
            summary: String::new(),
            diff: None,
            approver_id: None,
            approval_id: None,
            request_fingerprint: None,
        }
    }

    pub fn with_ts(mut self, ts: i64) -> Self {
        self.ts = Some(ts);
        self
    }

    pub fn with_user(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn with_actor(mut self, actor_id: impl Into<String>) -> Self {
        self.actor_id = Some(actor_id.into());
        self
    }

    pub fn with_target(mut self, kind: impl Into<String>, id: impl Into<String>) -> Self {
        self.target_kind = Some(kind.into());
        self.target_id = Some(id.into());
        self
    }

    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    pub fn with_diff(mut self, diff: Value) -> Self {
        self.diff = Some(diff);
        self
    }

    pub fn with_approval(mut self, approval_id: impl Into<String>, approver_id: Option<String>) -> Self {
        self.approval_id = Some(approval_id.into());
        self.approver_id = approver_id;
        self
    }

    pub fn with_request_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.request_fingerprint = Some(fingerprint.into());
        self
    }
}

/// Sort order for [`IAuditEventRepository::list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AuditOrder {
    /// Newest first (`seq` descending); page with `before_seq`.
    #[default]
    NewestFirst,
    /// Chain order (`seq` ascending); page with `after_seq`.
    OldestFirst,
}

/// Filter for listing events. Every field is optional; `limit` is clamped by
/// the repository to 1..=[`AUDIT_LIST_MAX_LIMIT`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AuditEventFilter {
    pub user_id: Option<String>,
    pub from_ts: Option<i64>,
    pub to_ts: Option<i64>,
    pub action: Option<String>,
    /// Matches `action` starting with this text (e.g. `config.`).
    pub action_prefix: Option<String>,
    pub actor_id: Option<String>,
    pub actor_kind: Option<String>,
    pub before_seq: Option<i64>,
    pub after_seq: Option<i64>,
    pub order: AuditOrder,
    pub limit: Option<i64>,
}

pub const AUDIT_LIST_DEFAULT_LIMIT: i64 = 100;
pub const AUDIT_LIST_MAX_LIMIT: i64 = 1000;

/// Result of re-hashing the whole chain from the first row.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AuditChainVerification {
    pub ok: bool,
    /// Rows checked (all rows when `ok`).
    pub checked: i64,
    /// First row whose `prev_hash` or `hash` does not match.
    pub first_broken_seq: Option<i64>,
    /// Hash of the last row (the chain head), when the table is not empty.
    pub head_hash: Option<String>,
}

#[async_trait::async_trait]
pub trait IAuditEventRepository: Send + Sync {
    /// Redact, hash-chain and insert one event.
    async fn append(&self, event: &NewAuditEvent) -> Result<AuditEventRow, DbError>;

    async fn list(&self, filter: &AuditEventFilter) -> Result<Vec<AuditEventRow>, DbError>;

    /// Re-hash every row in chain order.
    async fn verify_chain(&self) -> Result<AuditChainVerification, DbError>;
}
