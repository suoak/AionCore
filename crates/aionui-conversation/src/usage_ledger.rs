use std::sync::Arc;

use std::collections::{BTreeMap, HashMap};

use aionui_api_types::{
    UsageCostAmount, UsageEventDto, UsageGroupBy, UsageListResponse, UsageSummaryGroup, UsageSummaryQuery,
    UsageSummaryResponse,
};
use aionui_common::now_ms;
use aionui_db::models::{ConversationRow, UsageEventRow};
use aionui_db::{IConversationRepository, IUsageEventRepository, InsertUsageEventParams, UsageEventFilter};
use serde_json::Value;
use tracing::warn;

use crate::error::ConversationError;

const USAGE_RETENTION_MS: i64 = 180 * 24 * 60 * 60 * 1000;
const USAGE_MAX_EVENTS: i64 = 50_000;

pub struct ContextUsageSpend {
    pub total_tokens: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub thought_tokens: i64,
    pub cached_read_tokens: i64,
    pub cached_write_tokens: i64,
    pub session_cost_amount: Option<f64>,
    pub cost_currency: Option<String>,
    pub model_id: Option<String>,
}

fn non_negative_int(value: Option<&Value>) -> i64 {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n.round() as i64)
        .unwrap_or(0)
}

fn spend_counter(payload: &Value, meta: Option<&Value>, snake: &str, camel: &str) -> i64 {
    let nested = meta.and_then(|m| m.get("usage"));
    non_negative_int(
        meta.and_then(|m| m.get(snake))
            .or_else(|| nested.and_then(|u| u.get(camel)))
            .or_else(|| nested.and_then(|u| u.get(snake)))
            .or_else(|| payload.get(snake))
            .or_else(|| payload.get(camel)),
    )
}

/// Occupancy-only frames have `used`/`size` and no per-turn spend. Those are ignored.
pub fn spend_from_context_usage(payload: &Value) -> Option<ContextUsageSpend> {
    let meta = payload.get("_meta");
    let input_tokens = spend_counter(payload, meta, "input_tokens", "inputTokens");
    let output_tokens = spend_counter(payload, meta, "output_tokens", "outputTokens");
    let thought_tokens = spend_counter(payload, meta, "thought_tokens", "reasoningTokens");
    let cached_read_tokens = spend_counter(payload, meta, "cached_read_tokens", "cachedReadTokens");
    let cached_write_tokens = spend_counter(payload, meta, "cached_write_tokens", "cacheCreationTokens");
    let explicit_total_tokens = spend_counter(payload, meta, "total_tokens", "totalTokens");
    let cost = payload.get("cost");
    let session_cost_amount = cost
        .and_then(|c| c.get("amount"))
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0);
    let cost_currency = cost
        .and_then(|c| c.get("currency"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let model_id = meta
        .and_then(|m| m.get("model_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "unknown")
        .map(str::to_owned);

    if input_tokens + output_tokens + thought_tokens == 0 && session_cost_amount.is_none() {
        return None;
    }

    let cache_tokens = cached_read_tokens.saturating_add(cached_write_tokens);
    let input_includes_cache = cache_tokens > 0 && input_tokens >= cache_tokens;
    Some(ContextUsageSpend {
        total_tokens: if explicit_total_tokens > 0 {
            explicit_total_tokens
        } else {
            input_tokens
                .saturating_add(output_tokens)
                .saturating_add(if input_includes_cache { 0 } else { cache_tokens })
        },
        input_tokens,
        output_tokens,
        thought_tokens,
        cached_read_tokens,
        cached_write_tokens,
        session_cost_amount,
        cost_currency,
        model_id,
    })
}

fn extra_string(extra: &Value, key: &str) -> Option<String> {
    extra
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn conversation_backend(row: &ConversationRow, extra: &Value) -> String {
    if let Some(backend) = extra_string(extra, "backend") {
        return backend;
    }
    if row.r#type == "aionrs" {
        return "aionrs".into();
    }
    if !row.r#type.trim().is_empty() {
        return row.r#type.clone();
    }
    "unknown".into()
}

fn conversation_model_id(row: &ConversationRow, extra: &Value, hinted: Option<&str>) -> Option<String> {
    if let Some(model) = hinted.map(str::trim).filter(|s| !s.is_empty() && *s != "unknown") {
        return Some(model.to_owned());
    }
    if let Some(model) = extra_string(extra, "current_model_id") {
        return Some(model);
    }
    row.model.as_deref().and_then(|raw| {
        let value = serde_json::from_str::<Value>(raw).ok()?;
        value
            .get("use_model")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    })
}

pub fn usage_event_fingerprint(turn_id: Option<&str>, spend: &ContextUsageSpend) -> String {
    if let Some(turn_id) = turn_id.map(str::trim).filter(|s| !s.is_empty()) {
        return format!("turn:{turn_id}");
    }
    format!(
        "counts:{}:{}:{}:{}:{}",
        spend.input_tokens,
        spend.output_tokens,
        spend.thought_tokens,
        spend.cached_read_tokens,
        spend.cached_write_tokens
    )
}

pub fn row_to_usage_dto(row: UsageEventRow) -> UsageEventDto {
    UsageEventDto {
        id: row.id,
        fingerprint: row.fingerprint,
        recorded_at: row.recorded_at,
        conversation_id: row.conversation_id,
        conversation_name: row.conversation_name,
        conversation_source: row.conversation_source,
        backend: row.backend,
        assistant_id: row.assistant_id,
        assistant_name: row.assistant_name,
        model_id: row.model_id,
        turn_id: row.turn_id,
        total_tokens: row.total_tokens,
        input_tokens: row.input_tokens,
        output_tokens: row.output_tokens,
        thought_tokens: row.thought_tokens,
        cached_read_tokens: row.cached_read_tokens,
        cached_write_tokens: row.cached_write_tokens,
        cost_delta: row.cost_delta,
        cost_currency: row.cost_currency,
        event_source: row.event_source,
        task_session_id: row.task_session_id,
        run_id: row.run_id,
        usage_status: if row.usage_status.is_empty() {
            USAGE_STATUS_REPORTED.to_owned()
        } else {
            row.usage_status
        },
    }
}

pub const USAGE_STATUS_REPORTED: &str = "reported";
pub const USAGE_STATUS_UNAVAILABLE: &str = "unavailable";

/// Task linkage carried by a relay that runs inside a task run (M7 E5-1).
#[derive(Debug, Clone, Copy)]
pub struct UsageTaskLink<'a> {
    pub task_session_id: &'a str,
    pub run_id: &'a str,
}

#[allow(clippy::too_many_arguments)]
pub async fn record_context_usage_spend(
    usage_repo: &dyn IUsageEventRepository,
    conversation_repo: &dyn IConversationRepository,
    user_id: &str,
    conversation_id: &str,
    turn_id: Option<&str>,
    model_hint: Option<&str>,
    task_link: Option<UsageTaskLink<'_>>,
    spend: &ContextUsageSpend,
) -> Result<Option<UsageEventRow>, ConversationError> {
    if user_id.trim().is_empty() || conversation_id.trim().is_empty() {
        return Ok(None);
    }

    let conversation = conversation_repo
        .get(user_id, conversation_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to load conversation for usage: {e}")))?;
    let Some(conversation) = conversation else {
        return Ok(None);
    };
    let extra = serde_json::from_str::<Value>(&conversation.extra).unwrap_or(Value::Null);
    let snapshot = conversation_repo
        .get_assistant_snapshot(user_id, conversation_id)
        .await
        .ok()
        .flatten();

    let previous_cost = usage_repo
        .last_session_cost(user_id, conversation_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to load previous usage cost: {e}")))?;
    let mut cost_delta = 0.0;
    let mut cost_currency = spend.cost_currency.clone();
    if let Some(amount) = spend.session_cost_amount {
        let previous_amount = match &previous_cost {
            Some((prev_amount, prev_currency))
                if cost_currency.as_deref().unwrap_or(prev_currency) == prev_currency.as_str() =>
            {
                *prev_amount
            }
            _ => 0.0,
        };
        cost_delta = (amount - previous_amount).max(0.0);
        if cost_currency.is_none() {
            cost_currency = previous_cost.map(|(_, currency)| currency);
        }
    }

    if spend.input_tokens + spend.output_tokens + spend.thought_tokens == 0 && cost_delta <= 0.0 {
        return Ok(None);
    }

    let fingerprint = usage_event_fingerprint(turn_id, spend);
    let backend = conversation_backend(&conversation, &extra);
    let event_source = if conversation.r#type == "aionrs" {
        "aionrs"
    } else {
        "acp"
    };
    let model_id = conversation_model_id(&conversation, &extra, model_hint.or(spend.model_id.as_deref()));
    let params = InsertUsageEventParams {
        user_id,
        conversation_id,
        recorded_at: now_ms(),
        fingerprint: &fingerprint,
        backend: &backend,
        conversation_source: conversation.source.as_deref().unwrap_or("aionui"),
        conversation_name: Some(conversation.name.as_str()).filter(|s| !s.is_empty()),
        assistant_id: snapshot.as_ref().map(|row| row.assistant_id.as_str()),
        assistant_name: None,
        model_id: model_id.as_deref(),
        turn_id,
        total_tokens: spend.total_tokens,
        input_tokens: spend.input_tokens,
        output_tokens: spend.output_tokens,
        thought_tokens: spend.thought_tokens,
        cached_read_tokens: spend.cached_read_tokens,
        cached_write_tokens: spend.cached_write_tokens,
        cost_delta,
        session_cost_amount: spend.session_cost_amount,
        cost_currency: cost_currency.as_deref(),
        event_source,
        task_session_id: task_link.map(|link| link.task_session_id),
        run_id: task_link.map(|link| link.run_id),
        usage_status: USAGE_STATUS_REPORTED,
    };

    let inserted = usage_repo
        .insert_if_new(&params)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to record usage: {e}")))?;
    if inserted.is_some()
        && let Err(error) = usage_repo
            .prune_for_user(user_id, now_ms() - USAGE_RETENTION_MS, USAGE_MAX_EVENTS)
            .await
    {
        warn!(user_id, error = %error, "Failed to prune usage ledger");
    }
    Ok(inserted)
}

pub async fn list_usage_events(
    usage_repo: &dyn IUsageEventRepository,
    user_id: &str,
    since: Option<i64>,
    limit: Option<i64>,
    task_session_id: Option<String>,
    run_id: Option<String>,
) -> Result<UsageListResponse, ConversationError> {
    if let Err(error) = usage_repo
        .prune_for_user(user_id, now_ms() - USAGE_RETENTION_MS, USAGE_MAX_EVENTS)
        .await
    {
        warn!(user_id, error = %error, "Failed to prune usage ledger before listing");
    }
    let limit = limit.unwrap_or(USAGE_MAX_EVENTS);
    let events = if task_session_id.is_some() || run_id.is_some() {
        usage_repo
            .list_filtered(
                user_id,
                &UsageEventFilter {
                    since,
                    task_session_id,
                    run_id,
                    limit,
                    ..UsageEventFilter::default()
                },
            )
            .await
    } else {
        usage_repo.list_for_user(user_id, since, limit).await
    }
    .map_err(|e| ConversationError::internal(format!("Failed to list usage: {e}")))?;
    Ok(UsageListResponse {
        events: events.into_iter().map(row_to_usage_dto).collect(),
    })
}

pub async fn clear_usage_events(
    usage_repo: &dyn IUsageEventRepository,
    user_id: &str,
) -> Result<u64, ConversationError> {
    usage_repo
        .clear_for_user(user_id)
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to clear usage: {e}")))
}

/// Server-side ledger aggregation for `GET /api/usage/summary` (M7 E5-1).
pub async fn summarize_usage(
    usage_repo: &dyn IUsageEventRepository,
    user_id: &str,
    query: UsageSummaryQuery,
) -> Result<UsageSummaryResponse, ConversationError> {
    let tz_offset_minutes = match query.tz_offset_minutes {
        Some(offset) if !(-14 * 60..=14 * 60).contains(&offset) => {
            return Err(ConversationError::bad_request(
                "tz_offset_minutes must be within -840..=840",
            ));
        }
        Some(offset) => offset,
        None => chrono::Local::now().offset().local_minus_utc() / 60,
    };
    if let (Some(since), Some(until)) = (query.since, query.until)
        && until <= since
    {
        return Err(ConversationError::bad_request("until must be greater than since"));
    }
    let rows = usage_repo
        .list_filtered(
            user_id,
            &UsageEventFilter {
                since: query.since,
                until: query.until,
                task_session_id: query.task_session_id.clone(),
                run_id: query.run_id.clone(),
                conversation_id: None,
                limit: USAGE_MAX_EVENTS,
            },
        )
        .await
        .map_err(|e| ConversationError::internal(format!("Failed to summarize usage: {e}")))?;
    let read_truncated = rows.len() as i64 >= USAGE_MAX_EVENTS;
    let (totals, mut groups) = aggregate_usage(&rows, query.group_by, tz_offset_minutes);
    let mut truncated = read_truncated;
    if !query.group_by.is_time_bucket()
        && let Some(limit) = query.limit.filter(|limit| *limit > 0)
        && groups.len() as i64 > limit
    {
        groups.truncate(limit as usize);
        truncated = true;
    }
    Ok(UsageSummaryResponse {
        group_by: query.group_by,
        since: query.since,
        until: query.until,
        tz_offset_minutes,
        totals,
        groups,
        truncated,
    })
}

#[derive(Default)]
struct GroupAcc {
    label: Option<String>,
    turns: i64,
    unavailable_turns: i64,
    costed_turns: i64,
    total_tokens: i64,
    input_tokens: i64,
    output_tokens: i64,
    thought_tokens: i64,
    cached_read_tokens: i64,
    cached_write_tokens: i64,
    cost: BTreeMap<String, f64>,
    first: Option<i64>,
    last: Option<i64>,
}

impl GroupAcc {
    fn add(&mut self, row: &UsageEventRow) {
        self.turns += 1;
        self.first = Some(self.first.map_or(row.recorded_at, |v| v.min(row.recorded_at)));
        self.last = Some(self.last.map_or(row.recorded_at, |v| v.max(row.recorded_at)));
        if row.usage_status == USAGE_STATUS_UNAVAILABLE {
            self.unavailable_turns += 1;
            return;
        }
        self.total_tokens += row.total_tokens;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        self.thought_tokens += row.thought_tokens;
        self.cached_read_tokens += row.cached_read_tokens;
        self.cached_write_tokens += row.cached_write_tokens;
        if row.session_cost_amount.is_some() || row.cost_delta > 0.0 {
            self.costed_turns += 1;
            let currency = row
                .cost_currency
                .as_deref()
                .filter(|c| !c.trim().is_empty())
                .unwrap_or("UNKNOWN")
                .to_owned();
            *self.cost.entry(currency).or_insert(0.0) += row.cost_delta;
        }
    }

    fn finish(self, key: String) -> UsageSummaryGroup {
        let reported_turns = self.turns - self.unavailable_turns;
        let cost_status = if self.costed_turns == 0 {
            "unavailable"
        } else if self.costed_turns == reported_turns && self.unavailable_turns == 0 {
            "reported"
        } else {
            "partial"
        };
        let cost = (!self.cost.is_empty()).then(|| {
            self.cost
                .into_iter()
                .map(|(currency, amount)| UsageCostAmount { currency, amount })
                .collect()
        });
        UsageSummaryGroup {
            key,
            label: self.label,
            turns: self.turns,
            unavailable_turns: self.unavailable_turns,
            total_tokens: self.total_tokens,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            thought_tokens: self.thought_tokens,
            cached_read_tokens: self.cached_read_tokens,
            cached_write_tokens: self.cached_write_tokens,
            cost,
            cost_status: cost_status.to_owned(),
            first_recorded_at: self.first,
            last_recorded_at: self.last,
        }
    }
}

fn local_date(recorded_at: i64, tz_offset_minutes: i32) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp_millis(recorded_at + i64::from(tz_offset_minutes) * 60_000)
        .map(|dt| dt.naive_utc().date())
}

fn group_key(row: &UsageEventRow, group_by: UsageGroupBy, tz_offset_minutes: i32) -> (String, Option<String>) {
    use chrono::Datelike;
    match group_by {
        UsageGroupBy::Day => (
            local_date(row.recorded_at, tz_offset_minutes)
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_default(),
            None,
        ),
        UsageGroupBy::Week => (
            local_date(row.recorded_at, tz_offset_minutes)
                .map(|d| {
                    let monday = d - chrono::Duration::days(i64::from(d.weekday().num_days_from_monday()));
                    monday.format("%Y-%m-%d").to_string()
                })
                .unwrap_or_default(),
            None,
        ),
        UsageGroupBy::Conversation => (row.conversation_id.clone(), row.conversation_name.clone()),
        UsageGroupBy::Assistant => (row.assistant_id.clone().unwrap_or_default(), row.assistant_name.clone()),
        UsageGroupBy::Task => (row.task_session_id.clone().unwrap_or_default(), None),
        UsageGroupBy::Run => (row.run_id.clone().unwrap_or_default(), None),
        UsageGroupBy::Model => (row.model_id.clone().unwrap_or_default(), None),
    }
}

/// Pure aggregation: totals always equal the sum of the groups.
pub fn aggregate_usage(
    rows: &[UsageEventRow],
    group_by: UsageGroupBy,
    tz_offset_minutes: i32,
) -> (UsageSummaryGroup, Vec<UsageSummaryGroup>) {
    let mut totals = GroupAcc::default();
    let mut groups: HashMap<String, GroupAcc> = HashMap::new();
    for row in rows {
        totals.add(row);
        let (key, label) = group_key(row, group_by, tz_offset_minutes);
        let acc = groups.entry(key).or_default();
        if acc.label.is_none() {
            acc.label = label;
        }
        acc.add(row);
    }
    let mut out: Vec<UsageSummaryGroup> = groups.into_iter().map(|(key, acc)| acc.finish(key)).collect();
    if group_by.is_time_bucket() {
        out.sort_by(|a, b| a.key.cmp(&b.key));
    } else {
        out.sort_by(|a, b| b.total_tokens.cmp(&a.total_tokens).then_with(|| a.key.cmp(&b.key)));
    }
    (totals.finish(String::new()), out)
}

pub fn maybe_usage_repo(
    repo: &std::sync::RwLock<Option<Arc<dyn IUsageEventRepository>>>,
) -> Option<Arc<dyn IUsageEventRepository>> {
    repo.read().ok().and_then(|guard| guard.clone())
}

#[cfg(test)]
mod tests {
    use super::spend_from_context_usage;
    use serde_json::json;

    #[test]
    fn occupancy_only_frames_are_not_spend() {
        assert!(spend_from_context_usage(&json!({ "used": 12_000, "size": 200_000 })).is_none());
    }

    #[test]
    fn end_of_turn_meta_is_spend() {
        let spend = spend_from_context_usage(&json!({
            "used": 12_400,
            "size": 200_000,
            "_meta": { "input_tokens": 900, "output_tokens": 80 }
        }))
        .expect("spend");
        assert_eq!(spend.input_tokens, 900);
        assert_eq!(spend.output_tokens, 80);
        assert_eq!(spend.total_tokens, 980);
    }

    #[test]
    fn grok_nested_usage_is_spend() {
        let spend = spend_from_context_usage(&json!({
            "used": 14_794,
            "size": 0,
            "_meta": {
                "usage": {
                    "inputTokens": 14675,
                    "outputTokens": 119,
                    "reasoningTokens": 77,
                    "cachedReadTokens": 11648
                }
            }
        }))
        .expect("spend");
        assert_eq!(spend.input_tokens, 14675);
        assert_eq!(spend.output_tokens, 119);
        assert_eq!(spend.thought_tokens, 77);
        assert_eq!(spend.cached_read_tokens, 11648);
        assert_eq!(spend.total_tokens, 14794);
    }

    #[test]
    fn explicit_turn_total_is_preserved() {
        let spend = spend_from_context_usage(&json!({
            "used": 200_000,
            "size": 200_000,
            "_meta": {
                "total_tokens": 41_224,
                "input_tokens": 919,
                "output_tokens": 305,
                "cached_read_tokens": 20_000,
                "cached_write_tokens": 20_000
            }
        }))
        .expect("spend");
        assert_eq!(spend.total_tokens, 41_224);
    }
}

#[cfg(test)]
mod summary_tests {
    use super::{USAGE_STATUS_UNAVAILABLE, aggregate_usage};
    use aionui_api_types::{UsageCostAmount, UsageGroupBy};
    use aionui_db::models::UsageEventRow;

    fn row(id: &str, recorded_at: i64, tokens: i64, cost: Option<(f64, &str)>) -> UsageEventRow {
        UsageEventRow {
            id: id.to_owned(),
            user_id: "u1".to_owned(),
            conversation_id: "c1".to_owned(),
            recorded_at,
            fingerprint: format!("turn:{id}"),
            backend: "claude".to_owned(),
            conversation_source: "aionui".to_owned(),
            conversation_name: Some("Chat".to_owned()),
            assistant_id: Some("a1".to_owned()),
            assistant_name: None,
            model_id: Some("m1".to_owned()),
            turn_id: Some(id.to_owned()),
            total_tokens: tokens,
            input_tokens: tokens - 1,
            output_tokens: 1,
            thought_tokens: 0,
            cached_read_tokens: 0,
            cached_write_tokens: 0,
            cost_delta: cost.map_or(0.0, |(amount, _)| amount),
            session_cost_amount: cost.map(|(amount, _)| amount),
            cost_currency: cost.map(|(_, currency)| currency.to_owned()),
            event_source: "acp".to_owned(),
            task_session_id: None,
            run_id: None,
            usage_status: "reported".to_owned(),
        }
    }

    // 2026-10-19 is a Monday. 23:30 UTC on Sunday 2026-10-18 is Monday 07:30 in UTC+8.
    const SUN_2330_UTC: i64 = 1_792_366_200_000;

    #[test]
    fn totals_equal_sum_of_groups() {
        let mut rows = vec![
            row("t1", SUN_2330_UTC, 10, Some((0.5, "USD"))),
            row("t2", SUN_2330_UTC + 86_400_000, 20, Some((0.25, "USD"))),
            row("t3", SUN_2330_UTC + 2 * 86_400_000, 30, None),
        ];
        rows[1].task_session_id = Some("task-1".to_owned());
        rows[1].run_id = Some("run-1".to_owned());
        for group_by in [
            UsageGroupBy::Day,
            UsageGroupBy::Week,
            UsageGroupBy::Conversation,
            UsageGroupBy::Assistant,
            UsageGroupBy::Task,
            UsageGroupBy::Run,
            UsageGroupBy::Model,
        ] {
            let (totals, groups) = aggregate_usage(&rows, group_by, 480);
            assert_eq!(totals.total_tokens, 60, "{group_by:?}");
            assert_eq!(groups.iter().map(|g| g.total_tokens).sum::<i64>(), 60, "{group_by:?}");
            assert_eq!(groups.iter().map(|g| g.turns).sum::<i64>(), 3, "{group_by:?}");
            assert_eq!(totals.cost_status, "partial");
        }
        let (_, by_task) = aggregate_usage(&rows, UsageGroupBy::Task, 480);
        let linked = by_task.iter().find(|g| g.key == "task-1").expect("task group");
        assert_eq!(linked.total_tokens, 20);
        assert_eq!(linked.cost_status, "reported");
        let unlinked = by_task.iter().find(|g| g.key.is_empty()).expect("chat group");
        assert_eq!(unlinked.turns, 2);
    }

    #[test]
    fn day_and_week_buckets_use_offset_and_monday() {
        let rows = vec![row("t1", SUN_2330_UTC, 10, None)];
        let (_, utc_days) = aggregate_usage(&rows, UsageGroupBy::Day, 0);
        assert_eq!(utc_days[0].key, "2026-10-18");
        let (_, taipei_days) = aggregate_usage(&rows, UsageGroupBy::Day, 480);
        assert_eq!(taipei_days[0].key, "2026-10-19");
        let (_, utc_weeks) = aggregate_usage(&rows, UsageGroupBy::Week, 0);
        assert_eq!(utc_weeks[0].key, "2026-10-12");
        let (_, taipei_weeks) = aggregate_usage(&rows, UsageGroupBy::Week, 480);
        assert_eq!(taipei_weeks[0].key, "2026-10-19");
    }

    #[test]
    fn missing_cost_is_null_and_unavailable() {
        let mut rows = vec![row("t1", SUN_2330_UTC, 10, None)];
        let (totals, _) = aggregate_usage(&rows, UsageGroupBy::Day, 0);
        assert_eq!(totals.cost, None);
        assert_eq!(totals.cost_status, "unavailable");

        rows[0].usage_status = USAGE_STATUS_UNAVAILABLE.to_owned();
        let (totals, _) = aggregate_usage(&rows, UsageGroupBy::Day, 0);
        assert_eq!(totals.unavailable_turns, 1);
        assert_eq!(
            totals.total_tokens, 0,
            "unavailable turns never contribute invented tokens"
        );

        let (empty, groups) = aggregate_usage(&[], UsageGroupBy::Day, 0);
        assert!(groups.is_empty());
        assert_eq!(empty.cost, None);
        assert_eq!(empty.cost_status, "unavailable");
    }

    #[test]
    fn currencies_are_summed_separately() {
        let rows = vec![
            row("t1", SUN_2330_UTC, 10, Some((1.0, "USD"))),
            row("t2", SUN_2330_UTC, 10, Some((2.0, "CNY"))),
            row("t3", SUN_2330_UTC, 10, Some((0.5, "USD"))),
        ];
        let (totals, _) = aggregate_usage(&rows, UsageGroupBy::Model, 0);
        assert_eq!(
            totals.cost,
            Some(vec![
                UsageCostAmount {
                    currency: "CNY".to_owned(),
                    amount: 2.0
                },
                UsageCostAmount {
                    currency: "USD".to_owned(),
                    amount: 1.5
                },
            ])
        );
        assert_eq!(totals.cost_status, "reported");
    }

    #[tokio::test]
    async fn task_run_spend_carries_task_and_run_ids() {
        use super::{ContextUsageSpend, UsageTaskLink, record_context_usage_spend};
        use aionui_db::models::ConversationRow;
        use aionui_db::{
            IConversationRepository, IUsageEventRepository, SqliteConversationRepository, SqliteUsageEventRepository,
            init_database_memory,
        };

        let db = init_database_memory().await.unwrap();
        let conversations = SqliteConversationRepository::new(db.pool().clone());
        let usage = SqliteUsageEventRepository::new(db.pool().clone());
        let user = "system_default_user";
        let now = aionui_common::now_ms();
        conversations
            .create(&ConversationRow {
                id: "conv-task".into(),
                user_id: user.into(),
                name: "Task run".into(),
                r#type: "acp".into(),
                extra: "{}".into(),
                model: None,
                status: Some("running".into()),
                source: Some("aionui".into()),
                channel_chat_id: None,
                pinned: false,
                pinned_at: None,
                created_at: now,
                updated_at: now,
                project_id: None,
                folder_id: None,
                name_source: None,
            })
            .await
            .unwrap();
        let spend = |input: i64| ContextUsageSpend {
            total_tokens: input + 2,
            input_tokens: input,
            output_tokens: 2,
            thought_tokens: 0,
            cached_read_tokens: 0,
            cached_write_tokens: 0,
            session_cost_amount: None,
            cost_currency: None,
            model_id: None,
        };
        let linked = record_context_usage_spend(
            &usage,
            &conversations,
            user,
            "conv-task",
            Some("turn-task"),
            None,
            Some(UsageTaskLink {
                task_session_id: "task-1",
                run_id: "run-1",
            }),
            &spend(10),
        )
        .await
        .unwrap()
        .expect("inserted");
        assert_eq!(linked.task_session_id.as_deref(), Some("task-1"));
        assert_eq!(linked.run_id.as_deref(), Some("run-1"));
        assert_eq!(linked.usage_status, "reported");

        let chat = record_context_usage_spend(
            &usage,
            &conversations,
            user,
            "conv-task",
            Some("turn-chat"),
            None,
            None,
            &spend(5),
        )
        .await
        .unwrap()
        .expect("inserted");
        assert_eq!(chat.task_session_id, None);
        assert_eq!(usage.list_for_user(user, None, 10).await.unwrap().len(), 2);
    }
}
