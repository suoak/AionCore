-- 3.0 C7: `@assistant` delegation from a conversation.
--
-- A grant is created only when the user explicitly picks an assistant in the
-- `@` popover of that conversation; the agent can delegate to granted
-- assistants only. A delegation runs one child conversation (cold start, the
-- assistant's own permission mode). Depth 1: a child cannot delegate.
CREATE TABLE IF NOT EXISTS delegation_grants (
    conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    assistant_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, assistant_id)
);

CREATE TABLE IF NOT EXISTS delegations (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    parent_conversation_id TEXT NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    child_conversation_id TEXT REFERENCES conversations(id) ON DELETE SET NULL,
    assistant_id TEXT NOT NULL,
    task_text TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('starting', 'running', 'completed', 'failed')),
    error TEXT,
    -- The child's final reply when it completed.
    result_text TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_delegations_parent ON delegations(parent_conversation_id, status);
CREATE UNIQUE INDEX IF NOT EXISTS idx_delegations_child ON delegations(child_conversation_id)
    WHERE child_conversation_id IS NOT NULL;
