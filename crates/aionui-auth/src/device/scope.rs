//! `mobile_default` scope: what a paired-device token may call (M8 E9-1).
//!
//! Table-driven like `agent_scope`: everything not listed is denied.
//!
//! Allowed:
//! - read conversations / messages / artifacts (`GET /api/conversations/**`,
//!   `GET /api/messages/search`)
//! - follow up in an **existing** conversation
//!   (`POST /api/conversations/{id}/messages`); creating conversations or
//!   work items is denied (D9-4)
//! - answer approval cards: ACP tool confirmations, ask-user answers,
//!   Plan/Goal task approvals, agent config-write approvals (D9-5)
//! - read / mark Inbox items (list and `GET /api/inbox/{id}`)
//! - read work items and their approvals (`GET /api/task-sessions/**`)
//! - `GET /api/auth/user`
//!
//! Denied: everything else — provider / MCP / settings writes, device
//! management (`/api/auth/devices*`), config export, cron / batch creation,
//! raw filesystem (`/api/fs/*`), session-token minting (`/api/ws-token`).
use axum::http::Method;

use crate::agent_scope::{has_prefix, normalized_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceRouteDecision {
    Allow,
    Deny,
}

/// GET/HEAD prefixes readable with a device token.
const READ_PREFIXES: &[&str] = &[
    "/api/conversations",
    "/api/messages/search",
    "/api/inbox",
    "/api/task-sessions",
    "/api/agent-approvals",
    "/api/auth/user",
];

/// Conversation subpaths a device may POST to: `/api/conversations/{id}/<pattern>`
/// where `*` matches exactly one non-empty segment.
const CONVERSATION_POST_PATTERNS: &[&[&str]] = &[
    &["messages"],
    &["confirmations", "*", "confirm"],
    &["asks", "*", "answer"],
];

fn segment_ok(segment: &str) -> bool {
    !segment.is_empty() && segment != "." && segment != ".."
}

fn matches_pattern(segments: &[&str], pattern: &[&str]) -> bool {
    segments.len() == pattern.len()
        && segments
            .iter()
            .zip(pattern)
            .all(|(segment, expected)| segment_ok(segment) && (*expected == "*" || segment == expected))
}

fn classify_post(path: &str) -> DeviceRouteDecision {
    let allowed = if let Some(rest) = path.strip_prefix("/api/conversations/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match segments.split_first() {
            Some((id, sub)) if segment_ok(id) => CONVERSATION_POST_PATTERNS
                .iter()
                .any(|pattern| matches_pattern(sub, pattern)),
            _ => false,
        }
    } else if let Some(rest) = path.strip_prefix("/api/task-sessions/") {
        let segments: Vec<&str> = rest.split('/').collect();
        matches_pattern(&segments, &["*", "approvals", "*", "decision"])
    } else if let Some(rest) = path.strip_prefix("/api/agent-approvals/") {
        let segments: Vec<&str> = rest.split('/').collect();
        matches_pattern(&segments, &["*", "approve"]) || matches_pattern(&segments, &["*", "deny"])
    } else if path == "/api/inbox/read-all" {
        true
    } else if let Some(rest) = path.strip_prefix("/api/inbox/") {
        let segments: Vec<&str> = rest.split('/').collect();
        matches_pattern(&segments, &["*", "read"])
    } else {
        false
    };
    if allowed {
        DeviceRouteDecision::Allow
    } else {
        DeviceRouteDecision::Deny
    }
}

/// Classify a request made with a `mobile_default` device token.
pub fn classify_device_request(method: &Method, path: &str) -> DeviceRouteDecision {
    let Some(path) = normalized_path(path) else {
        return DeviceRouteDecision::Deny;
    };
    if method == Method::GET || method == Method::HEAD {
        if READ_PREFIXES.iter().any(|prefix| has_prefix(path, prefix)) {
            return DeviceRouteDecision::Allow;
        }
        return DeviceRouteDecision::Deny;
    }
    if method == Method::POST {
        return classify_post(path);
    }
    DeviceRouteDecision::Deny
}

#[cfg(test)]
mod tests {
    use super::*;
    use DeviceRouteDecision::{Allow, Deny};

    fn get(path: &str) -> DeviceRouteDecision {
        classify_device_request(&Method::GET, path)
    }
    fn post(path: &str) -> DeviceRouteDecision {
        classify_device_request(&Method::POST, path)
    }

    #[test]
    fn reads_are_limited_to_the_mobile_surfaces() {
        for path in [
            "/api/conversations",
            "/api/conversations?limit=20",
            "/api/conversations/c1",
            "/api/conversations/c1/messages",
            "/api/conversations/c1/confirmations",
            "/api/conversations/c1/artifacts",
            "/api/conversations/c1/outputs/report.md",
            "/api/messages/search?q=x",
            "/api/inbox",
            "/api/inbox?unread_only=true",
            "/api/inbox/i1",
            "/api/task-sessions/t1/approvals",
            "/api/agent-approvals",
            "/api/agent-approvals/a1",
            "/api/auth/user",
        ] {
            assert_eq!(get(path), Allow, "{path}");
        }
        for path in [
            "/api/providers",
            "/api/mcp/servers",
            "/api/settings/client",
            "/api/auth/devices",
            "/api/auth/devices/pair/offers/o1",
            "/api/ws-token",
            "/api/fs/stream",
            "/api/cron/jobs",
            "/api/batch-jobs",
            "/api/config/export",
            "/api/conversationsX",
            "/api/conversations/../providers",
            "/api/conversations/%2e%2e/providers",
            "/api/conversations//c1",
        ] {
            assert_eq!(get(path), Deny, "{path}");
        }
    }

    #[test]
    fn writes_are_follow_ups_and_approvals_only() {
        for path in [
            "/api/conversations/c1/messages",
            "/api/conversations/c1/confirmations/call-1/confirm",
            "/api/conversations/c1/asks/r1/answer",
            "/api/task-sessions/t1/approvals/a1/decision",
            "/api/agent-approvals/a1/approve",
            "/api/agent-approvals/a1/deny",
            "/api/inbox/read-all",
            "/api/inbox/i1/read",
            "/api/inbox/i1/read/",
        ] {
            assert_eq!(post(path), Allow, "{path}");
        }
        for path in [
            // D9-4: no new conversations or work items.
            "/api/conversations",
            "/api/conversations/clone",
            "/api/conversations/c1/fork",
            "/api/task-sessions",
            "/api/task-sessions/t1/execute",
            "/api/conversations/c1/reset",
            "/api/conversations/c1/runtime/restart",
            "/api/conversations/c1/messages/m1",
            "/api/conversations/c1/confirmations//confirm",
            "/api/conversations/../messages",
            "/api/task-sessions/t1/approvals/a1/decision/x",
            "/api/agent-approvals/a1",
            "/api/providers",
            "/api/mcp/servers",
            "/api/auth/devices/pair/offers",
            "/api/auth/change-password",
            "/logout",
            "/api/cron/jobs",
            "/api/batch-jobs",
            "/api/fs/read",
            "/api/inbox/i1/x",
        ] {
            assert_eq!(post(path), Deny, "{path}");
        }
    }

    #[test]
    fn audit_routes_are_denied() {
        // M8 E12-2: the audit ledger is desktop-user only.
        for method in [Method::GET, Method::POST, Method::DELETE] {
            for path in ["/api/audit/events", "/api/audit/verify", "/api/audit/export.csv"] {
                assert_eq!(classify_device_request(&method, path), Deny, "{method} {path}");
            }
        }
    }

    #[test]
    fn other_methods_are_denied() {
        for method in [Method::PUT, Method::PATCH, Method::DELETE] {
            assert_eq!(classify_device_request(&method, "/api/conversations/c1"), Deny);
            assert_eq!(classify_device_request(&method, "/api/inbox/i1/read"), Deny);
        }
    }
}
