# Unsigned local extensions in release builds

CSBU WorkMate 3.0 trusts **signed** extensions (built-in packs and curated hub packages). Older
*unsigned* extensions — a folder without a signed `files` list, dropped into an extensions path —
still load, but release builds treat them as content only.

| | Debug / development build | Release build (default) | Release build + admin opt-in |
|---|---|---|---|
| Loads; pages, panels, themes, i18n | yes | yes | yes |
| Lifecycle hooks (`onInstall`, `onUninstall`, `onActivate`, `onDeactivate`) | run | **not run** | run |
| Background services (legacy path spawn) | start | **refused** | start |

## Admin opt-in

Set this in the environment of the Host process (`aioncore`, or the desktop app that launches it):

```
WORKMATE_ALLOW_UNSIGNED_EXTENSION_CODE=1
```

Accepted values are `1` and `true`; anything else (or unset) keeps the default. Only do this when
you trust every unsigned extension on the machine: their hooks and services run with your user's
permissions.

## What users see

- A refused service start reports the reason code `unsigned_extension_code_disabled` (see
  `crates/aionui-extension/docs/extension-reason-codes.md`); the desktop app shows a translated hint.
- A skipped hook is logged at `warn` with the same code; installing or enabling the extension still
  succeeds.

Signed extensions are unaffected: they never run hooks or path-spawned code at all.

Background: internal security review finding F4 (decided 2026-11-14).
