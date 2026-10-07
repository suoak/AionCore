//! Stable WorkMate projection of Codex app-server account/authentication data.
//!
//! OAuth credentials remain owned by Codex. These DTOs intentionally contain
//! no token fields and never mirror Codex's unstable `chatgptAuthTokens` API.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexSnapshotFreshness {
    Fresh,
    Refreshing,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexSnapshotSource {
    Read,
    NotificationMerge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexUsageAvailability {
    Available,
    Limited,
    Blocked,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRateLimitWindow {
    pub used_percent: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_duration_mins: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexCreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexSpendControlSnapshot {
    pub limit: String,
    pub used: String,
    pub remaining_percent: i64,
    pub resets_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRateLimitBucket {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub normal_model_slug: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<CodexRateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary: Option<CodexRateLimitWindow>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<CodexCreditsSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub individual_limit: Option<CodexSpendControlSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control_reached: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexResetCreditSummary {
    pub available_count: i64,
    /// Read-only display rows. WorkMate exposes no consume/redeem mutation.
    #[serde(default)]
    pub credits: Vec<CodexResetCredit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexResetCredit {
    pub id: String,
    pub status: String,
    pub reset_type: String,
    pub granted_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexRateLimitsView {
    /// Provider authority. `None` is deliberately preserved as unknown.
    pub ordinary_usage_allowed: Option<bool>,
    pub availability: CodexUsageAvailability,
    pub buckets: Vec<CodexRateLimitBucket>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits: Option<CodexResetCreditSummary>,
    pub fetched_at: i64,
    pub source: CodexSnapshotSource,
    pub freshness: CodexSnapshotFreshness,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CodexAccountUsageSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lifetime_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_daily_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longest_running_turn_sec: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_streak_days: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longest_streak_days: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexDailyUsageBucket {
    /// Provider-defined date/timezone. WorkMate performs no local rebucketing.
    pub start_date: String,
    pub tokens: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexAccountUsageView {
    pub summary: CodexAccountUsageSummary,
    #[serde(default)]
    pub daily_buckets: Vec<CodexDailyUsageBucket>,
    pub fetched_at: i64,
    pub source: CodexSnapshotSource,
    pub freshness: CodexSnapshotFreshness,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CodexDiagnosticStatus {
    Pass,
    Warn,
    Fail,
    NotApplicable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexDiagnosticCheck {
    pub id: String,
    pub status: CodexDiagnosticStatus,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexDiagnosticsView {
    pub checks: Vec<CodexDiagnosticCheck>,
    pub fetched_at: i64,
    pub freshness: CodexSnapshotFreshness,
}

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
    QuotaUnavailable,
    DiagnosticFailed,
    RateLimited,
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
    /// Stable projection of `account/rateLimits/read`; raw provider JSON never crosses the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<CodexRateLimitsView>,
    /// Provider-reported activity only; no billing or cost inference.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<CodexAccountUsageView>,
    pub diagnostics: CodexDiagnosticsView,
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
