-- M8 E12-3: roles, agent run permissions and global policies.
--
-- Roles only take effect in multi-user identity modes (WebUI / AionPro). The
-- desktop single-user mode treats its one user as `owner` and never reads
-- this table (D12-6).
--
-- A user without a row is a `member`, except the built-in
-- `system_default_user` (WebUI login `admin`), which is the bootstrap owner.
-- The seed row below makes that explicit; the code applies the same rule
-- when the row is missing.
CREATE TABLE IF NOT EXISTS user_roles (
    user_id TEXT PRIMARY KEY,
    role TEXT NOT NULL CHECK (role IN ('owner', 'admin', 'member')),
    granted_by TEXT,
    granted_at INTEGER NOT NULL
);

INSERT OR IGNORE INTO user_roles (user_id, role, granted_by, granted_at)
SELECT id, 'owner', 'system', CAST(strftime('%s', 'now') AS INTEGER) * 1000
FROM users
WHERE id = 'system_default_user';

-- Which roles may start a given agent. `subject_kind` is `assistant`
-- (assistant id) or `agent_type` (conversation type / ACP backend name).
-- `allowed_roles` is a JSON array of role names; NULL means unrestricted.
-- No row means unrestricted. Rows are updated in place, never deleted.
CREATE TABLE IF NOT EXISTS agent_run_permissions (
    subject_kind TEXT NOT NULL CHECK (subject_kind IN ('assistant', 'agent_type')),
    subject_id TEXT NOT NULL,
    allowed_roles TEXT,
    updated_by TEXT,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (subject_kind, subject_id)
);

-- Global policy switches (D12-7): `no_yolo`, `mcp_strict_only`,
-- `force_sandbox` (JSON booleans) and `banned_backends` (JSON array of
-- strings). A missing row means the default: off / empty.
CREATE TABLE IF NOT EXISTS policies (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    updated_by TEXT,
    updated_at INTEGER NOT NULL
);
