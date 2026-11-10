# vendor/

Third-party crates carried in-repo so a security fix does not have to wait for an upstream release.
They are **not** workspace members (see `exclude` in the root `Cargo.toml`), so they are not
formatted, linted or tested as first-party code, and they are wired in only through
`[patch.crates-io]`.

## sqlx-sqlite 0.8.6

- Upstream: <https://github.com/launchbadge/sqlx>, crates.io release `sqlx-sqlite` 0.8.6.
- License: MIT OR Apache-2.0 (`vendor/sqlx-sqlite/LICENSE-MIT`, `LICENSE-APACHE`). No AGPL.
- `src/` is byte-identical to the published 0.8.6 crate.
- `Cargo.toml` is the published normalized manifest with exactly two edits (marked in the file):
  1. `libsqlite3-sys` version requirement `0.30.1` -> `>=0.30.1, <0.40.0`.
  2. the dev-only `[dev-dependencies.sqlx]` entry removed (dev cycle; crate is not a workspace member).
- Why: RUSTSEC-2026-0459 (SQLite < 3.54.0 bundled in `libsqlite3-sys` < 0.39). sqlx 0.8.x pins
  `libsqlite3-sys` 0.30.1, the patched `libsqlite3-sys` 0.39 ships with `rusqlite` 0.41, and Cargo
  allows only one `links = "sqlite3"` crate per graph. No released sqlx (0.9.0 caps `< 0.38`) accepts 0.39.
- Result: workspace uses `libsqlite3-sys` 0.39.1 (bundled SQLite 3.54.1) for both `rusqlite` 0.41.1 and `sqlx-sqlite`.
- Remove when: the workspace moves to a released `sqlx` whose `sqlx-sqlite` accepts `libsqlite3-sys >= 0.39`;
  then delete `vendor/sqlx-sqlite`, the `[patch.crates-io]` entry and the `exclude`.

To re-verify the patch is unmodified apart from the two edits:

```sh
diff -r vendor/sqlx-sqlite/src ~/.cargo/registry/src/*/sqlx-sqlite-0.8.6/src
```
