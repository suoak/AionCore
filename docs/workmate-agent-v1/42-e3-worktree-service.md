# E3-1: managed git worktrees (Core)

## Status

```text
TYPE      = FEATURE (Core only; no UI yet — E3-3)
SCOPE     = M7 / E3-1 (design: E3-parallel-worktree-fanout.md §4.1, §4.3, §5, §6.1, §6.3, §7, §8)
CRATE     = crates/aionui-worktree (new)
MIGRATION = 064_task_worktrees.sql (task_worktrees, fanout_groups, task_sessions.fanout_group_id)
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
- Migration 064: once applied, older Core builds show the existing
  `database.newer_than_app` dialog (no downgrade to ≤ 0.2.20).

## Deferred to later E3 PRs

- E3-2: `POST /api/conversations` `worktree_id`, protected extra keys,
  workspace lock, delete hook → `unbound`, `ScmRepository.managed_worktree_id`,
  `diff-stat` / `diff` routes, skill-isolation regression in worktrees,
  `task_trace_events` (`worktree.created`) when a TaskSession is linked.
- E3-3 (UI): Worktree toggle, Start-from picker, progress / cancel / retry,
  delete dialog, preserved-branch review.
- E3-4: `FanoutService` and `/api/fanout-groups*` (table already in 064).
- E3-7 (M8): remote `fetch` before create, `size_bytes`, disk quotas, Windows
  deepest-path pre-check.
