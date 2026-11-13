# Built-in signed packs (test signatures only)

`index.json` and the `*.zip` files are produced by `workmate-builtin-packs` using the
throwaway test/dev key in `tools/builtin-sign/` (id `workmate-test-dev`).

```
cargo run -p aionui-extension --bin workmate-builtin-packs
cargo run -p aionui-extension --bin workmate-builtin-packs -- --check
```

## Packs

| Pack | Kind | What it exercises |
| --- | --- | --- |
| `workmate.theme-dusk-ember` | theme | converted CSS themes (`tools/theme-import/`) |
| `workmate.sample-shortcuts` | standalone panel | `contributes.panels`, i18n `en` + `zh-CN`, verified asset serve; static HTML + CSS, no script |
| `workmate.sample-checklist` | work-panel view | `contributes.workPanelViews` (`placement: secondary`), sandboxed iframe with script; HTML + JS + CSS, in-memory state only |

The two samples declare `permissions` all `false`, no hooks, no background services and no
network. Every shipped file is in the signed `files` list and the entry point's
`entryPointIntegrity` is written by this tool from the entry file's bytes (`--check` fails when it
is missing or stale). The pages also carry a `<meta>` Content-Security-Policy (`default-src 'none'`
plus same-origin `style`/`img`, and `script-src 'self'` for the checklist) as defence in depth.

`--check` fails when a source directory, the converter output, or the test signature
does not match the committed zip / index. No production key is used or required.

These test-signed packs are **not** installed by the Host at startup. The production
built-in set (`aionui_app::builtin_extensions::builtins`) stays empty until the owner
embeds a production public key and production-signed packs.
