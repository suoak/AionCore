# Optional entry-point digest binding (panels + workPanelViews)

## Why the panels knife (choice B, PR252)

Readonly map after Core PR251 (`ad100781…`):

| Site | Holds trusted directory `File` when `SignatureCheckedManifest` exists? |
| --- | --- |
| `HubInstaller::install/update` | No — path strings + soft `evaluate_install` only; must not `File::open(ext_dir)` |
| `hub_routes` install handlers | No — delegates to HubInstaller |
| `aionui-app` `build_extension_states` | Wires prepare adapter; never opens install roots |
| AionUi desktop hub.install | HTTP POST only |

No honest Host call site exists to wire `try_retain_curated_signed_root` without inventing trust from path strings. Choice **A skipped**. Panels knife added declaration-only panel integrity parallel to `handlerIntegrity`.

## What this adds

### Standalone panels (`contributes.panels`)

`ExtPanel` may include optional `entryPointIntegrity` (serde field
`entry_point_integrity`, JSON alias `entryPointIntegrity`). Same canonical
SHA-512 SRI rules as `handlerIntegrity` via `hub::is_canonical_sha512_sri`.

### Work panel views (`contributes.workPanelViews`) — sibling knife

`ExtWorkPanelView` may include the same optional `entryPointIntegrity` field
and validation rules. Implemented as a **sibling binder** (not a generalized
panels binder with a contribute-kind switch) so:

- panels binder + `EntryPointDigestError::{PanelNotFound,AmbiguousPanelId}` stay
  unchanged for app compose consumers;
- a workPanelViews id never resolves through the panels binder (and vice versa),
  even when string ids collide across contribute arrays.

## What absence means

**Missing/None does not mean verified.** Back-compat manifests without the field
continue to parse.

## Pure helpers

### Panels

- `expected_entry_point_integrity(&ExtensionManifest, panel_id)` — exact panel id;
  duplicate ids → `AmbiguousPanelId`.
- `bind_entry_point_digest_from_signed(&SignatureCheckedManifest, panel_id)` —
  re-parses owned signed byte snapshot only (no filesystem reread).

### Work panel views (sibling)

- `expected_work_panel_view_entry_point_integrity(&ExtensionManifest, view_id)` —
  exact view id; duplicate ids → `AmbiguousViewId`.
- `bind_work_panel_view_entry_point_digest_from_signed(&SignatureCheckedManifest, view_id)`.

Both binders reuse opaque `EntryPointDigestBinding`. Present invalid SRI, or
present SRI with empty `entryPoint`, is rejected at `parse_manifest` /
`validate_manifest` (fail closed).

## Trust limits

Declaration only. Does **not** authorize panel/view load/execution, invent
signed-root bindings, change installer/routes/gates, or complete M9/E13. No
entry-point file IO in these helpers.

## Host composition (app)

`aionui_app::extension_js_panel_source::load_checked_panel_source_from_signed`
composes the **panels** binder with `read_checked_source` against a Host-supplied
trusted root. Missing integrity remains `NotPresent` (no unchecked fallback).
Success returns the same owned verified source `String`. Still not panel-load/
iframe auth, curation/owner/root proof, or M9/E13 completion.

**No workPanelViews app compose in the sibling knife** — extension-crate field +
validate + binder only until a deliberate Host call site exists.
