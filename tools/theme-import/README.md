# Theme import (Orca Dusk & Ember → WorkMate CSS)

Vendored inputs (no network in CI):

- `upstream/LICENSE`, `upstream/dusk.json`, `upstream/ember.json` from
  https://github.com/stablyai/orca-plugins `plugins/dusk-ember` at commit
  `4589eb93e28d8a4e4fdd3a77670297da804e7abe`.
- Git blob SHAs are pinned in `upstream/SHAS` and checked by `workmate-theme-import`.
- Terminal themes, `tokenColors`, and Orca `cover.png` / `dusk.png` / `ember.png` are
  **not** vendored and must not be added.

Commands (from the repository root):

```
cargo run -p aionui-extension --bin workmate-theme-import
cargo run -p aionui-extension --bin workmate-theme-import -- --check
```

`--check` exits 2 if a vendored file's git blob SHA drifted, 3 if the committed
`extensions/workmate.theme-dusk-ember/themes/dusk-ember.css` does not match the converter.

Mapping: see the module docs of `aionui_extension::theme_import` and the generated CSS header.
