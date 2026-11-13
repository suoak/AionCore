# `workmate-pack` — author packing helper (M9 E13)

Author-side tool of the CSBU WorkMate plugin platform. It turns an extension **source directory**
into what the Host installer (`install_local_signed_package_auto`) accepts: a manifest whose
top-level `files` list covers every package file, and a deterministic zip. It is built from
`aionui-extension` (`cargo run -p aionui-extension --bin workmate-pack -- …`) and shares the
installer's path rules and limits. It adds no Host route and performs no exec or network access.

```text
workmate-pack <dir> [--zip-out <file>] [--manifest-out <file>] [--update-manifest]
                    [--exclude <relative-path>]... [--force] [--check]
```

* No output option: dry run; a JSON report (name, version, `manifestIntegrity`, per-file SRI,
  excluded paths, `filesChanged`) is printed to stdout and nothing is written.
* `--zip-out` / `--manifest-out`: write the zip / the manifest (with the computed `files`). Outputs
  must be outside `<dir>`, refuse to replace an existing file without `--force`, and are never
  written through a symlink.
* `--update-manifest`: rewrite `<dir>/aion-extension.json` in place (the only way the tool writes
  into the source directory). The manifest is re-serialized as pretty JSON with sorted keys; running
  it again is a no-op.
* `--check`: write nothing; exit 3 when the manifest's `files` list is out of date (CI guard).
* `.git`, `node_modules` and `.DS_Store` are always left out (and reported); `--exclude` adds exact
  relative files or directories. Excluded trees are not inspected.

## Rules (all refuse, none silently skip)
Symlinks and non-regular files; non-UTF-8, non-canonical (`\`, `..`, empty components) or
control-character names; names that cannot exist on Windows (DOS device names such as `con`/`aux`/`nul`/`com1`, trailing dot or space, `: * ? " < > |`, names shaped like an 8.3 short-name alias such as `PROGRA~1` or `BOARD~1.HTM`, over 255 UTF-16 units — Windows is a required platform, so such a package is refused at pack time); case-fold collisions (also with `aion-extension.json`); `$file:` references
in the manifest; more than 512 entries (files + manifest), a file over 8 MiB, more than 32 MiB in
total, a manifest over 256 KiB; and any manifest the Host would refuse (`parse_manifest` runs on the
result, so e.g. a referenced asset that is not in the directory, or a declared
`entryPointIntegrity` that disagrees with the real bytes, fails at pack time). Empty directories are
not packaged. File modes are normalized to `0644` and timestamps to 1980-01-01 in the zip.

## Signing
Signing stays a curator step: sign `signature_payload(name, version, manifestIntegrity)` with the
curator key and ship `(pubkey_id, integrity, signature)` in the index entry. The zip is not signed;
the signature covers the manifest and the manifest covers each file's SRI.

## Built-in names are reserved
The packs CSBU WorkMate ships with the app (today `workmate.theme-dusk-ember`,
`workmate.sample-shortcuts`, `workmate.sample-checklist`) are pinned by the app binary: name,
version, key id + key bytes and manifest integrity. Such a name — compared ASCII
case-insensitively — can only hold that exact pinned pack. A package under a built-in name is
refused by hub install/update whatever its signer, version or `bundled`/`curated` flags ("built
into CSBU WorkMate and is updated with the app"), and a copy found on disk that is not the pin is
not restored (`BuiltinPinMismatch`) and is replaced by the app's own copy at startup. Built-ins
change only with an app update, and an app rollback rolls them back too. Pick another name.

## Windows hosts (required platform)
Signed packages install on Windows with the same guarantees as on unix (handle-relative, never
following links, signature and per-file SRI re-verified on every restart). What differs:

* **Lifecycle hooks are not supported on Windows.** `lifecycle.onInstall` / `onActivate` /
  `onDeactivate` are executed as the script file itself (a POSIX `#!/bin/sh` script), which Windows
  cannot do. A pack that relies on hooks works on macOS / Linux hosts only; do not depend on hooks
  for a pack that must run on Windows. (Hooks are not part of the signed-install guarantees.)
* **Identities that Windows cannot hold are refused at install** with `InvalidDirName`: a signed
  `name` or `version` whose encoded directory name is a DOS device name (`con`, `nul`, `aux`,
  `com1`, …, also `con.txt`), ends in a dot or space, or has the shape of an 8.3 short name
  (e.g. the name `ab!` encodes to `ab~21`). Pick other identifiers.
* **8.3 short-name aliases are never a listed name.** A component with at most one dot, a stem of
  at most 8 characters holding a single `~` followed only by digits, and an extension of at most 3
  characters (`PROGRA~1`, `BOARD~1.HTM`) can open a *different* long-named entry on a volume that
  generates short names, so it is refused at pack time, at install and by the verified reader.
  As defence in depth the Windows verified reader also compares the normalized final path of every
  file it opens (`GetFinalPathNameByHandleW`) with the retained root plus exactly the listed
  names, and refuses the read on any difference.
* **Names that differ only by ASCII case collide on NTFS** (`Demo.Plugin` vs `demo.plugin`, or the
  versions `1.0.0-RC1` vs `1.0.0-rc1`). The Host refuses to install or uninstall such an identity
  with `CaseCollision` instead of reusing (overwriting / deleting) the other one's directory. It
  never lower-cases an identity: the signed name must keep equal to the directory it is stored in
  (restore checks that), and lower-casing would also change the layout of existing unix installs.
  Use lower-case identifiers (the curated packs all do) so the question never comes up.
* A same-version *replace* is a two-step rename on Windows (no atomic directory exchange). A crash
  between the two steps leaves that extension unbound after restart (fail closed); reinstall it.
  A first install or a version change has no such window.

## Panel / work-panel-view listing

`GET /api/extensions/panels` and `/work-panel-views` (and the panel hint of `commands/invoke`) use a
split rule. Unsigned / unbound local extensions keep today's listing behaviour (their panels and
work-panel views still appear, with the same JSON). A **bound / signed** extension is listed only
when the Host-retained signed snapshot vouches for the entry: the signed manifest declares a
`files` list that contains the entry point (exact, case-sensitive), the contribution id is unique,
and the resolved entry URL equals the one derived from the signed entry point. A bound extension
signed without `files` does not appear. So a pack that ships panels must declare `files`
(`workmate-pack` always does). The response shape is unchanged.
