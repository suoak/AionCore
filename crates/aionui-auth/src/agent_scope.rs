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
    /// M8 E7: Squad template writes.
    Squad,
    /// M8 Q9 (D6-7): an agent asking to create a work item. Only
    /// `POST /api/task-sessions` qualifies, and the handler additionally
    /// limits the body to an unassigned `backlog` item.
    WorkItem,
}

/// Per-conversation read sub-resources an agent token may NOT read even for
/// its own / `@@`-granted conversation (M7 E5-3: the context ring is a
/// user-facing cost/context view, not agent input).
const AGENT_DENIED_CONVERSATION_SUBPATHS: &[&str] = &["context-breakdown"];

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
    "/api/squads",
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
    ("/api/squads", AgentActionKind::Squad),
];

/// M8 Q9: the only work-item write an agent token may request (with user
/// approval). Every other `/api/task-sessions` write — board moves,
/// assignment, review, comments, edits — stays outside agent scope.
pub const AGENT_WORK_ITEM_CREATE_PATH: &str = "/api/task-sessions";

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
            let mut parts = rest.split('/');
            let id = parts.next().unwrap_or("");
            let sub = parts.next().unwrap_or("");
            return if id.is_empty() || id == "." || id == ".." || AGENT_DENIED_CONVERSATION_SUBPATHS.contains(&sub) {
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

    if method == Method::POST && path == AGENT_WORK_ITEM_CREATE_PATH {
        return AgentRouteDecision::RequireApproval(AgentActionKind::WorkItem);
    }

    if matches!(method, &Method::POST | &Method::PUT | &Method::PATCH | &Method::DELETE)
        && let Some((_, kind)) = APPROVAL_PREFIXES.iter().find(|(prefix, _)| has_prefix(path, prefix))
    {
        return AgentRouteDecision::RequireApproval(*kind);
    }

    AgentRouteDecision::Deny
}

/// Auto-inject skill that owns a write-capable helper surface (B-15).
///
/// A conversation-helper token carries the set of these skills actually
/// installed in its session; requests to a surface whose skill is missing are
/// refused with a reason. The read-only diagnostic surface, own-conversation
/// reads/rename and the approval status route stay on the base scope because
/// `aioncore diagnose` is used by several non-auto-inject skills and assistant
/// rules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HelperSkill {
    /// `aionui-config`
    Config,
    /// `cron`
    Cron,
    /// `conversation-create`
    ConversationCreate,
    /// `session-message`
    SessionMessage,
}

impl HelperSkill {
    pub const ALL: [HelperSkill; 4] = [
        HelperSkill::Config,
        HelperSkill::Cron,
        HelperSkill::ConversationCreate,
        HelperSkill::SessionMessage,
    ];

    /// The builtin skill name, as stored in `conversation.extra.skills`.
    pub fn skill_name(self) -> &'static str {
        match self {
            Self::Config => "aionui-config",
            Self::Cron => "cron",
            Self::ConversationCreate => "conversation-create",
            Self::SessionMessage => "session-message",
        }
    }

    fn bit(self) -> u8 {
        match self {
            Self::Config => 1,
            Self::Cron => 2,
            Self::ConversationCreate => 4,
            Self::SessionMessage => 8,
        }
    }
}

/// Set of [`HelperSkill`]s granted to a helper token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HelperSkillSet(u8);

impl HelperSkillSet {
    /// Every helper surface — the behavior for sessions that exclude nothing.
    pub fn all() -> Self {
        Self(HelperSkill::ALL.iter().fold(0, |acc, skill| acc | skill.bit()))
    }

    pub fn empty() -> Self {
        Self(0)
    }

    #[must_use]
    pub fn with(self, skill: HelperSkill) -> Self {
        Self(self.0 | skill.bit())
    }

    pub fn contains(self, skill: HelperSkill) -> bool {
        self.0 & skill.bit() != 0
    }
}

impl FromIterator<HelperSkill> for HelperSkillSet {
    fn from_iter<I: IntoIterator<Item = HelperSkill>>(iter: I) -> Self {
        iter.into_iter().fold(Self::empty(), Self::with)
    }
}

const CRON_PREFIXES: &[&str] = &["/api/cron", "/api/internal/conversation-cron"];

/// Skills any one of which must be installed for an agent-token request to
/// reach its route; `None` means the base helper scope suffices.
///
/// Only write-capable surfaces are gated: config writes (approval routes) and
/// the provider/MCP probe POSTs need `aionui-config`; cron writes need
/// `aionui-config` or `cron`. Reads stay on the base scope.
pub fn required_helper_skills(method: &Method, path: &str) -> Option<&'static [HelperSkill]> {
    if method == Method::GET || method == Method::HEAD {
        return None;
    }
    let path = normalized_path(path)?;
    if CRON_PREFIXES.iter().any(|prefix| has_prefix(path, prefix)) {
        return Some(&[HelperSkill::Config, HelperSkill::Cron]);
    }
    if (method == Method::POST && path == AGENT_WORK_ITEM_CREATE_PATH)
        || READ_LIKE_POSTS.contains(&path)
        || APPROVAL_PREFIXES.iter().any(|(prefix, _)| has_prefix(path, prefix))
    {
        return Some(&[HelperSkill::Config]);
    }
    None
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

    #[test]
    fn helper_skill_gate_only_applies_to_write_surfaces() {
        for path in [
            "/api/mcp/servers",
            "/api/providers",
            "/api/cron/jobs",
            "/api/conversations",
        ] {
            assert_eq!(required_helper_skills(&Method::GET, path), None, "{path}");
        }
        assert_eq!(
            required_helper_skills(&Method::POST, "/api/mcp/servers"),
            Some(&[HelperSkill::Config][..])
        );
        assert_eq!(
            required_helper_skills(&Method::POST, "/api/providers/fetch-models"),
            Some(&[HelperSkill::Config][..])
        );
        assert_eq!(
            required_helper_skills(&Method::DELETE, "/api/skills/foo"),
            Some(&[HelperSkill::Config][..])
        );
        assert_eq!(
            required_helper_skills(&Method::POST, "/api/cron/jobs"),
            Some(&[HelperSkill::Config, HelperSkill::Cron][..])
        );
        assert_eq!(
            required_helper_skills(&Method::PUT, "/api/internal/conversation-cron/x"),
            Some(&[HelperSkill::Config, HelperSkill::Cron][..])
        );
        // Base surface: own rename and anything that is not a skill-owned write.
        assert_eq!(required_helper_skills(&Method::PATCH, "/api/conversations/c1"), None);
        assert_eq!(required_helper_skills(&Method::POST, "/api/agent-approvals/x"), None);
    }

    #[test]
    fn helper_skill_set_all_contains_every_skill() {
        let all = HelperSkillSet::all();
        for skill in HelperSkill::ALL {
            assert!(all.contains(skill));
            assert!(!HelperSkillSet::empty().contains(skill));
        }
        let set: HelperSkillSet = [HelperSkill::Cron].into_iter().collect();
        assert!(set.contains(HelperSkill::Cron));
        assert!(!set.contains(HelperSkill::Config));
        assert_eq!(HelperSkill::Config.skill_name(), "aionui-config");
    }
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
            (Method::POST, "/api/squads", AgentActionKind::Squad),
            (Method::PATCH, "/api/squads/s1", AgentActionKind::Squad),
            (Method::DELETE, "/api/squads/s1", AgentActionKind::Squad),
            (Method::POST, "/api/task-sessions", AgentActionKind::WorkItem),
            (Method::POST, "/api/task-sessions/", AgentActionKind::WorkItem),
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
    fn q9_agents_may_only_request_work_item_creation() {
        // Creation is the single approvable work-item write.
        assert_eq!(
            classify(Method::POST, "/api/task-sessions?x=1"),
            AgentRouteDecision::RequireApproval(AgentActionKind::WorkItem)
        );
        // Board moves, assignment, review, comments, edits, execution and
        // reads stay outside agent scope (no approval card either).
        for (method, path) in [
            (Method::PATCH, "/api/task-sessions/t1/board"),
            (Method::POST, "/api/task-sessions/t1/review"),
            (Method::PATCH, "/api/task-sessions/t1"),
            (Method::POST, "/api/task-sessions/t1/comments"),
            (Method::POST, "/api/task-sessions/t1/execute"),
            (Method::POST, "/api/task-sessions/t1/automatic-plan"),
            (Method::PUT, "/api/task-sessions/t1/retry-policy"),
            (Method::POST, "/api/task-sessions/t1/approvals/a1/decision"),
            (Method::PUT, "/api/task-sessions"),
            (Method::DELETE, "/api/task-sessions"),
            (Method::GET, "/api/task-sessions"),
            (Method::GET, "/api/task-sessions/board"),
        ] {
            assert_eq!(
                classify(method.clone(), path),
                AgentRouteDecision::Deny,
                "{method} {path}"
            );
        }
        assert_eq!(
            required_helper_skills(&Method::POST, "/api/task-sessions"),
            Some(&[HelperSkill::Config][..])
        );
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
    fn task_retry_policy_and_execution_routes_are_denied() {
        // M7 E5-5: retry policy decides how often an approval may be
        // re-executed; an agent must never be able to widen it or re-run.
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for path in [
                "/api/task-sessions/t1/retry-policy",
                "/api/task-sessions/t1/retry-policy?max_attempts=3",
                "/api/task-sessions/t1/execute",
                "/api/task-sessions/t1/runs",
                "/api/task-sessions/t1",
                // M8 E6-3 board: review gate, user credential only.
                "/api/task-sessions/board",
                "/api/task-sessions/t1/board",
                "/api/task-sessions/t1/review",
                "/api/task-sessions/t1/comments",
                "/api/task-sessions/t1/comments/c1",
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

    #[test]
    fn context_breakdown_route_is_denied_even_for_own_conversation() {
        for method in [
            Method::GET,
            Method::HEAD,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
        ] {
            for path in [
                "/api/conversations/own/context-breakdown",
                "/api/conversations/own/context-breakdown?x=1",
                "/api/conversations/other/context-breakdown",
            ] {
                assert_eq!(
                    classify_agent_request(&method, path, "own"),
                    AgentRouteDecision::Deny,
                    "{method} {path}"
                );
            }
        }
        // Sibling reads keep their existing per-conversation scoping.
        assert_eq!(
            classify_agent_request(&Method::GET, "/api/conversations/own/usage", "own"),
            AgentRouteDecision::AllowConversationRead("own".to_owned())
        );
    }
}
