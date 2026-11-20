# 3.0 C8: Issue start options and the confirmed Issue comment

Plan B row C8 (§1.7, §5): a task imported from a GitHub Issue can be started
as **fix**, **plan first** or **review only**, and can write **one** comment
back to its Issue, only after the user confirmed the exact (editable) text.

## Start options

`POST /api/task-sessions/{id}/start` takes an optional body
`{ approach?: "fix" | "plan_first" | "review_only", assistant_id? }`
(unknown fields refused). No body behaves exactly as before.

- Options are applied only while the task is fresh: draft/ready, backlog/todo,
  no conversation, no fan-out group, not queued. Later they are refused.
- `approach` is accepted only for tasks whose source is `github`.
- The approach is stored in the task's prompt state (`approach`):
  - `fix`: agent mode, no planning turn.
  - `plan_first`: plan mode with the C4 planning turn before any action.
  - `review_only`: agent mode with a review-only instruction leading the
    prompt.

### Review only is enforced by Core

A review-only task can never change the repository:

- `deliver` (open / update a PR) is refused (`review_only_task`);
- desktop adopt is refused by the worktree hook *before* any git change
  (`ADOPT_NOT_ALLOWED`, 409);
- a remote (C9) adopt approval request is refused before it is issued.

## Issue comment write-back

- `GET /api/task-sessions/{id}/issue-comment/draft` returns a suggested body
  (status, PR link, merged commit, review-only note) and the repo / number.
  It posts nothing.
- `POST /api/task-sessions/{id}/issue-comment` with
  `{ body, confirm: true }` posts **exactly** the user's (trimmed) body:
  - without `confirm: true` → `issue_comment_unconfirmed`, nothing posted;
  - body must be 1 – 65,536 characters;
  - the task must carry a `github:owner/name#N` reference;
  - once per task (`issue_comment_already_posted`, 409); a failed post
    records nothing and can be retried.
- The post runs `gh issue comment N --repo owner/name --body-file -` with the
  user's own `gh` login; the body goes through stdin, never argv. Errors are
  redacted and mapped (`github_cli_missing`, not authenticated, failed).
- The posted URL is recorded in the task's `delivery.issue_comment`.

### Who may post

- **Desktop only.** The paired-device scope denies the POST (the phone may
  read the draft); agent callers are denied both routes.
- **Audited.** `POST /api/task-sessions/{id}/issue-comment` is an audit rule
  (`task.issue_comment`), in addition to a service log line.
- Posts are serialized by their own lock, so a slow `gh` never blocks starts.

## Tests

API types (strict bodies), worktree (a refused adopt changes nothing),
conversation (start options, refusals after the first run and for non-Issue
tasks, review-only refusals for deliver and remote adopt, draft posts nothing,
confirm / exact body / once / failure / no-Issue), app (the `gh` argv and
stdin with a fake `gh`, missing CLI), auth (device and agent scopes).
