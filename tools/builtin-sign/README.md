# Test/dev curator key (throwaway)

This directory holds the **throwaway test/dev** Ed25519 seed used by
`workmate-builtin-packs` and by CI `--check`.

- Key id: `workmate-test-dev`
- `workmate-test-dev.hex` is a 32-byte seed encoded as 64 lowercase hex characters.
- It is **not a secret** and **not for production**. Anyone can sign a pack that
  verifies against this key.
- Do **not** add a production private or public key here. Production signing and
  pubkey embedding are done later, off-repo, by the owner.

The Host application does not embed this key (`builtin_extensions::builtins()` stays empty).
