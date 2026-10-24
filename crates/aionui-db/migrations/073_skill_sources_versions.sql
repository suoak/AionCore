-- M8 E10-1: skill provenance, append-only versions, and the review gate.
-- See docs/design/m8-e10-skills.md. The design draft reserved 074; main's
-- highest migration was 072, so this is 073.
--
-- Hard rule (D10-7): a skill that ships scripts never goes live without a
-- human approval. A new version of a script-bearing skill sets
-- skills.review_status back to 'pending', and the skill resolver hides
-- 'pending' and 'rejected' skills. Evolution must never auto-apply to them.

-- Where a skill came from. At most one source per skill; `registry` rows are a
-- pointer only, and skill_registry_installs stays the source of truth for
-- registry installs.
CREATE TABLE skill_sources (
    id              TEXT    PRIMARY KEY NOT NULL,
    user_id         TEXT    REFERENCES users(id),
    skill_id        TEXT    NOT NULL UNIQUE REFERENCES skills(id),
    kind            TEXT    NOT NULL
                        CHECK (kind IN ('url', 'git', 'local_snapshot', 'registry')),
    locator         TEXT    NOT NULL,
    ref             TEXT,
    subpath         TEXT,
    origin_app      TEXT    CHECK (origin_app IS NULL OR origin_app IN ('claude', 'codex')),
    last_fetched_at INTEGER,
    last_commit     TEXT,
    trust           TEXT    NOT NULL DEFAULT 'untrusted'
                        CHECK (trust IN ('trusted', 'untrusted')),
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE INDEX idx_skill_sources_user ON skill_sources(user_id);

-- One row per stored snapshot. Content lives append-only under
-- skills/.versions/{skill_id}/{version_no}/. Rows are never updated or
-- deleted (D10-4: keep every version).
CREATE TABLE skill_versions (
    id           TEXT    PRIMARY KEY NOT NULL,
    skill_id     TEXT    NOT NULL REFERENCES skills(id),
    version_no   INTEGER NOT NULL CHECK (version_no >= 1),
    content_hash TEXT    NOT NULL,
    manifest     TEXT    NOT NULL DEFAULT '{}',
    has_scripts  INTEGER NOT NULL DEFAULT 0 CHECK (has_scripts IN (0, 1)),
    source_ref   TEXT,
    created_by   TEXT    NOT NULL
                     CHECK (created_by IN ('user', 'import', 'evolution')),
    created_at   INTEGER NOT NULL,
    UNIQUE (skill_id, version_no)
);

CREATE TRIGGER skill_versions_no_update
BEFORE UPDATE ON skill_versions
BEGIN
    SELECT RAISE(ABORT, 'skill_versions is append-only');
END;

CREATE TRIGGER skill_versions_no_delete
BEFORE DELETE ON skill_versions
BEGIN
    SELECT RAISE(ABORT, 'skill_versions is append-only');
END;

ALTER TABLE skills ADD COLUMN visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (visibility IN ('private', 'team'));
ALTER TABLE skills ADD COLUMN team_id TEXT;
ALTER TABLE skills ADD COLUMN review_status TEXT NOT NULL DEFAULT 'none'
    CHECK (review_status IN ('none', 'pending', 'approved', 'rejected'));
