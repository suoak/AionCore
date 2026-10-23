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
- Local estimates (`EstimatedSegment`) only fill keys the provider did not report and are always `estimated`. If any estimate is involved, `other` is `estimated` too.
- Labels are stripped of control characters and capped at 64 chars. At most 32 segments are returned.
- User-scoped through `get_usage` → `ensure_owned_conversation`; another user gets an error.

## Agent scope

`context-breakdown` is in `AGENT_DENIED_CONVERSATION_SUBPATHS`. Agent tokens are denied even for their own or `@@`-granted conversation, for all methods. Guard test: `context_breakdown_route_is_denied_even_for_own_conversation`. Sibling reads such as `/usage` keep the existing per-conversation scoping.

## Not in this PR (follow-ups)

- **Writer for `breakdown`.** ACP `UsageUpdate` carries no categories today. The Claude engine's `ContextUsage.categories` (aionui-session `claude_conn`) is an ephemeral `SessionInfo`. Persisting it into `context_usage.breakdown` is the next step. Until then the ring shows `used` as a single reported `other` slice: honest, not fabricated.
- **Local estimators for skills and tool schemas.** The hook is `EstimatedSegment`, and the service currently passes none.
- **UI ring (AionUi).**
