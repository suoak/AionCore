-- M8 E7-6: a Squad instance may be started for one E6 work item
-- (task_sessions.id). The link lets board cards show the instance's
-- squad_run state. NULL = not started from a work item (behaviour unchanged).
--
-- Numbered as main's highest (074) + 1; M8-plan's "075" reservation for E8
-- moves to the next free number.
ALTER TABLE teams ADD COLUMN work_item_id TEXT;
CREATE INDEX IF NOT EXISTS idx_teams_work_item
    ON teams (user_id, work_item_id, created_at) WHERE work_item_id IS NOT NULL;
