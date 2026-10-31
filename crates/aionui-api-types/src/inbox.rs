use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// One inbox entry (M7 E5-4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxItemDto {
    pub id: String,
    /// `approval` | `blocked` | `completed` | `failed` | `question` | `mention`.
    pub kind: String,
    pub source_kind: String,
    pub source_id: String,
    pub conversation_id: Option<String>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    pub title: String,
    pub summary: Option<String>,
    pub created_at: TimestampMs,
    pub read_at: Option<TimestampMs>,
    pub resolved_at: Option<TimestampMs>,
    /// Producer the item came from when it is not a conversation / task run:
    /// [`INBOX_ORIGIN_BATCH`] (end-of-job batch summary) or
    /// [`INBOX_ORIGIN_AUTOPILOT`] (Autopilot run failure). Derived, not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    /// The Plan/Goal task approval this item asks about (dedupe key
    /// `task_approval:{id}`), so a client can match it to
    /// `task.approval.changed` (M8 E9-3). Derived, not stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_id: Option<String>,
}

/// Dedupe-key prefix of a work item's Plan/Goal approval Inbox item.
pub const TASK_APPROVAL_INBOX_KEY_PREFIX: &str = "task_approval:";

/// The task approval id behind an Inbox item's dedupe key, if any.
pub fn inbox_task_approval_id(dedupe_key: &str) -> Option<&str> {
    dedupe_key
        .strip_prefix(TASK_APPROVAL_INBOX_KEY_PREFIX)
        .filter(|id| !id.is_empty())
}

/// `origin` of a batch job's end-of-job summary (dedupe key `batch:{id}`).
pub const INBOX_ORIGIN_BATCH: &str = "batch";
/// `origin` of an Autopilot run failure.
pub const INBOX_ORIGIN_AUTOPILOT: &str = "autopilot";
/// Title prefix the Autopilot notifier gives its Inbox items.
pub const AUTOPILOT_INBOX_TITLE_PREFIX: &str = "Autopilot: ";

/// Derives [`InboxItemDto::origin`] from the stored dedupe key and title.
/// An Autopilot failure tied to a conversation shares the turn's key, so it
/// is recognised by the notifier's title prefix.
pub fn inbox_origin(dedupe_key: &str, kind: &str, title: &str) -> Option<&'static str> {
    if dedupe_key.starts_with("batch:") {
        Some(INBOX_ORIGIN_BATCH)
    } else if dedupe_key.starts_with("cron:") || (kind == "failed" && title.starts_with(AUTOPILOT_INBOX_TITLE_PREFIX)) {
        Some(INBOX_ORIGIN_AUTOPILOT)
    } else {
        None
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct InboxListQuery {
    #[serde(default)]
    pub unread_only: Option<bool>,
    /// Cursor: only items created strictly before this timestamp.
    #[serde(default)]
    pub before: Option<TimestampMs>,
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxListResponse {
    pub items: Vec<InboxItemDto>,
    pub unread_count: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboxMarkReadResponse {
    pub updated: u64,
    pub unread_count: i64,
}

#[cfg(test)]
mod origin_tests {
    use super::*;

    #[test]
    fn origin_is_derived_from_key_and_title() {
        assert_eq!(inbox_origin("batch:bj_1", "completed", "x"), Some(INBOX_ORIGIN_BATCH));
        assert_eq!(inbox_origin("cron:job_1", "failed", "x"), Some(INBOX_ORIGIN_AUTOPILOT));
        assert_eq!(
            inbox_origin("turn:c1", "failed", "Autopilot: Nightly"),
            Some(INBOX_ORIGIN_AUTOPILOT)
        );
        assert_eq!(inbox_origin("turn:c1", "completed", "Autopilot: Nightly"), None);
        assert_eq!(inbox_origin("turn:c1", "failed", "Fix build"), None);
        assert_eq!(inbox_origin("approval:c1:x", "approval", "Autopilot: x"), None);
    }

    #[test]
    fn task_approval_id_comes_from_the_dedupe_key() {
        assert_eq!(inbox_task_approval_id("task_approval:ap_1"), Some("ap_1"));
        assert_eq!(inbox_task_approval_id("task_approval:"), None);
        assert_eq!(inbox_task_approval_id("approval:c1:x"), None);
        assert_eq!(inbox_task_approval_id("batch:bj_1"), None);
    }

    #[test]
    fn origin_is_omitted_when_absent() {
        let dto = InboxItemDto {
            id: "i".into(),
            kind: "failed".into(),
            source_kind: "conversation".into(),
            source_id: "c".into(),
            conversation_id: None,
            task_session_id: None,
            run_id: None,
            title: "t".into(),
            summary: None,
            created_at: 1,
            read_at: None,
            resolved_at: None,
            origin: None,
            approval_id: None,
        };
        let v = serde_json::to_value(&dto).unwrap();
        assert!(v.get("origin").is_none());
        assert!(v.get("approval_id").is_none());
        let back: InboxItemDto = serde_json::from_value(v).unwrap();
        assert_eq!(back.origin, None);
    }
}
