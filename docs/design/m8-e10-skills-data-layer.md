# M8 E10-1: skill sources, versions, and the review gate (data layer)

This page covers the first slice of the E10 skills design ("技能来源与版本").

## Migration

The migration is `073_skill_sources_versions.sql`. The design draft reserved 074, but the highest migration on main was 072, so this one takes main's maximum plus one.

It adds:

- **`skill_sources`**: one row per skill, unique on `skill_id`.
  - `kind` is one of `url`, `git`, `local_snapshot`, `registry`.
  - Other columns: `locator`, `ref`, `subpath`, `origin_app` (`claude`, `codex`, or NULL), `last_fetched_at`, `last_commit`.
  - `trust` is `trusted` or `untrusted` and defaults to `untrusted`.
  - A `registry` row is only a pointer. `skill_registry_installs` is not migrated and stays the source of truth for registry installs.
- **`skill_versions`**: `version_no` counts up from 1 for each skill.
  - `content_hash` is a SHA-256 over SKILL.md and the whole file tree.
  - Other columns: `manifest` (JSON), `has_scripts`, `source_ref`, `created_by` (`user`, `import`, or `evolution`).
  - Triggers reject UPDATE and DELETE, so the table is append-only.
- **`skills`** gains three columns:
  - `visibility`: `private` or `team`, default `private`.
  - `team_id`
  - `review_status`: `none`, `pending`, `approved`, or `rejected`, default `none`.

## Code

- **`aionui-db`**:
  - `ISkillSourceRepository` and `SqliteSkillSourceRepository`.
  - Row types `SkillSourceRow`, `SkillVersionRow`, `SkillGovernanceRow`.
  - Helpers `evolution_auto_apply_allowed` and `review_status_is_live`.
  - `append_version` gives each new content hash the next number. If the content hash matches the latest version, it writes nothing.
- **`aionui-extension::skill_versions`**:
  - `snapshot_skill_dir` returns the hash, the manifest and `has_scripts`, and rejects symlinks.
  - `store_version` copies the skill to `skills/.versions/{skill_id}/{version_no}/` and never overwrites an existing version.
  - The skill scanner skips `.versions`.

## Hard rule: scripts need a human, and evolution never auto-applies

- Every new version with `has_scripts = 1` sets `skills.review_status = 'pending'` in the same transaction. This holds for any creator (user, import, or evolution), and also when the skill was approved before. A new script or a changed script therefore always goes back to review.
- Skill resolution (`list_for_user` and `find_by_name_for_user`) only returns skills whose `review_status` is `none` or `approved`. Pending and rejected skills are neither enabled nor injected.
- The evolution path `auto_apply_on_pass` checks `SkillEvolutionApplyPort::target_has_scripts` before it applies anything.
  - If the target ships scripts, or the answer is unknown, the proposal is left for a human review.
  - The trait default is `true`, so an adapter that cannot tell keeps auto-apply off.
  - A missing apply port also counts as `true`.

A script counts as one of the following:

- a known script extension (sh, py, js, ts, ps1, bat, …)
- a file under `scripts/` or `bin/`
- a `#!` shebang
- an executable bit

## Pending decisions: defaults adopted (marked 「采用建议默认」)

| # | Decision | Adopted default |
|---|---|---|
| D10-1 | Copy or symlink for imports and versions | Copy (no symlinks; symlinks are rejected) |
| D10-2 | Automatic update checks for url/git sources | None. Refresh is manual; `record_fetch` only stamps the time |
| D10-3 | Who approves a script skill | The owner for a private skill; a team admin for a team skill. Enforced in the service layer (E10-2); the DB only stores the state |
| D10-4 | Version retention | Keep all versions (append-only, no pruning) |
| D10-5 | git implementation | The system `git` CLI (E10-2). No `git2` dependency |
| D10-6 | Upstream rename | Keep the local name |
| D10-7 | Script skills and auto_apply | Forbidden. This is a hard rule, not a default |

## Not yet done (E10-2 and later)

- HTTP APIs for import, refresh, review, and visibility.
- The approval-permission check for D10-3.
- git and url fetchers.
- Calling `snapshot_skill_dir`, `store_version`, and `append_version` from the import and evolution-apply paths.
- Showing pending skills in the UI review queue. Today pending skills are hidden from the default list.
