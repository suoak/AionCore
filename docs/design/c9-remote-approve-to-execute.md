# C9 — Remote approve-to-execute (plan B §5.4, D8)

A decision taken on a paired phone or in a paired IM chat now *executes*
(plan/goal run, or the fan-out adopt), instead of only being recorded. Because a
remote answer can start work, every request is bound, one-shot, expiring,
limited to trusted identities and audited.

## Request (`approval_requests`, migration 084)

| field | meaning |
| --- | --- |
| `task_session_id`, `kind` | `plan`, `goal` or `adopt`; at most one pending per task and kind |
| `approval_id`, `artifact_id`, `artifact_hash` | plan/goal: the pending `task_approvals` row and the exact artifact (sha256) |
| `fanout_group_id`, `slot`, `strategy`, `target_branch` | adopt: fixed by the desktop when the request is issued |
| `diff_digest` / `digest` | what the approver saw; Core recomputes it before executing |
| `requested_at`, `expires_at` | lifetime: default 30 min, per-user setting 1..1440 min |
| `status` | `pending → approved/rejected/expired/invalidated` (conditional update, once) |
| `decided_by` | `desktop`, `device:<id>` or `im:<platform>:<identity>` |
| `result_ref` / `error` | `run:<id>` / `commit:<sha>`, or the execution error |

The adopt digest is `sha256("workmate-adopt-digest-v1", base, branch tip,
strategy, target, target head, full binary diff of the worktree, sorted
untracked files with their content hashes)`, so any edit, new file, strategy
or target move changes it. The adopt re-checks it under the adopt lock.

## Decision order (`decide_remote`)

1. unknown request → 404 (audited `identity_denied`)
2. identity: device must be paired and unrevoked; IM identity must be bound to
   the user (`get_user_by_platform`) **and** registered as an approver → 403
3. E12 run permission for the task's assistant → 403
4. not pending → 409 `approval_already_decided` (audited `replay`)
5. expired → marked expired, back in "needs you", 409 `approval_expired`
6. digest echo differs → 409 `approval_digest_mismatch`, request kept
7. approve: recompute; changed → invalidated `diff_changed`, 409
8. conditional claim (`WHERE status='pending' AND expires_at > now`); the loser gets 409
9. execute exactly the stored request; the result or error is stored

The remote body is `{decision, digest, comment?}` with
`deny_unknown_fields`: no plan content, agent, permission mode, strategy or
target can be passed. Issuing requests, the lifetime setting and approver
bindings are desktop-only (device scope and agent scope both deny them).

## Invalidation

New plan version (`superseded`), task end (`task_ended`), adopted /
conflicted / follow-up for adopt requests, a changed diff (`diff_changed`) and
expiry (`expired`, 30 s sweep). A desktop decision settles the matching remote
request (`decided_by = desktop`).

## IM

Telegram-style button card (`apr.d`, request id 16 hex, 24-hex digest prefix,
fits 64-byte callback data). Only the button presser counts (`cb.from`).
IM decisions are recorded in `audit_events` as `actor_id im:<platform>:<identity>`,
`via mobile`.

## Routes

- `POST/GET /api/task-sessions/{id}/approval-requests` (desktop issues)
- `GET /api/approval-requests`, `GET /api/approval-requests/{id}`
- `POST /api/approval-requests/{id}/decision` (paired device only)
- `GET/PUT /api/remote-approval/settings`
- `GET/POST /api/remote-approval/approvers`, `DELETE /api/remote-approval/approvers/{platform}/{identity}`
