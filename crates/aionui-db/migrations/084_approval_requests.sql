-- 3.0 C9 (plan B §5.4, D8): remote approve-to-execute.
--
-- A paired phone or a bound IM identity may approve a Plan/Goal approval or
-- an adopt, and the approval executes at once. Every remote decision is
-- bound to one exact request:
-- - `digest` is the value the approver must echo back: the plan artifact's
--   sha256 (`artifact_hash`) or, for an adopt, `diff_digest` (sha256 over the
--   base commit, branch head, target branch and its head, and the normalized
--   worktree diff). Core recomputes it before acting; a mismatch rejects.
-- - One-shot: `pending → approved|rejected` is a conditional update
--   (`WHERE status = 'pending' AND expires_at > now`); a second decision is 409.
-- - `expires_at` defaults to 30 minutes (per-user setting, at most 24 hours).
-- - `decided_by`: `desktop`, `device:<id>` or `im:<platform>:<identity>`.
-- - The approver cannot change anything but the decision: strategy and target
--   are fixed here when the request is issued.
CREATE TABLE IF NOT EXISTS approval_requests (
    id TEXT PRIMARY KEY NOT NULL,
    user_id TEXT NOT NULL,
    task_session_id TEXT NOT NULL REFERENCES task_sessions(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('plan', 'goal', 'adopt')),
    approval_id TEXT REFERENCES task_approvals(id) ON DELETE CASCADE,
    artifact_id TEXT,
    artifact_hash TEXT,
    fanout_group_id TEXT,
    slot TEXT,
    diff_digest TEXT,
    target_branch TEXT,
    strategy TEXT,
    digest TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'approved', 'rejected', 'expired', 'invalidated')),
    requested_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    decided_at INTEGER,
    decided_by TEXT,
    -- Why a pending request stopped being answerable without a decision
    -- (superseded, cancelled, expired, diff_changed).
    reason TEXT,
    -- What the approval produced: the run id or the adopt commit.
    result_ref TEXT,
    error TEXT
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_approval_requests_one_pending
    ON approval_requests(task_session_id, kind) WHERE status = 'pending';
CREATE INDEX IF NOT EXISTS idx_approval_requests_user_status
    ON approval_requests(user_id, status, expires_at);
CREATE INDEX IF NOT EXISTS idx_approval_requests_approval
    ON approval_requests(approval_id);

-- IM identities the user bound as "this is me" for remote approvals. The
-- identity must also stay paired in the channel (`assistant_users`).
CREATE TABLE IF NOT EXISTS remote_approvers (
    user_id TEXT NOT NULL,
    platform_type TEXT NOT NULL,
    platform_user_id TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (user_id, platform_type, platform_user_id)
);

CREATE TABLE IF NOT EXISTS remote_approval_settings (
    user_id TEXT PRIMARY KEY NOT NULL,
    ttl_minutes INTEGER NOT NULL CHECK (ttl_minutes BETWEEN 1 AND 1440),
    updated_at INTEGER NOT NULL
);

-- Pending Plan/Goal approvals from before C9 get a request with the default
-- 30-minute lifetime, so a phone can answer them under the same rules.
INSERT INTO approval_requests (id, user_id, task_session_id, kind, approval_id, artifact_id, artifact_hash,
    digest, status, requested_at, expires_at)
SELECT lower(hex(randomblob(16))), s.user_id, a.task_session_id, a.approval_type, a.id, a.artifact_id,
    a.artifact_hash, a.artifact_hash, 'pending', a.requested_at,
    CAST(strftime('%s', 'now') AS INTEGER) * 1000 + 1800000
FROM task_approvals a JOIN task_sessions s ON s.id = a.task_session_id
WHERE a.status = 'pending'
  AND NOT EXISTS (SELECT 1 FROM approval_requests r WHERE r.approval_id = a.id);
