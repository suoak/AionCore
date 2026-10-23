-- M7 E5-5 follow-up: a user cancel must stop the whole retry chain.
--
-- Between a failed attempt and the start of its retry there is no live turn to
-- cancel, so the request is recorded on the task instead. create_retry_run
-- refuses to start another attempt while it is set; a fresh, user-initiated
-- execution (create_run) clears it.
ALTER TABLE task_sessions ADD COLUMN cancel_requested_at INTEGER;
