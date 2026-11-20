//! Agent preflight: one aggregated readiness report per agent.
//!
//! `GET /api/agents/preflight` folds the stored availability snapshot, the
//! enable/install state and the managed-runtime state of every agent into a
//! short list of checks the UI can render before a task starts. Each failing
//! check may carry a `fix` the UI maps to an existing action (enable, run a
//! health check, open login, configure a model provider, ...).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

/// Severity of a single preflight check. Ordered so `max()` yields the worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightLevel {
    Pass,
    Warn,
    Fail,
}

/// Stable identifier of a preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightCheckKey {
    /// The user has the agent enabled.
    Enabled,
    /// The agent's CLI / package is installed.
    Installed,
    /// A managed runtime the agent depends on is ready.
    Runtime,
    /// The last health check reached the agent.
    Connection,
    /// The agent is reachable but needs the user to log in.
    Auth,
    /// The built-in agent has an enabled model provider.
    Provider,
    /// The installed CLI version matches the tested range.
    Version,
}

/// The UI action that resolves a failing check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreflightFixAction {
    /// `PATCH /api/agents/{id}/enabled`.
    Enable,
    /// Open the install flow for the agent.
    Install,
    /// `POST /api/agents/{id}/health-check`.
    HealthCheck,
    /// Open the agent's login flow.
    Login,
    /// Open model provider settings.
    ConfigureProvider,
    /// `POST /api/agents/{id}/runtime/prepare`.
    PrepareRuntime,
    /// Show the CLI update guidance.
    UpdateCli,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightFix {
    pub action: PreflightFixAction,
    /// What the action applies to: an agent id, hub package id or settings route.
    pub target: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreflightCheck {
    pub key: PreflightCheckKey,
    pub level: PreflightLevel,
    /// English fallback text; the UI translates by `key` + `code`.
    pub message: String,
    /// Machine-readable reason (the availability error code when there is one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<PreflightFix>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPreflight {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backend: Option<String>,
    /// Worst level among `checks`.
    pub level: PreflightLevel,
    /// True when the agent is enabled and no check fails.
    pub ready: bool,
    pub checks: Vec<PreflightCheck>,
    /// When the underlying health snapshot was taken, if ever.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<TimestampMs>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn levels_order_from_pass_to_fail() {
        assert!(PreflightLevel::Pass < PreflightLevel::Warn);
        assert!(PreflightLevel::Warn < PreflightLevel::Fail);
    }

    #[test]
    fn check_omits_empty_code_and_fix() {
        let check = PreflightCheck {
            key: PreflightCheckKey::Connection,
            level: PreflightLevel::Pass,
            message: "ok".to_owned(),
            code: None,
            fix: None,
        };
        assert_eq!(
            serde_json::to_value(&check).unwrap(),
            json!({ "key": "connection", "level": "pass", "message": "ok" })
        );
    }

    #[test]
    fn fix_serializes_action_and_target() {
        let check = PreflightCheck {
            key: PreflightCheckKey::Auth,
            level: PreflightLevel::Fail,
            message: "Log in".to_owned(),
            code: Some("auth_required".to_owned()),
            fix: Some(PreflightFix {
                action: PreflightFixAction::ConfigureProvider,
                target: "a".to_owned(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&check).unwrap(),
            json!({
                "key": "auth",
                "level": "fail",
                "message": "Log in",
                "code": "auth_required",
                "fix": { "action": "configure_provider", "target": "a" }
            })
        );
    }
}
