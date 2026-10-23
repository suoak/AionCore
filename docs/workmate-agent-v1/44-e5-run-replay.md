# E5-2: paged run replay and `unavailable` usage rows (Core)

## Status

```text
TYPE      = FEATURE (Core only; replay UI is E5-6)
SCOPE     = M7 / E5-2 (design: m7-design/E5-replay-cost-context-inbox.md §4)
MIGRATION = none (reads task_trace_events from 062, usage_events from 045/067)
```

## What ships

### `GET /api/task-sessions/{id}/runs/{run_id}/replay`

Query: `after_sequence` (default 0, must be >= 0), `limit` (default 200,
clamped to 1..=1000).

Response (`TaskRunReplayResponse`):

| field | meaning |
| --- | --- |
| `run` | the run (status, timestamps, model) |
| `events` | trace events with `sequence > after_sequence`, ascending, at most `limit` |
| `next_after_sequence` | cursor for the next request |
| `has_more` | page was full and stopped below `latest_sequence` |
| `latest_sequence` | run's allocated high-water mark (`task_runs.next_trace_sequence`) |
| `retention_gap` | sequences in the requested window are missing |
| `approvals` / `evidence` | first page only (`after_sequence = 0`); evidence links to events via `trace_event_id` |

- Paging is keyed on the per-run trace sequence (allocated densely from 1 in
  the same transaction as the insert), so stitching pages by
  `next_after_sequence` returns every retained event exactly once — no
  duplicates and no gaps. A live client keeps polling with the last cursor;
  a caught-up cursor returns an empty page with the same cursor.
- `retention_gap = true` when a hole exists between the cursor and the first
  returned event, between returned events, or between the last returned event
  and `latest_sequence` on a short page. The cursor then skips the hole so
  callers always make progress. No pruning of trace rows exists today; the
  flag is there so future retention cannot silently truncate a replay.
- Payloads are the stored ones re-sanitized with `trace_redaction::sanitize_json`
  (the same path as `/trace`); raw tool output is never replayed.
- Ownership: the run is loaded with the caller's user id; another user's run
  is 404. Agent tokens are denied (`/api/task-sessions/**` is not in any agent
  allow-list; guard test `task_run_replay_routes_are_denied`).

### `unavailable` usage rows

- The stream relay tracks whether the provider reported spend for the turn
  (`AcpContextUsage` that parses as spend). On a terminal `Finish`/`Error`
  (not a deferred auto-replay error, not while deleting) with no report, it
  writes a `usage_status='unavailable'` row via
  `usage_ledger::record_unavailable_turn_usage`: tokens 0, `cost_delta` 0,
  `session_cost_amount` NULL, fingerprint `turn:<turn_id>`, carrying the task
  link when the turn runs inside a task run.
- Summaries count such rows only in `unavailable_turns`; missing usage stays
  visible instead of being read as "free".
- If a provider-reported row for the same `turn:<id>` fingerprint arrives
  later, `insert_if_new` deletes the placeholder in the same transaction
  before inserting, so the reported row wins. An `unavailable` write never
  replaces a reported row (plain `INSERT OR IGNORE`).

## Deferred

- Realtime `task.trace.appended` WebSocket push: `TaskTraceContext` has no
  broadcaster today; clients poll the replay cursor. Tracked for E5-6.
