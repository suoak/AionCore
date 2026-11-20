# 3.0 C6: scheduled jobs that run as tasks

Plan B row C6: a cron job may run as a **task** instead of a conversation
turn. Each fire queues a backlog task (source `cron`) that goes through the
normal task flow and **stops at review**; nothing is merged without a human.

## Shape

- `run_as`: `conversation` (default, unchanged) or `task`.
- `task`: `{ start, repo_path?, base_ref? }` (unknown fields refused).
  Stored as one JSON object in `cron_jobs.task_options` (migration 085);
  `NULL` means conversation, so existing jobs are untouched.
- `POST /api/cron/jobs/{id}/run-now` on a task job runs synchronously and
  returns `task_session_id`.

## What a fire does

1. If the job skips overlaps and an open (not done / cancelled) task with
   `source = cron, source_ref = job id` exists, the fire is **skipped**.
2. Otherwise a task is created (`mode = agent`, status ready, backlog), with
   the job message as its text and the job's assistant, repo and base ref.
3. With `start = true` the task is started through the normal start path; a
   refused start is reported as a job error, the task stays in the backlog.

The run ends where every task run ends: in review. Review gates, the agent
review gate and adopt (always a human action) apply unchanged.

## Governance

Refused for `run_as = task`:

- creation or edits by an agent caller, or a job created by an agent;
- `autopilot` (a conversation feature; a task always stops at review);
- an existing-conversation execution mode;
- an empty message or one over 20,000 characters;
- a relative or overlong `repo_path`, a `base_ref` without `repo_path`, or an
  unsafe `base_ref`;
- `start = true` without an assistant;
- `task` options on a job that does not run as a task.

Switching a job between modes is an ordinary update and is audited by the
existing `config.cron.update` rule.

## Tests

API types (strict input), DB round-trip and object check, cron service
(validation, overlap skip, run-now), conversation (`enqueue_cron_task`), and
end-to-end: run-now of a task job creates a backlog task and no conversation
turn; strict input is refused over HTTP.
