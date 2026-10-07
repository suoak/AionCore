//! Stable WorkMate projection of Codex app-server account/authentication data.
//!
//! OAuth credentials remain owned by Codex. These DTOs intentionally contain
//! no token fields and never mirror Codex's unstable `chatgptAuthTokens` API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexAuthState {
    Unknown,
    SignedOut,
    Authenticating,
    SignedIn,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexAccountErrorCode {
    LoginCancelled,
    LoginFailed,
    NotAuthenticated,
    AccountReadFailed,
    LogoutFailed,
    RateLimitReadFailed,
    UsageReadFailed,
    RuntimeUnavailable,
    ProtocolUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CodexLoginAttemptState {
    Waiting,
    Cancelling,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexLoginAttempt {
    pub login_id: String,
    pub started_at: i64,
    pub state: CodexLoginAttemptState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexAccountSnapshot {
    pub auth_state: CodexAuthState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    pub requires_openai_auth: bool,
    /// Experimental diagnostic only. It is never part of the signed-in gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_routing: Option<serde_json::Value>,
    pub updated_at: i64,
    pub generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_login: Option<CodexLoginAttempt>,
}

impl Default for CodexAccountSnapshot {
    fn default() -> Self {
        Self {
            auth_state: CodexAuthState::Unknown,
            auth_mode: None,
            account_type: None,
            email: None,
            plan_type: None,
            requires_openai_auth: true,
            workspace_routing: None,
            updated_at: 0,
            generation: 0,
            active_login: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexAccountWarning {
    pub code: CodexAccountErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexAccountView {
    pub account: CodexAccountSnapshot,
    /// Official response from `account/rateLimits/read`; absent on optional failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<serde_json::Value>,
    /// Official response from `account/usage/read`; absent on optional failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_usage: Option<serde_json::Value>,
    #[serde(default)]
    pub warnings: Vec<CodexAccountWarning>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    pub required_version: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodexLoginStartResponse {
    pub account: CodexAccountSnapshot,
    /// Ephemeral authorization URL. Callers must open it externally and discard it.
    pub authorization_url: String,
}
