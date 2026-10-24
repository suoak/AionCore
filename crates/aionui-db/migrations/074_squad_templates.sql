-- M8 E7-1: Squad templates (a Leader + members picked from team-selectable
-- Assistants). Members are stored as references only (D7-2): the runtime
-- always re-reads the Assistant config by id at instantiation time.
--
-- Numbered as main's highest (073) + 1; the design doc's "073" reservation
-- was taken by E10-1.
CREATE TABLE IF NOT EXISTS squad_templates (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    leader_assistant_id TEXT NOT NULL,
    -- D7-1: the Leader only routes by default.
    leader_mode TEXT NOT NULL DEFAULT 'route_only'
        CHECK (leader_mode IN ('route_only', 'route_and_work')),
    -- [{assistant_id, role_title, role_description, max_parallel}]
    members TEXT NOT NULL DEFAULT '[]'
        CHECK (json_valid(members) AND json_type(members) = 'array'),
    -- 'team' is reserved for E12; the API refuses it until team roles land.
    visibility TEXT NOT NULL DEFAULT 'private' CHECK (visibility IN ('private', 'team')),
    team_id TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    -- Soft delete only; rows are never physically removed by the API.
    deleted_at INTEGER
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_squad_templates_user_name
    ON squad_templates (user_id, name) WHERE deleted_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_squad_templates_user_updated
    ON squad_templates (user_id, updated_at);

-- A Team instantiated from a Squad template keeps a back-reference
-- (NULL = ordinary Team, behaviour unchanged per D7-6).
ALTER TABLE teams ADD COLUMN squad_template_id TEXT;
CREATE INDEX IF NOT EXISTS idx_teams_squad_template
    ON teams (squad_template_id) WHERE squad_template_id IS NOT NULL;
