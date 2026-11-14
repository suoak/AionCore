# Extension reason codes (CSBU WorkMate)

When CSBU WorkMate refuses to open, start, run, install, update or remove an extension, it reports a
**fixed reason code**: a lower-case `snake_case` token such as `verified_asset_integrity_mismatch`.
The codes are a stable contract. They are never renamed or reused, and new ones are only added.

## Where the code appears (no new fields, no new routes)

The code is part of text the app already receives:

| Situation | Where |
|---|---|
| Opening an extension page or file is refused (`/assets/…`, `/asset-session/…`, panels, settings, theme CSS) | the `error` field of the JSON error body of the `403` (or `404` for a missing signed file) response, e.g. `{"success":false,"error":"Extension asset refused: verified_asset_not_listed. This file is not part of …","code":"FORBIDDEN"}`; the hint after the code is the English text below |
| A background service does not start | the start error message (`background service start refused: <code> (…)`) |
| An extension command / agent tool is invoked | the `reason` field of the invoke result |
| Installing, updating or removing from the hub fails | the result `msg` (`Signed package install failed: <code>`, `… (<code>)`) |

**Finding it:** split the text on every character that is not `a–z`, `0–9` or `_` and take the
first token that is a code in the table below (the Host's `reason_catalogue::find_code` does exactly
this). Then show the translated hint for that code (the UI keeps its own zh-CN text per code); if no
code is found, show the original message. Hints never contain paths, addresses, digests or keys.

## For extension authors

Most codes mean the installed copy no longer matches what you signed, or the package breaks a rule:

* **Package rules** (`package_*`, `curated_*`, `signature_*`): build with `workmate-pack`, which
  refuses links, special files, names Windows cannot hold (device names, trailing dot or space,
  8.3 short names such as `PROGRA~1`), case collisions and oversize packages before you sign
  (see `workmate-pack.md`). Sign the exact manifest you ship and publish its integrity in the hub
  index. A higher version is needed for an update; same-version reinstalls repair.
* **Files** (`verified_asset_*`, `theme_css_*`): every page, script, stylesheet, image, theme and
  translation the extension uses must be in the signed `files` list. Unlisted files are never served.
* **Background services** (`bound_service_*`): declare `runtime` (`workmate-js`), point
  `entryPoint` at a listed `.js` / `.mjs` file, and do not rely on `shell`, `node` or native
  programs: they are refused for signed extensions. `bound_service_runner_unavailable` and
  `bound_service_capabilities_unavailable` mean the service is valid but this app version cannot
  run it yet.
* **Built-in names** (`name_reserved_by_builtin`): the names of extensions shipped inside the app
  are reserved. Publish under your own name.
* `signed_snapshot_inconclusive` is reported both when opening pages and when starting services.

## Catalogue

Generated from `src/reason_catalogue.rs` (checked by a test; regenerate with
`AIONCORE_WRITE_REASON_DOC=1 cargo test -p aionui-extension --lib reason_catalogue`).

<!-- BEGIN GENERATED: reason_catalogue -->

### Opening extension pages and files

| Code | Hint |
|---|---|
| `entry_point_integrity_declared_unchecked_serve` | This extension page requires an integrity check that this copy cannot provide, so it was not opened. Reinstall the extension from the WorkMate hub. |
| `signed_entry_point_integrity_declared_unchecked_serve` | This signed extension page can only be opened from its verified installation. Reinstall the extension from the WorkMate hub. |
| `signed_snapshot_inconclusive` | The extension's signed description could not be checked, so it was blocked for safety. Reinstall the extension from the WorkMate hub. |
| `verified_asset_invalid_snapshot` | The extension's list of signed files is invalid. Reinstall the extension, or contact its publisher if this keeps happening. |
| `verified_asset_not_listed` | This file is not part of the extension's signed package, so it was not opened. Contact the extension's publisher. |
| `verified_asset_missing` | A file of this extension is missing from its installation. Reinstall the extension from the WorkMate hub. |
| `verified_asset_unreadable` | A file of this extension could not be read safely, for example because it was replaced by a link. Reinstall the extension from the WorkMate hub. |
| `verified_asset_integrity_mismatch` | A file of this extension was changed after it was installed. Reinstall the extension to restore the original files. |
| `verified_asset_unavailable` | Signed extension files cannot be opened on this platform. |
| `theme_css_invalid_path` | The extension's theme points to a file name that is not allowed. Contact the extension's publisher. |
| `theme_css_not_utf8` | The extension's theme file is not readable text. Contact the extension's publisher. |

### Starting background services

| Code | Hint |
|---|---|
| `entry_point_integrity_declared_unchecked_spawn` | This background service requires an integrity check that this copy cannot provide, so it was not started. Reinstall the extension from the WorkMate hub. |
| `signed_entry_point_integrity_declared_unchecked_spawn` | This signed background service can only start from its verified installation. Reinstall the extension from the WorkMate hub. |
| `bound_service_root_missing` | The verified installation of this extension is not available right now. Restart CSBU WorkMate; if it persists, reinstall the extension. |
| `bound_service_invalid_snapshot` | The extension's list of signed files is invalid, so the service was not started. Reinstall the extension or contact its publisher. |
| `bound_service_signed_snapshot_inconclusive` | The signed description of this service is incomplete or ambiguous, so it was not started. Contact the extension's publisher. |
| `bound_service_integrity_required` | This service is not covered by the extension's signature, so it was not started. Contact the extension's publisher. |
| `bound_service_entry_mismatch` | The service does not match its signed description, so it was not started. Reinstall the extension from the WorkMate hub. |
| `bound_service_entry_missing` | The service's program file is missing from the installation. Reinstall the extension from the WorkMate hub. |
| `bound_service_entry_unreadable` | The service's program file could not be read safely. Reinstall the extension from the WorkMate hub. |
| `bound_service_integrity_mismatch` | The service's program file was changed after installation, so it was not started. Reinstall the extension to restore it. |
| `bound_service_verified_read_unavailable` | Signed background services cannot be started on this platform. |
| `bound_service_runtime_required` | This service does not say how it should run, so it was not started. Contact the extension's publisher. |
| `bound_service_runtime_unsupported` | This service needs a runtime that CSBU WorkMate does not provide for extensions. Contact the extension's publisher. |
| `bound_service_shell_script_unsupported` | Shell-script services are not allowed for signed extensions. Contact the extension's publisher. |
| `bound_service_native_unsupported` | Native program services are not allowed for extensions. Contact the extension's publisher. |
| `bound_service_entry_kind_unsupported` | The service's program file is not a supported script type. Contact the extension's publisher. |
| `bound_service_capabilities_unavailable` | This service needs extra permissions that this version of CSBU WorkMate cannot grant yet. Update the app when a newer version is available. |
| `bound_service_runtime_requires_signed_files` | This service can only run from a signed installation. Reinstall the extension from the WorkMate hub. |
| `bound_service_runner_unavailable` | This service is verified, but running extension services is not enabled in this version of CSBU WorkMate. |

### Extension commands and agent tools

| Code | Hint |
|---|---|
| `integrity_binding_unavailable` | This extension command cannot run because its signed installation could not be verified. Reinstall the extension from the WorkMate hub. |
| `agent_tool_not_plan_safe` | This tool can make changes, and extension tools that make changes cannot run in this version of CSBU WorkMate. |
| `handler_integrity_checked` | The command's code was verified. Running extension code is not enabled in this version, so nothing was executed. |

### Installing, updating and removing extensions

| Code | Hint |
|---|---|
| `name_reserved_by_builtin` | This extension is built into CSBU WorkMate and is updated together with the app. Update the app instead. |
| `curated_install_refused` | This extension is not approved for installation here. Choose a curated extension or ask your administrator. |
| `curated_not_in_index` | This extension is not listed in the WorkMate hub. Refresh the hub and try again. |
| `curated_not_curated` | Only curated or built-in extensions can be installed. Choose a curated extension from the WorkMate hub. |
| `curated_not_allowlisted` | This extension is not on your organisation's allowed list. Ask your administrator to allow it. |
| `curated_integrity_missing` | The hub entry for this extension has no integrity value, so it cannot be verified. Contact the extension's publisher. |
| `curated_integrity_mismatch` | The extension's files do not match the hub listing. Refresh the hub and try again, or contact the publisher. |
| `curated_signature_required` | This installation only accepts signed extensions, and this one is not signed. Contact the extension's publisher. |
| `curated_signature_missing_key_id` | The extension's signature does not say which key signed it. Contact the extension's publisher. |
| `curated_signature_invalid` | The extension's signature is not valid. Do not install it; contact the extension's publisher. |
| `curated_unknown_key` | The extension was signed by a key this installation does not trust. Ask your administrator. |
| `curated_key_config_invalid` | The trusted signing keys are configured incorrectly. Ask your administrator to fix the configuration. |
| `signature_manifest_limit` | The extension's description is too large to be accepted. Contact the extension's publisher. |
| `signature_metadata_limit` | The extension's signing information is too large to be accepted. Contact the extension's publisher. |
| `signature_invalid_identity` | The extension's name or version is not valid. Contact the extension's publisher. |
| `signature_not_allowlisted` | This extension is not on your organisation's allowed list. Ask your administrator to allow it. |
| `signature_missing_key_id` | The extension's signature does not say which key signed it. Contact the extension's publisher. |
| `signature_invalid_key_id` | The extension's signing key name is not valid. Contact the extension's publisher. |
| `signature_unknown_key` | The extension was signed by a key this installation does not trust. Ask your administrator. |
| `signature_missing_integrity` | The extension's listing has no integrity value, so it cannot be verified. Contact the extension's publisher. |
| `signature_invalid_integrity` | The extension's integrity value is malformed. Contact the extension's publisher. |
| `signature_integrity_mismatch` | The extension's description does not match its signed listing. Refresh the hub and try again, or contact the publisher. |
| `signature_missing_signature` | This extension is not signed, and only signed extensions are accepted. Contact the extension's publisher. |
| `signature_invalid_signature` | The extension's signature is not valid. Do not install it; contact the extension's publisher. |
| `signature_invalid_manifest` | The extension's description is not valid. Contact the extension's publisher. |
| `signature_identity_mismatch` | The extension's name or version does not match its signed listing. Refresh the hub and try again, or contact the publisher. |
| `package_invalid` | The extension package is damaged or not a valid package. Download it again or contact the publisher. |
| `package_limit_exceeded` | The extension package is too large or has too many files. Contact the extension's publisher. |
| `package_unsafe_entry_name` | The extension package contains a file name that is not allowed on this system. Contact the extension's publisher. |
| `package_duplicate_entry` | The extension package contains the same file twice. Contact the extension's publisher. |
| `package_symlink_or_special_entry` | The extension package contains links or special files, which are not allowed. Contact the extension's publisher. |
| `package_entry_not_covered_by_signature` | The extension package contains files that are not covered by its signature. Contact the extension's publisher. |
| `package_covered_file_missing` | A signed file is missing from the extension package. Download it again or contact the publisher. |
| `package_invalid_covered_path` | The extension lists a file name that is not allowed. Contact the extension's publisher. |
| `package_conflicting_covered_digest` | The extension lists the same file with two different integrity values. Contact the extension's publisher. |
| `package_manifest_mismatch` | The description inside the package differs from the signed one. Download it again or contact the publisher. |
| `package_integrity_mismatch` | A file in the extension package does not match its signature. Download it again; do not install a modified package. |
| `package_invalid_dir_name` | The extension's name or version cannot be stored on this system. Contact the extension's publisher. |
| `package_case_collision` | Another installed extension or version has the same name apart from upper and lower case. Remove it first, then try again. |
| `package_place_failed` | The extension could not be put in place. Nothing was changed; try again. |
| `package_version_slot_blocked` | Something unexpected is in the way of this extension version. Remove the extension and install it again. |
| `package_lifecycle_hooks_refused` | This extension asks to run setup programs, which signed extensions cannot do. Contact the extension's publisher. |
| `package_downgrade_refused` | A newer version of this extension is already installed. Older versions are not installed automatically. |
| `package_marker_write_failed` | The installation could not be completed. Nothing was changed; check free disk space and try again. |
| `package_parent_not_directory` | The extensions folder of CSBU WorkMate is not usable. Restart the app; if it persists, contact support. |
| `package_remove_failed` | The extension could not be removed completely. Close anything using it, restart CSBU WorkMate and try again. |
| `package_root_create_failed` | The extension folder could not be created. Check free disk space and permissions, then try again. |
| `package_write_failed` | The extension files could not be written. Check free disk space, then try again. |
| `package_retain_failed` | The extension was installed but could not be activated. Restart CSBU WorkMate. |
| `package_unsupported_platform` | Signed extensions cannot be installed on this platform. |

<!-- END GENERATED: reason_catalogue -->
