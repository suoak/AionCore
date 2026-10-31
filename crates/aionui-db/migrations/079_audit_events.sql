-- M8 E12-2: append-only audit ledger.
-- The E12 design reserved 078; 077/078 were taken by autopilot and
-- user_devices, so the ledger is 079 (roles/policies follow at 080).
--
-- One row per audited action: who (actor_kind/actor_id, user_id), how (via),
-- what (action, target, summary, redacted diff), and who approved it.
-- Secret values never reach this table: the writer replaces them with a
-- fingerprint before insert.
--
-- Integrity:
-- - UPDATE and DELETE are refused by triggers (the ledger is never edited or
--   pruned; archive by exporting).
-- - hash = SHA-256(prev_hash, row content); prev_hash is the previous row's
--   hash (64 zeros for the first row). prev_hash is UNIQUE, so two writers
--   racing on the same tail cannot fork the chain: the loser retries.
CREATE TABLE IF NOT EXISTS audit_events (
    seq INTEGER PRIMARY KEY AUTOINCREMENT,
    id TEXT NOT NULL UNIQUE,
    ts INTEGER NOT NULL,
    user_id TEXT,
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('user', 'agent', 'device', 'system')),
    actor_id TEXT,
    via TEXT NOT NULL CHECK (via IN ('desktop', 'webui', 'mobile', 'helper', 'cron', 'batch', 'system')),
    action TEXT NOT NULL,
    target_kind TEXT,
    target_id TEXT,
    summary TEXT NOT NULL DEFAULT '',
    diff TEXT,
    approver_id TEXT,
    approval_id TEXT,
    request_fingerprint TEXT,
    prev_hash TEXT NOT NULL UNIQUE,
    hash TEXT NOT NULL UNIQUE
);
CREATE INDEX IF NOT EXISTS idx_audit_events_ts ON audit_events (ts);
CREATE INDEX IF NOT EXISTS idx_audit_events_user_ts ON audit_events (user_id, ts);
CREATE INDEX IF NOT EXISTS idx_audit_events_action_ts ON audit_events (action, ts);
CREATE INDEX IF NOT EXISTS idx_audit_events_actor_ts ON audit_events (actor_id, ts);

CREATE TRIGGER IF NOT EXISTS audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit_events is append-only');
END;

CREATE TRIGGER IF NOT EXISTS audit_events_no_delete
BEFORE DELETE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit_events is append-only');
END;
