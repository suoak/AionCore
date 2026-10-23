use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageEventDto {
    pub id: String,
    pub fingerprint: String,
    pub recorded_at: TimestampMs,
    pub conversation_id: String,
    pub conversation_name: Option<String>,
    pub conversation_source: String,
    pub backend: String,
    pub assistant_id: Option<String>,
    pub assistant_name: Option<String>,
    pub model_id: Option<String>,
    pub turn_id: Option<String>,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub cost_delta: f64,
    pub cost_currency: Option<String>,
    pub event_source: String,
    /// M7 E5-1: task session / run this turn belongs to (None for chat turns).
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    /// `reported` or `unavailable` (provider reported no usage; never estimated).
    #[serde(default = "default_usage_status")]
    pub usage_status: String,
}

fn default_usage_status() -> String {
    "reported".to_owned()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageListQuery {
    pub since: Option<TimestampMs>,
    pub limit: Option<i64>,
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageListResponse {
    pub events: Vec<UsageEventDto>,
}

/// Grouping dimension for `GET /api/usage/summary` (M7 E5-1).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum UsageGroupBy {
    #[default]
    Day,
    Week,
    Conversation,
    Assistant,
    Task,
    Run,
    Model,
}

impl UsageGroupBy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Conversation => "conversation",
            Self::Assistant => "assistant",
            Self::Task => "task",
            Self::Run => "run",
            Self::Model => "model",
        }
    }

    pub fn is_time_bucket(self) -> bool {
        matches!(self, Self::Day | Self::Week)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct UsageSummaryQuery {
    #[serde(default)]
    pub group_by: UsageGroupBy,
    pub since: Option<TimestampMs>,
    /// Exclusive upper bound.
    pub until: Option<TimestampMs>,
    /// Minutes east of UTC used for day/week buckets; defaults to the Core host zone.
    pub tz_offset_minutes: Option<i32>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    /// Max groups returned for non-time dimensions (largest first).
    pub limit: Option<i64>,
}

/// Provider-reported spend in one currency. Never estimated from a price table.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageCostAmount {
    pub currency: String,
    pub amount: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageSummaryGroup {
    /// Bucket key: `YYYY-MM-DD` (day, or the Monday of a week) or the dimension id.
    /// Empty string means "not linked" (e.g. chat turns when grouping by task).
    pub key: String,
    pub label: Option<String>,
    pub turns: i64,
    pub unavailable_turns: i64,
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    /// Per-currency totals; `None` when no turn in the group reported a cost.
    pub cost: Option<Vec<UsageCostAmount>>,
    /// `reported` (every reported turn carried cost), `partial`, or `unavailable`.
    pub cost_status: String,
    pub first_recorded_at: Option<TimestampMs>,
    pub last_recorded_at: Option<TimestampMs>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UsageSummaryResponse {
    pub group_by: UsageGroupBy,
    pub since: Option<TimestampMs>,
    pub until: Option<TimestampMs>,
    pub tz_offset_minutes: i32,
    pub totals: UsageSummaryGroup,
    pub groups: Vec<UsageSummaryGroup>,
    /// True when the ledger read hit the retention cap or groups were cut by `limit`.
    pub truncated: bool,
}
