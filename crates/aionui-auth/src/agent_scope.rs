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
const READ_PREFIXES: &[&str] = &[
    "/api/conversations",
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
            "/api/conversations?limit=50",
            "/api/conversations/conv-own/messages?limit=10",
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
