//! 3.0 C9 (plan B §5.4, D8): remote approve-to-execute.
//!
//! A paired phone or a bound IM identity answers one exact request: the
//! approver echoes its id and digest, and may only approve or reject. Plan
//! content, agent, permission mode, adopt strategy and target are fixed when
//! the request is issued, so the decision body carries nothing else.

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Default lifetime of a remote approval request.
pub const DEFAULT_APPROVAL_TTL_MINUTES: i64 = 30;
/// Longest lifetime the setting accepts (24 hours).
pub const MAX_APPROVAL_TTL_MINUTES: i64 = 24 * 60;
/// Shortest digest prefix an IM card may echo (96 bits). Phones send the
/// full 64-hex digest.
pub const MIN_IM_DIGEST_PREFIX: usize = 24;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequestKind {
    Plan,
    Goal,
    Adopt,
}

impl ApprovalRequestKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Goal => "goal",
            Self::Adopt => "adopt",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "plan" => Self::Plan,
            "goal" => Self::Goal,
            "adopt" => Self::Adopt,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalRequestStatus {
    Pending,
    Approved,
    Rejected,
    Expired,
    Invalidated,
}

impl ApprovalRequestStatus {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "pending" => Self::Pending,
            "approved" => Self::Approved,
            "rejected" => Self::Rejected,
            "expired" => Self::Expired,
            "invalidated" => Self::Invalidated,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ApprovalRequestResponse {
    pub id: String,
    pub task_session_id: String,
    pub kind: ApprovalRequestKind,
    pub status: ApprovalRequestStatus,
    /// What the approver must echo back.
    pub digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artifact_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fanout_group_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strategy: Option<String>,
    pub requested_at: TimestampMs,
    pub expires_at: TimestampMs,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_at: Option<TimestampMs>,
    /// `desktop`, `device:<id>` or `im:<platform>:<identity>`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<String>,
    /// Why it stopped being answerable: superseded, cancelled, expired,
    /// diff_changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The run id or adopt commit the approval produced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RemoteApprovalDecision {
    Approve,
    Reject,
}

/// `POST /api/approval-requests/{id}/decision` from a paired device. Unknown
/// fields are refused: a remote approver cannot change the plan, the agent,
/// the permission mode or the adopt strategy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteApprovalDecisionRequest {
    pub decision: RemoteApprovalDecision,
    pub digest: String,
    #[serde(default)]
    pub comment: Option<String>,
}

/// Desktop: issue (or re-issue after expiry) a remote approval request. For
/// an adopt the strategy and target are fixed here (default: squash into the
/// checked-out base branch); a Plan/Goal request binds the pending approval.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IssueApprovalRequest {
    pub kind: ApprovalRequestKind,
    #[serde(default)]
    pub strategy: Option<String>,
    #[serde(default)]
    pub target_branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteApprovalSettings {
    pub ttl_minutes: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RemoteApproverRequest {
    pub platform_type: String,
    pub platform_user_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteApproverResponse {
    pub platform_type: String,
    pub platform_user_id: String,
    pub created_at: TimestampMs,
}
