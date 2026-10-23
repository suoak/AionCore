//! Route scope for agent-issued (conversation-runtime) helper tokens.
//!
//! A runtime token authenticates the helper CLIs (`aioncore config` /
//! `diagnose`) that an agent runs inside its conversation. Historically a valid
//! token became the full user identity on every route, so a prompt-injected
//! agent could add stdio MCP servers, rewrite provider endpoints, change
//! settings or schedule cron jobs. This module narrows the token to:
//!
//! - read-only access to an explicit list of configuration/diagnostic routes
//!   (responses are secret-redacted, see [`redact_secrets`]);
//! - renaming its own conversation;
//! - configuration writes only after the user approves each one in the app
//!   (see [`crate::agent_approval`]);
//! - nothing else.

use axum::http::Method;
use serde_json::Value;

/// What the auth middleware should do with an agent-token request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRouteDecision {
    /// Allowed as-is (read-only routes).
    Allow,
    /// `GET /api/conversations` — allowed, but the response is filtered to the
    /// conversations this token may read (its own plus `@@`-granted ones).
    AllowConversationList,
    /// `GET /api/conversations/{id}/...` — allowed only when `id` is the
    /// token's own conversation or one the user granted it with `@@`.
    AllowConversationRead(String),
    /// `PATCH /api/conversations/{own}` — allowed only for a rename-only body.
    AllowOwnRename,
    /// A configuration write that needs explicit user approval.
    RequireApproval(AgentActionKind),
    /// Outside the agent token's scope.
    Deny,
}

/// Category of a configuration write, shown to the user on the approval card.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentActionKind {
    Mcp,
    Provider,
    Settings,
    Agent,
    Assistant,
    Skill,
    Cron,
}

/// Route prefixes an agent token may read (GET/HEAD).
///
/// `/api/conversations` is NOT here: conversation reads are scoped per id (see
/// [`AgentRouteDecision::AllowConversationRead`]).
const READ_PREFIXES: &[&str] = &[
    "/api/assistants",
    "/api/skills",
    "/api/mcp/servers",
    "/api/mcp/agent-configs",
    "/api/mcp/oauth/authenticated",
    "/api/providers",
    "/api/settings",
    "/api/agents",
    "/api/cron",
    "/api/internal/conversation-cron/list",
    "/api/teams",
    "/api/agent-approvals",
];

/// Non-GET routes that only read or probe and never persist state.
const READ_LIKE_POSTS: &[&str] = &[
    "/api/mcp/oauth/check-status",
    "/api/providers/detect-protocol",
    "/api/providers/fetch-models",
    "/api/agents/provider-health-check",
    "/api/skills/info",
    "/api/skills/scan",
];

/// Write routes that need user approval, by category.
const APPROVAL_PREFIXES: &[(&str, AgentActionKind)] = &[
    ("/api/mcp", AgentActionKind::Mcp),
    ("/api/providers", AgentActionKind::Provider),
    ("/api/settings", AgentActionKind::Settings),
    ("/api/agents", AgentActionKind::Agent),
    ("/api/assistants", AgentActionKind::Assistant),
    ("/api/skills", AgentActionKind::Skill),
    ("/api/internal/conversation-cron", AgentActionKind::Cron),
    ("/api/cron", AgentActionKind::Cron),
];

/// `path` matches `prefix` exactly or as a `/`-delimited parent.
fn has_prefix(path: &str, prefix: &str) -> bool {
    path == prefix || path.strip_prefix(prefix).is_some_and(|rest| rest.starts_with('/'))
}

fn normalized_path(path: &str) -> Option<&str> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    // Refuse traversal-ish or encoded separators outright; axum routes on the
    // raw path, so a mismatch between our check and the router is a bypass.
    if path.contains("/../") || path.ends_with("/..") || path.contains("//") || path.contains('%') {
        return None;
    }
    Some(path.trim_end_matches('/'))
}

/// Classify an agent-token request.
pub fn classify_agent_request(method: &Method, path: &str, own_conversation_id: &str) -> AgentRouteDecision {
    let Some(path) = normalized_path(path) else {
        return AgentRouteDecision::Deny;
    };

    if method == Method::GET || method == Method::HEAD {
        if path == "/api/conversations" {
            return AgentRouteDecision::AllowConversationList;
        }
        if path == "/api/conversations/active-count" {
            // A bare count; reveals no conversation content.
            return AgentRouteDecision::Allow;
        }
        if let Some(rest) = path.strip_prefix("/api/conversations/") {
            let id = rest.split('/').next().unwrap_or("");
            return if id.is_empty() || id == "." || id == ".." {
                AgentRouteDecision::Deny
            } else {
                AgentRouteDecision::AllowConversationRead(id.to_owned())
            };
        }
        return if READ_PREFIXES.iter().any(|prefix| has_prefix(path, prefix)) {
            AgentRouteDecision::Allow
        } else {
            AgentRouteDecision::Deny
        };
    }

    if method == Method::POST && READ_LIKE_POSTS.contains(&path) {
        return AgentRouteDecision::Allow;
    }

    if method == Method::PATCH
        && let Some(id) = path.strip_prefix("/api/conversations/")
        && !id.is_empty()
        && !id.contains('/')
    {
        return if id == own_conversation_id {
            AgentRouteDecision::AllowOwnRename
        } else {
            AgentRouteDecision::Deny
        };
    }

    if matches!(method, &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE)
        && let Some((_, kind)) = APPROVAL_PREFIXES.iter().find(|(prefix, _)| has_prefix(path, prefix))
    {
        return AgentRouteDecision::RequireApproval(*kind);
    }

    AgentRouteDecision::Deny
}

/// Keep only the conversations an agent token may read in a
/// `GET /api/conversations` response (`{ data: { items: [...], total } }`).
///
/// Fail closed: an unexpected shape drops `data` entirely rather than passing
/// other conversations through.
pub fn filter_conversation_list(value: &mut Value, may_read: impl Fn(&str) -> bool) {
    let Some(data) = value.get_mut("data") else {
        return;
    };
    let Some(items) = data.get_mut("items").and_then(Value::as_array_mut) else {
        *data = Value::Null;
        return;
    };
    items.retain(|item| item.get("id").and_then(Value::as_str).is_some_and(&may_read));
    let kept = items.len();
    if let Some(map) = data.as_object_mut() {
        map.insert("total".to_owned(), Value::from(kept));
        map.insert("has_more".to_owned(), Value::Bool(false));
    }
}

/// Whether a JSON body is a rename-only conversation patch.
pub fn is_rename_only_patch(body: &[u8]) -> bool {
    let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(body) else {
        return false;
    };
    !map.is_empty() && map.keys().all(|key| key == "name")
}

const SECRET_KEY_MARKERS: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "secret",
    "password",
    "passwd",
    "token",
    "authorization",
    "cookie",
    "credential",
    "private_key",
    "access_key",
];

/// Whether an object key names secret material.
pub fn is_secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    SECRET_KEY_MARKERS.iter().any(|marker| key.contains(marker))
}

/// Keys whose whole object holds credentials by convention (MCP server `env`,
/// HTTP `headers`), so every string inside is redacted regardless of name.
const SECRET_CONTAINER_KEYS: &[&str] = &["env", "headers", "extra_headers", "custom_headers"];

fn redact_all_strings(value: &mut Value) {
    match value {
        Value::String(s) if !s.is_empty() => *s = "[REDACTED]".to_owned(),
        Value::Object(map) => map.values_mut().for_each(redact_all_strings),
        Value::Array(items) => items.iter_mut().for_each(redact_all_strings),
        _ => {}
    }
}

/// Replace string values under secret-looking keys with `"[REDACTED]"`.
///
/// Applied to every JSON response served to an agent token, so provider API
/// keys, MCP env/header credentials and similar never reach agent context.
/// Non-string values (e.g. `max_tokens: 4096`) are left untouched.
pub fn redact_secrets(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, entry) in map.iter_mut() {
                if is_secret_key(key) && matches!(entry, Value::String(s) if !s.is_empty()) {
                    *entry = Value::String("[REDACTED]".to_owned());
                } else if entry.is_object() && SECRET_CONTAINER_KEYS.iter().any(|name| key.eq_ignore_ascii_case(name)) {
                    redact_all_strings(entry);
                } else {
                    redact_secrets(entry);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact_secrets),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OWN: &str = "conv-own";

    fn classify(method: Method, path: &str) -> AgentRouteDecision {
        classify_agent_request(&method, path, OWN)
    }

    #[test]
    fn reads_are_limited_to_the_allowlist() {
        for path in [
            "/api/mcp/servers",
            "/api/providers",
            "/api/settings/client",
            "/api/conversations/active-count",
            "/api/agents/management",
            "/api/cron/jobs",
            "/api/agent-approvals/abc",
        ] {
            assert_eq!(classify(Method::GET, path), AgentRouteDecision::Allow, "{path}");
        }
        for path in [
            "/api/fs/stream?path=/etc/passwd",
            "/api/shell/exec",
            "/api/auth/user",
            "/api/system/info",
            "/api/mcp/serversX",
            "/api/conversations/../fs/stream",
            "/api/conversations/%2e%2e/fs",
        ] {
            assert_eq!(classify(Method::GET, path), AgentRouteDecision::Deny, "{path}");
        }
    }

    #[test]
    fn conversation_reads_are_scoped_per_id() {
        assert_eq!(
            classify(Method::GET, "/api/conversations?limit=50"),
            AgentRouteDecision::AllowConversationList
        );
        for (path, id) in [
            ("/api/conversations/conv-own", "conv-own"),
            ("/api/conversations/conv-own/messages?limit=10", "conv-own"),
            ("/api/conversations/other/messages", "other"),
            ("/api/conversations/other/transcript", "other"),
        ] {
            assert_eq!(
                classify(Method::GET, path),
                AgentRouteDecision::AllowConversationRead(id.to_owned()),
                "{path}"
            );
        }
    }

    #[test]
    fn conversation_list_is_filtered_and_fails_closed() {
        let mut value = json!({
            "success": true,
            "data": { "items": [{"id": "conv-own"}, {"id": "other"}, {"id": "granted"}], "total": 3, "has_more": true }
        });
        filter_conversation_list(&mut value, |id| id == "conv-own" || id == "granted");
        assert_eq!(value["data"]["items"], json!([{"id": "conv-own"}, {"id": "granted"}]));
        assert_eq!(value["data"]["total"], 2);
        assert_eq!(value["data"]["has_more"], false);

        let mut odd = json!({ "success": true, "data": [{"id": "other"}] });
        filter_conversation_list(&mut odd, |_| false);
        assert!(odd["data"].is_null());
    }

    #[test]
    fn config_writes_need_approval() {
        let cases = [
            (Method::POST, "/api/mcp/servers", AgentActionKind::Mcp),
            (Method::DELETE, "/api/mcp/servers/abc", AgentActionKind::Mcp),
            (Method::POST, "/api/mcp/test-connection", AgentActionKind::Mcp),
            (Method::PUT, "/api/providers/p1", AgentActionKind::Provider),
            (Method::PATCH, "/api/settings", AgentActionKind::Settings),
            (Method::PUT, "/api/settings/client", AgentActionKind::Settings),
            (Method::POST, "/api/agents/custom", AgentActionKind::Agent),
            (Method::PUT, "/api/assistants/a1", AgentActionKind::Assistant),
            (Method::POST, "/api/skills/import", AgentActionKind::Skill),
            (
                Method::POST,
                "/api/internal/conversation-cron/create",
                AgentActionKind::Cron,
            ),
            (Method::DELETE, "/api/cron/jobs/j1", AgentActionKind::Cron),
        ];
        for (method, path, kind) in cases {
            assert_eq!(
                classify(method.clone(), path),
                AgentRouteDecision::RequireApproval(kind),
                "{method} {path}"
            );
        }
    }

    #[test]
    fn read_like_posts_and_own_rename_are_allowed() {
        assert_eq!(
            classify(Method::POST, "/api/providers/fetch-models"),
            AgentRouteDecision::Allow
        );
        assert_eq!(
            classify(Method::PATCH, "/api/conversations/conv-own"),
            AgentRouteDecision::AllowOwnRename
        );
        assert_eq!(
            classify(Method::PATCH, "/api/conversations/other"),
            AgentRouteDecision::Deny
        );
    }

    #[test]
    fn everything_else_is_denied() {
        for (method, path) in [
            (Method::POST, "/api/conversations"),
            (Method::POST, "/api/conversations/conv-own/messages"),
            (Method::DELETE, "/api/conversations/conv-own"),
            (Method::POST, "/api/agent-approvals/abc/approve"),
            (Method::POST, "/api/webui/reset-password"),
            (Method::POST, "/api/fs/write"),
            (Method::POST, "/api/teams"),
        ] {
            assert_eq!(
                classify(method.clone(), path),
                AgentRouteDecision::Deny,
                "{method} {path}"
            );
        }
    }

    /// M7 E4: agents may neither read nor write diff annotations, nor send
    /// annotation batches or drive commit / push / PR delivery through the
    /// WorkMate API. These prefixes must never join an allow-list.
    #[test]
    fn review_and_delivery_routes_are_denied() {
        for (method, path) in [
            (Method::GET, "/api/review-threads"),
            (Method::GET, "/api/review-threads?scope_kind=scm&scope_id=/repo"),
            (Method::HEAD, "/api/review-threads"),
            (Method::GET, "/api/review-threads/rt_1"),
            (Method::POST, "/api/review-threads"),
            (Method::PATCH, "/api/review-threads/rt_1"),
            (Method::DELETE, "/api/review-threads/rt_1"),
            (Method::POST, "/api/review-threads/rt_1/resolve"),
            (Method::POST, "/api/review-threads/rt_1/reopen"),
            (Method::POST, "/api/review-threads/rt_1/comments"),
            (Method::GET, "/api/review-batches?target_conversation_id=conv-own"),
            (Method::POST, "/api/review-batches"),
            (Method::GET, "/api/scm-deliveries/host?repo_path=/repo"),
            (Method::POST, "/api/scm-deliveries/pull-request"),
            (Method::GET, "/api/scm-deliveries?repo_path=/repo"),
        ] {
            assert_eq!(
                classify(method.clone(), path),
                AgentRouteDecision::Deny,
                "{method} {path}"
            );
        }
    }

    /// M7 E4-4: `scm/commit` and `scm/push` travel over the user's `/ws`
    /// connection; an agent token is refused there, and any REST spelling of
    /// the source-control surface stays closed to agents too.
    #[test]
    fn commit_and_push_surfaces_are_denied() {
        for (method, path) in [
            (Method::GET, "/ws"),
            (Method::GET, "/ws?token=x"),
            (Method::POST, "/api/scm/commit"),
            (Method::POST, "/api/scm/push"),
            (Method::GET, "/api/scm/remote-info?repository=scm:r"),
        ] {
            assert_eq!(
                classify(method.clone(), path),
                AgentRouteDecision::Deny,
                "{method} {path}"
            );
        }
    }

    /// M7 E3: worktree / fan-out control is user-only. Guard against a future
    /// allowlist edit accidentally opening these prefixes to agent tokens.
    #[test]
    fn usage_ledger_routes_are_denied() {
        // M7 E5-1: spend/cost ledger is user-only; agent tokens never read or clear it.
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for path in [
                "/api/usage",
                "/api/usage?since=0&limit=10",
                "/api/usage?task_session_id=t1&run_id=r1",
                "/api/usage/summary",
                "/api/usage/summary?group_by=task&tz_offset_minutes=480",
            ] {
                assert_eq!(
                    classify(method.clone(), path),
                    AgentRouteDecision::Deny,
                    "{method} {path}"
                );
            }
        }
        let prefix = "/api/usage";
        assert!(
            !READ_PREFIXES
                .iter()
                .any(|p| has_prefix(prefix, p) || has_prefix(p, prefix))
        );
        assert!(!READ_LIKE_POSTS.iter().any(|p| has_prefix(p, prefix)));
        assert!(
            !APPROVAL_PREFIXES
                .iter()
                .any(|(p, _)| has_prefix(prefix, p) || has_prefix(p, prefix))
        );
    }

    #[test]
    fn inbox_routes_are_denied() {
        // M7 E5-4: the inbox is the user's attention queue; agents never read or ack it.
        for method in [Method::GET, Method::POST, Method::PUT, Method::DELETE] {
            for path in [
                "/api/inbox",
                "/api/inbox?unread_only=true",
                "/api/inbox/read-all",
                "/api/inbox/i1/read",
            ] {
                assert_eq!(
                    classify(method.clone(), path),
                    AgentRouteDecision::Deny,
                    "{method} {path}"
                );
            }
        }
    }

    #[test]
    fn task_run_replay_routes_are_denied() {
        // M7 E5-2: run replay exposes the full sanitized trace; user-only.
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for path in [
                "/api/task-sessions/t1/runs/r1/replay",
                "/api/task-sessions/t1/runs/r1/replay?after_sequence=10&limit=50",
                "/api/task-sessions/t1/runs/r1/trace",
            ] {
                assert_eq!(
                    classify(method.clone(), path),
                    AgentRouteDecision::Deny,
                    "{method} {path}"
                );
            }
        }
        let prefix = "/api/task-sessions";
        assert!(
            !READ_PREFIXES
                .iter()
                .any(|p| has_prefix(prefix, p) || has_prefix(p, prefix))
        );
        assert!(!READ_LIKE_POSTS.iter().any(|p| has_prefix(p, prefix)));
        assert!(
            !APPROVAL_PREFIXES
                .iter()
                .any(|(p, _)| has_prefix(prefix, p) || has_prefix(p, prefix))
        );
    }

    #[test]
    fn worktree_and_fanout_routes_are_denied() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for path in [
                "/api/worktrees",
                "/api/worktrees?repo_path=/r&status=ready",
                "/api/worktrees/capabilities",
                "/api/worktrees/estimate?repo_path=/r",
                "/api/worktrees/prune",
                "/api/worktrees/wt_1",
                "/api/worktrees/wt_1/cancel",
                "/api/worktrees/wt_1/retry",
                "/api/worktrees/wt_1/diff-stat",
                "/api/worktrees/wt_1/diff?path=a.txt",
                "/api/fanout-groups",
                "/api/fanout-groups/g1",
                "/api/fanout-groups/g1/compare",
                "/api/fanout-groups/g1/adopt",
                "/api/fanout-groups/g1/discard",
                "/api/fanout-groups/g1/cancel",
            ] {
                assert_eq!(
                    classify(method.clone(), path),
                    AgentRouteDecision::Deny,
                    "{method} {path}"
                );
            }
        }
        for prefix in ["/api/worktrees", "/api/fanout-groups"] {
            assert!(
                !READ_PREFIXES
                    .iter()
                    .any(|p| has_prefix(prefix, p) || has_prefix(p, prefix))
            );
            assert!(!READ_LIKE_POSTS.iter().any(|p| has_prefix(p, prefix)));
            assert!(
                !APPROVAL_PREFIXES
                    .iter()
                    .any(|(p, _)| has_prefix(prefix, p) || has_prefix(p, prefix))
            );
        }
    }

    #[test]
    fn rename_only_patch_detection() {
        assert!(is_rename_only_patch(br#"{"name":"new title"}"#));
        assert!(!is_rename_only_patch(
            br#"{"name":"x","extra":{"session_mode":"yolo"}}"#
        ));
        assert!(!is_rename_only_patch(br#"{}"#));
        assert!(!is_rename_only_patch(b"not json"));
    }

    #[test]
    fn redaction_masks_secret_strings_only() {
        let mut value = json!({
            "providers": [{"id": "p", "api_key": "sk-live", "base_url": "https://x", "max_tokens": 4096}],
            "env": {"GITHUB_PERSONAL_ACCESS_TOKEN": "ghp_x", "OPENAI_KEY": "sk-y"},
            "headers": {"Authorization": "Bearer abc"},
            "password": ""
        });
        redact_secrets(&mut value);
        assert_eq!(value["providers"][0]["api_key"], "[REDACTED]");
        assert_eq!(value["providers"][0]["base_url"], "https://x");
        assert_eq!(value["providers"][0]["max_tokens"], 4096);
        assert_eq!(value["env"]["GITHUB_PERSONAL_ACCESS_TOKEN"], "[REDACTED]");
        assert_eq!(value["env"]["OPENAI_KEY"], "[REDACTED]");
        assert_eq!(value["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(value["password"], "");
    }
}
