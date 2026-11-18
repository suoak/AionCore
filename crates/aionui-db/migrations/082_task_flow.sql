-- 3.0 unified task flow (plan B C1): fields a task needs to start on its own.
--
-- Only ADD COLUMN, all nullable or defaulted; no table rebuild, no change to the
-- board_status / status enums. The board lane is derived at read time.
-- Older Cores ignore the columns, so a downgrade keeps working.
-- fanout_group_id already exists (066).
ALTER TABLE task_sessions ADD COLUMN assistant_id TEXT;
-- Composer state (text, file refs, skills, model options) as a JSON object.
ALTER TABLE task_sessions ADD COLUMN prompt_json TEXT
    CHECK (prompt_json IS NULL OR (json_valid(prompt_json) AND json_type(prompt_json) = 'object'));
ALTER TABLE task_sessions ADD COLUMN repo_path TEXT;
ALTER TABLE task_sessions ADD COLUMN base_ref TEXT;
ALTER TABLE task_sessions ADD COLUMN plan_first INTEGER NOT NULL DEFAULT 0 CHECK (plan_first IN (0, 1));
-- NULL on rows created before this migration; readers infer it from
-- external_ref / conversation_id.
ALTER TABLE task_sessions ADD COLUMN source TEXT
    CHECK (source IS NULL OR source IN ('manual', 'conversation', 'github', 'cron', 'agent'));
-- Message id, cron job id, … depending on source.
ALTER TABLE task_sessions ADD COLUMN source_ref TEXT;
-- Adopt result or PR link, written by delivery (C3).
ALTER TABLE task_sessions ADD COLUMN delivery_json TEXT
    CHECK (delivery_json IS NULL OR (json_valid(delivery_json) AND json_type(delivery_json) = 'object'));
-- Set while the task waits for a run slot (C2).
ALTER TABLE task_sessions ADD COLUMN queued_at INTEGER;
