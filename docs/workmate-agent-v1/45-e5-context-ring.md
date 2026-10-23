# E5-3 Context ring breakdown (Core)

Design: `m7-design/E5-replay-cost-context-inbox.md` §5 (E5-3). No migration, no new table.

## Endpoint

`GET /api/conversations/{id}/context-breakdown` → `ContextBreakdownResponse`

| field | meaning |
|---|---|
| `used` / `size` | from the live task usage or the persisted `acp_session.session_config.runtime.context_usage` (`size`, falling back to `max`) |
| `source` | source of `used`: `reported`, or `unavailable` when no snapshot / no `used` |
| `segments[]` | `{key, label, tokens, source}`; `source ∈ reported / estimated / unavailable` |

Invariants:

- When `used` is present, segments sum to **exactly** `used`. The remainder is one `other` segment.
- Provider-reported categories (optional `context_usage.breakdown: [{key?, name|label, tokens}]`) come first and win over local estimates with the same key.
- Capacity pseudo-categories (`free_space`, `autocompact_buffer`, `reserved`, `free`) are dropped because they are not context content. Zero-token items are dropped. Duplicate keys are merged.
- Every segment is clamped to the remaining budget, so a provider over-report or a large estimate never makes the ring exceed `used`.
- Local estimates (`EstimatedSegment`) are used **only when the provider reported no categories** (claude's own categories already include system/MCP tools and skills; mixing would double-count), and are always `estimated`. If any estimate is involved, `other` is `estimated` too.
- Labels are stripped of control characters and capped at 64 chars. At most 32 segments are returned.
- User-scoped through `get_usage` → `ensure_owned_conversation`; another user gets an error.

## Agent scope

`context-breakdown` is in `AGENT_DENIED_CONVERSATION_SUBPATHS`. Agent tokens are denied even for their own or `@@`-granted conversation, for all methods. Guard test: `context_breakdown_route_is_denied_even_for_own_conversation`. Sibling reads such as `/usage` keep the existing per-conversation scoping.

## Breakdown writer (claude)

- After the first terminal `TurnResult` of each turn, the event pump fires a `Command::QuerySessionInfo{ContextUsage}` when the backend advertises `supported_commands.query_session_info` (claude only). The pump holds only a `Weak` backend handle, so it never keeps the child CLI alive. The query is fire-and-forget; failures are debug-logged.
- The `SessionEvent::SessionInfo{context_usage}` reply is persisted by `persist_side_effects` → `persist_context_breakdown`: it replaces `context_usage.breakdown: [{name, tokens}]` (≤32 items), writes `breakdown_used`, and seeds `used`/`size` only when absent. The turn's own `UsageDelta` stays the headline `used`. An empty category list never blanks a stored breakdown.

## Local estimators

When no provider categories exist, the service derives estimates from the **live** task's reported tool surface (`AgentInstance::tool_surface`; never synthesized from config, a reaped task yields none):

- `tools`: non-deferred builtin + MCP entries × 300 tokens (`EST_TOKENS_PER_TOOL_SCHEMA`); falls back to `tools[]` count when entries are missing.
- `skills`: skill entries × 100 tokens (`EST_TOKENS_PER_SKILL_LISTING`).

Both are coarse constants (the surface carries names, not schemas) and always marked `estimated`.

## Not in this PR (follow-ups)

- ACP / non-claude engines report no categories; their ring relies on estimates plus `other`.
- **UI ring (AionUi)** — separate PR.
