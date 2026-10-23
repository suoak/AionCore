# E5-1: task-linked usage ledger and server-side summary (Core)

## Status

```text
TYPE      = FEATURE (Core only; UI cost panel is E5-6)
SCOPE     = M7 / E5-1 (design: m7-design/E5-replay-cost-context-inbox.md §4, §5, §7)
MIGRATION = 067_usage_events_task_link.sql (usage_events.task_session_id, run_id, usage_status)
LEDGER    = existing usage_events (045/047) is extended; no second accounting table
```

## What ships

- Migration 067 adds `task_session_id`, `run_id` (nullable, no FK so history
  survives task deletion) and `usage_status` (`reported` | `unavailable`,
  CHECK-constrained, default `reported` so pre-067 rows read as reported).
  Indexes: `(user_id, task_session_id, recorded_at)`, `(user_id, run_id)`.
- The stream relay passes its `task_trace` (task id + run id) into
  `record_context_usage_spend`, so every spend row written while a task run
  drives the conversation is linked to that run. Chat turns stay unlinked.
- `IUsageEventRepository::list_filtered` — user-scoped read with
  `since` / `until` (exclusive) / `task_session_id` / `run_id` /
  `conversation_id`, capped at the 50 000-row retention limit.
- `GET /api/usage` accepts `task_session_id` and `run_id`; each event now
  carries `task_session_id`, `run_id`, `usage_status`.
- `GET /api/usage/summary` —
  `group_by=day|week|conversation|assistant|task|run|model`, `since`, `until`,
  `tz_offset_minutes` (−840..=840, default = Core host zone), `task_session_id`,
  `run_id`, `limit` (non-time dimensions, largest first).
  Returns `totals` + `groups`; each group has token sums, `turns`,
  `unavailable_turns`, per-currency `cost` (`null` when nothing reported a
  cost) and `cost_status` (`reported` | `partial` | `unavailable`). Weeks start
  on Monday. Totals always equal the sum of the groups.

## Rules

- No price-table estimation: cost is only what the provider reported. Missing
  cost is `null` + `unavailable`, never 0 dressed up as data.
- Currencies are never mixed; a row without a currency code is bucketed as
  `UNKNOWN`.
- Agent tokens are denied on `/api/usage*` (default-deny; guarded by
  `agent_scope::usage_ledger_routes_are_denied`).

## Not in E5-1

Writing `usage_status = unavailable` rows for turns that end without usage,
run replay (E5-2), context ring (E5-3), Inbox (E5-4), retry/timeout (E5-5),
UI (E5-6).
