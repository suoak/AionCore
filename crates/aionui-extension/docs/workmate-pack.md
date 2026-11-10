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
control-character names; case-fold collisions (also with `aion-extension.json`); `$file:` references
in the manifest; more than 512 entries (files + manifest), a file over 8 MiB, more than 32 MiB in
total, a manifest over 256 KiB; and any manifest the Host would refuse (`parse_manifest` runs on the
result, so e.g. a referenced asset that is not in the directory, or a declared
`entryPointIntegrity` that disagrees with the real bytes, fails at pack time). Empty directories are
not packaged. File modes are normalized to `0644` and timestamps to 1980-01-01 in the zip.

## Signing
Signing stays a curator step: sign `signature_payload(name, version, manifestIntegrity)` with the
curator key and ship `(pubkey_id, integrity, signature)` in the index entry. The zip is not signed;
the signature covers the manifest and the manifest covers each file's SRI.
