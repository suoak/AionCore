# Built-in signed packs (test signatures only)

`index.json` and the `*.zip` files are produced by `workmate-builtin-packs` using the
throwaway test/dev key in `tools/builtin-sign/` (id `workmate-test-dev`).

```
cargo run -p aionui-extension --bin workmate-builtin-packs
cargo run -p aionui-extension --bin workmate-builtin-packs -- --check
```

`--check` fails when a source directory, the converter output, or the test signature
does not match the committed zip / index. No production key is used or required.

These test-signed packs are **not** installed by the Host at startup. The production
built-in set (`aionui_app::builtin_extensions::builtins`) stays empty until the owner
embeds a production public key and production-signed packs.
