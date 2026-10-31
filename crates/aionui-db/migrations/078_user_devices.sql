-- M8 E9-1: paired mobile devices with per-device opaque tokens.
-- Numbered as main's highest (077 cron autopilot) + 1; the E9 design reserved
-- 077, which autopilot took.
--
-- token_hash is the lowercase hex SHA-256 of the opaque device token; the
-- plaintext token is returned to the device once at pairing and never stored.
-- scopes is a JSON array of scope names (default ["mobile_default"]).
-- push_kind / push_target are storage only in E9-1 (IM push lands in E9-6;
-- APNs/FCM are not implemented in M8, D9-3).
-- revoked_by: 'user' (desktop revoke), 'inactivity' (unused for 90 days, D9-2).
CREATE TABLE IF NOT EXISTS user_devices (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    name TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('ios', 'android', 'web')),
    token_hash TEXT NOT NULL UNIQUE,
    scopes TEXT NOT NULL DEFAULT '["mobile_default"]',
    push_kind TEXT NOT NULL DEFAULT 'none' CHECK (push_kind IN ('none', 'im', 'apns', 'fcm')),
    push_target TEXT,
    last_seen_at INTEGER,
    created_at INTEGER NOT NULL,
    revoked_at INTEGER,
    revoked_by TEXT
);
CREATE INDEX IF NOT EXISTS idx_user_devices_user_active ON user_devices (user_id, revoked_at);
