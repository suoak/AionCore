-- M7 E1 tail ③: persist `@@` cross-session delivery grants across restarts.
-- Additive only. Rows are written by the server when the user sends with `@@`
-- (and for the reply edge of a delivery the user authorized); agents cannot
-- create them. Deleting a conversation removes its grants in both directions.

CREATE TABLE IF NOT EXISTS session_mention_grants (
    user_id TEXT NOT NULL,
    from_conversation_id TEXT NOT NULL,
    to_conversation_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, from_conversation_id, to_conversation_id)
);

CREATE INDEX IF NOT EXISTS idx_session_mention_grants_to
    ON session_mention_grants (user_id, to_conversation_id);
