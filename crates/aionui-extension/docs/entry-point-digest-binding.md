# Optional panel entry-point digest binding

## Why this knife (choice B)

Readonly map after Core PR251 (`ad100781…`):

| Site | Holds trusted directory `File` when `SignatureCheckedManifest` exists? |
| --- | --- |
| `HubInstaller::install/update` | No — path strings + soft `evaluate_install` only; must not `File::open(ext_dir)` |
| `hub_routes` install handlers | No — delegates to HubInstaller |
| `aionui-app` `build_extension_states` | Wires prepare adapter; never opens install roots |
| AionUi desktop hub.install | HTTP POST only |

No honest Host call site exists to wire `try_retain_curated_signed_root` without inventing trust from path strings. Choice **A skipped**. This knife adds declaration-only panel integrity parallel to `handlerIntegrity`.

## What this adds

`ExtPanel` may include optional `entryPointIntegrity` (serde field
`entry_point_integrity`, JSON alias `entryPointIntegrity`). Same canonical
SHA-512 SRI rules as `handlerIntegrity` via `hub::is_canonical_sha512_sri`.

Scoped to standalone `contributes.panels` only — not `workPanelViews` or
background services.

## What absence means

**Missing/None does not mean verified.** Back-compat manifests without the field
continue to parse.

## Pure helpers

- `expected_entry_point_integrity(&ExtensionManifest, panel_id)` — exact panel id;
  duplicate ids → `AmbiguousPanelId`.
- `bind_entry_point_digest_from_signed(&SignatureCheckedManifest, panel_id)` —
  re-parses owned signed byte snapshot only (no filesystem reread).

Present invalid SRI, or present SRI with empty `entryPoint`, is rejected at
`parse_manifest` / `validate_manifest` (fail closed).

## Trust limits

Declaration only. Does **not** authorize panel load/execution, invent signed-root
bindings, change installer/routes/gates, or complete M9/E13. No entry-point file IO
in these helpers.

## Host composition (app)

`aionui_app::extension_js_panel_source::load_checked_panel_source_from_signed`
composes this binder with `read_checked_source` against a Host-supplied trusted
root. Missing integrity remains `NotPresent` (no unchecked fallback). Success
returns the same owned verified source `String`. Still not panel-load/iframe
auth, curation/owner/root proof, or M9/E13 completion. Standalone `panels` only.
