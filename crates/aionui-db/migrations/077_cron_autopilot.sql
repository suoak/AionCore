-- M8 E8-6: cron Autopilot columns (E8-batch-autopilot.md §2.3).
-- Numbered 077 because 076 is batch_jobs (see 076 header); E9 user_devices → 078+.
--
-- D8-8: existing jobs are NOT opted in — every column defaults to "off", so
-- legacy cron jobs keep their current behaviour until the user enables
-- Autopilot explicitly.
--
-- autopilot          1 = Autopilot guardrails apply (Ask by default, no
--                    self-overlap, completion/failure notifications).
-- autopilot_yolo     1 = the user explicitly confirmed full-auto for this
--                    Autopilot job (user credential only; never for
--                    created_by = 'agent'). Ignored when autopilot = 0.
-- notify_channel_id  IM target for run summaries: an assistant_sessions.id
--                    owned by the job's user (resolved to platform + chat_id
--                    at send time). NULL = no IM summary.

ALTER TABLE cron_jobs ADD COLUMN autopilot INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cron_jobs ADD COLUMN autopilot_yolo INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cron_jobs ADD COLUMN notify_channel_id TEXT;
