//! The app's built-in (shipped) signed extension packs (M9 E13 slice B1).
//!
//! The install / upgrade / self-heal machinery lives in `aionui_extension::hub::builtin_packs`; this
//! module only names **what the app ships**. It is deliberately empty: no pack and, above all, no
//! signing key is embedded until the owner provisions a production key (slice B2). With an empty
//! set the Host behaves exactly as before — an unconfigured Host opens no directory and installs
//! nothing, and a configured Host merges nothing into its signature policy.
//!
//! Adding a pack later means adding its zip (`include_bytes!`), the metadata printed by the pack /
//! sign tooling and the matching public key here, and nothing else.

use aionui_extension::hub::BuiltinSet;

/// The production set (empty until a production key exists).
pub(crate) fn builtins() -> &'static BuiltinSet<'static> {
    static SET: BuiltinSet<'static> = BuiltinSet::EMPTY;
    &SET
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard: shipping a pack or key is a deliberate, reviewed change (B2), never an accident.
    #[test]
    fn production_builtin_set_is_empty_until_a_production_key_is_provisioned() {
        assert!(builtins().is_empty());
        assert!(builtins().packs.is_empty() && builtins().keys.is_empty());
    }
}
