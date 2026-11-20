-- 3.0 C6 (plan B §3, §7): cron "run as task".
-- Numbered 085 because 084 is approval_requests (C9).
--
-- task_options  NULL = the job runs as a conversation (every existing job).
--               A JSON object = each occurrence creates a task on the board
--               (source = cron, source_ref = job id) instead:
--               {"start": bool, "repo_path": "/abs"?, "base_ref": "main"?}.
--               A started task runs under the task start rules (E12 run
--               permission, run slots) and always stops at review.
ALTER TABLE cron_jobs ADD COLUMN task_options TEXT
    CHECK (task_options IS NULL OR (json_valid(task_options) AND json_type(task_options) = 'object'));
