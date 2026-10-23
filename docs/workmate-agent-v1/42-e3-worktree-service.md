# E3-1 / E3-2: managed git worktrees (Core)

## Status

```text
TYPE      = FEATURE (Core only; no UI yet — E3-3)
SCOPE     = M7 / E3-1 (design: E3-parallel-worktree-fanout.md §4.1, §4.3, §5, §6.1, §6.3, §7, §8)
CRATE     = crates/aionui-worktree (new)
MIGRATION = 066_task_worktrees.sql (task_worktrees, fanout_groups, task_sessions.fanout_group_id;
            designed as 064, renumbered after E4-1 landed 065 so apply order = version order)
SCM       = unchanged: scm/* protocol and git_provider are not touched
```

## What ships

- `GitCli` — system `git` runner used for every worktree write:
  - argv only, never a shell string; revs pass `validate_rev_text`, new branch
    names pass `validate_branch_name` **and** `git check-ref-format --branch`;
  - environment scrubbed from the parent: `AIONUI_*`, `AIONCORE_*`,
    `AION_MCP_*`, `CSBU_WORKMATE_*`, anything containing `RUNTIME_TOKEN` /
    `LOCAL_SECRET` / `BOOTSTRAP_SECRET`, repository redirects (`GIT_DIR`,
    `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_COMMON_DIR`, …) and config
    injection (`GIT_CONFIG_PARAMETERS`, `GIT_CONFIG_COUNT/KEY_n/VALUE_n`); then
    `GIT_TERMINAL_PROMPT=0`, `GCM_INTERACTIVE=never`;
  - own process group (Unix) / `CREATE_NO_WINDOW` (Windows); timeout and
    cancellation kill the whole tree; Windows adds `-c core.longpaths=true`;
  - version probe, minimum git 2.31 (`GIT_NOT_AVAILABLE` / `GIT_TOO_OLD`).
- `WorktreeService` — create / cancel / retry / remove / list / get / prune /
  recover_on_boot. Worktrees live in `{data_dir}/worktrees/<repo8>/<id8>/`
  (never inside the repository), checked by canonical containment.
- Routes (user identity only; agent tokens → 403 `AGENT_SCOPE_DENIED`):

  | Method | Path | Notes |
  |---|---|---|
  | GET | `/api/worktrees/capabilities` | git available / version / reason |
  | POST | `/api/worktrees` | `{repo_path, base_ref?, branch?, title?, task_session_id?}` → 202 `creating` |
  | GET | `/api/worktrees?repo_path=&status=&include_removed=` | managed worktrees only |
  | GET | `/api/worktrees/{id}` | adds `ahead` / `behind` vs `base_commit` |
  | POST | `/api/worktrees/{id}/cancel` | only while `creating` |
  | POST | `/api/worktrees/{id}/retry` | only `failed` → 202 `creating` |
  | DELETE | `/api/worktrees/{id}?branch=auto\|keep\|force&force_dir=` | 202 `removing`; dirty without `force_dir` → 409 `WORKTREE_DIRTY` |
  | POST | `/api/worktrees/prune` | records whose directory is gone → `removed` |

- Events: `worktree.progress {worktree_id, phase, percent?, message}`
  (250 ms throttle per worktree; phase change / 100 % always sent) and
  `worktree.changed {worktree_id, status}`, both carrying `user_id`.

## State machine and safety rules

- `creating → ready → (unbound) → removing → removed`; create error → `failed`
  (code persisted: `CANCELLED`, `INTERRUPTED`, `BRANCH_EXISTS`, `GIT_TIMEOUT`, …);
  directory deleted outside WorkMate → `orphaned` (detected on list/get).
- A failed **removal** restores the previous status with `error_code =
  REMOVE_FAILED`, so `failed` always means "creation did not finish".
- Cancel / failure rollback removes only the managed path and deletes the
  branch only if this attempt created it and it still points at `base_commit`.
- Retry first checks whether the worktree is actually healthy (registered,
  not `locked: initializing`, branch checked out) and adopts it instead of
  rebuilding — an interrupted process never costs the user their files.
- Branch policy on remove: `auto` deletes only when the branch tip is an
  ancestor of `base_commit`, the re-resolved `base_ref`, or the repo `HEAD`;
  otherwise it is kept and `branch_preserved = true`. `keep` always keeps,
  `force` always deletes. Prune and boot recovery never delete branches.
- Stale admin entries are dropped precisely (only the `.git/worktrees/<name>`
  whose `gitdir` points at the managed path) instead of a repo-wide
  `git worktree prune`, so the user's own worktrees are left alone.
- Boot: `creating → failed (INTERRUPTED)`; `removing → removed` if the
  directory is gone, else `failed (INTERRUPTED)` (retry re-adopts it).

## Notes for release

- WorkMate now needs a system `git` (≥ 2.31) for worktree features. The SCM
  panel keeps working without it (git2, read-only).
- `git worktree add` runs the repository's `post-checkout` hook, exactly as if
  the user ran git themselves.
- Migration 066: once applied, older Core builds show the existing
  `database.newer_than_app` dialog (no downgrade to ≤ 0.2.20).

## E3-2: conversations in a worktree (Core)

- `POST /api/conversations` takes a top-level `worktree_id`. The worktree must
  belong to the caller, be `ready` (or `unbound`, i.e. its previous
  conversation was deleted) and hold no conversation; the server then writes
  `extra.workspace = worktree.path`, `extra.worktree_id`, and
  `extra.fanout_group_id` / `extra.fanout_slot` when set. A client workspace is
  overridden; no temp workspace is created; project bind runs as usual.
  Errors: 404 `WORKTREE_NOT_FOUND`, 409 `WORKTREE_INVALID_STATE` /
  `WORKTREE_ALREADY_BOUND`, 400 when the worktree service is not wired.
- Binding is a compare-and-set on `task_worktrees.conversation_id` after the
  conversation row exists (FK). Losing the race deletes the new conversation
  and returns 409, so no half-bound conversation is left behind.
- Protected keys `worktree_id`, `fanout_group_id`, `fanout_slot`: stripped from
  every client create body (forged values never bind), rejected on PATCH
  (400). Echoing the stored value back is allowed, like the team keys.
- Workspace lock: when `extra.worktree_id` is set, `PATCH` and the internal
  `update_extra` both refuse to change `extra.workspace`
  (400 `WORKSPACE_LOCKED`). Clone and fork never inherit the binding (clone
  drops `worktree_id`; both strip the keys on create).
- Delete: an `OnConversationDelete` hook (runs before the row delete) moves
  every `ready` worktree held by the conversation to `unbound`, broadcasts
  `worktree.changed`, and never touches the directory or branch.
  `conversation_id` is left for the FK (`ON DELETE SET NULL`) to clear when the
  delete commits. Failure fallback: if the delete does not commit, the row is
  `unbound` but still names a live conversation; reads more than 30 s after
  the unbind (grace for the in-flight delete) flip it back to `ready`
  (`worktree.rebound` log). Hook errors are logged, never fail the delete.
- An `unbound` worktree can be bound again by a new conversation. A `ready`
  worktree that is bound cannot be removed (409 `WORKTREE_INVALID_STATE`);
  delete the conversation first.
- `GET /api/worktrees/{id}/diff-stat`: committed + staged + unstaged +
  untracked changes against `base_commit` (git2; ignored files excluded;
  renames detected; binary flagged; capped at 2000 listed files with totals
  still complete). `GET /api/worktrees/{id}/diff?path=`: one file's unified
  diff (untracked against `/dev/null`), `unchanged` when equal, cut at 1 MiB.
  Paths must be plain relative paths (no `..`, absolute, or `.git/`);
  pathspec magic is not interpreted. git2 never runs repository-configured
  diff / textconv / filter drivers. Both are user-only (agent tokens 403).
- `ScmRepository.managed_worktree_id` (optional, omitted when absent):
  `ScmRuntime` back-fills it during discover through
  `IManagedWorktreeLookup`, which the app implements by exact path match
  against live `task_worktrees` rows. The git provider and `scm/*` frames are
  otherwise unchanged.
- TaskSession: `task_session_id` on worktree create is ownership-checked
  (400 otherwise). `worktree.created`, `worktree.bound`, `worktree.unbound`
  and `worktree.removed` are appended to `task_trace_events` on the session's
  latest run; with no run yet there is nowhere to attach them (the table needs
  `run_id`), so they are skipped (structured `tracing` logs always exist).
- Skill isolation regression: a linked worktree checked out on a branch that
  ships `.csbu-workmate/skills`, `.aionrs/skills` and a malicious
  `.aionrs.toml` loads none of them (aionrs agent test).

## Deferred to later E3 PRs

- E3-3 (UI): Worktree toggle, Start-from picker, progress / cancel / retry,
  delete dialog, preserved-branch review, `managed_worktree_id` label in the
  SCM tree, diff-stat view.
- E3-4: `FanoutService` and `/api/fanout-groups*` (table already in 066);
  `fanout.adopted` trace event; fan-out conversations pass `worktree_id` per
  slot.
- E3-7 (M8): remote `fetch` before create, `size_bytes`, disk quotas, Windows
  deepest-path pre-check.
