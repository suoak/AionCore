-- Extensible workflow contract for Agent Center definitions.
-- MVP uses Start -> Agent -> Output, while JSON keeps future node types additive.
ALTER TABLE assistant_agent_center
    ADD COLUMN workflow_definition TEXT NOT NULL DEFAULT '{}';
