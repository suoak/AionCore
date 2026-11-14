//! Release-build policy for **unsigned local extensions** (internal security review F4, decided
//! 2026-11-14: option (b)).
//!
//! Unsigned (legacy, no signed `files` list) extensions keep loading in every build: their UI
//! contributions, themes and pages still work. In **release builds** the Host never runs code they
//! ship by path — lifecycle hooks (`onInstall` / `onUninstall` / `onActivate` / `onDeactivate`) and
//! legacy background-service spawns — unless an administrator opts back in with
//! [`ALLOW_UNSIGNED_CODE_ENV`]`=1` in the Host's environment. Development (debug) builds are
//! unchanged. Signed extensions are not affected by this module (they never get path-spawned code
//! at all, see `lifecycle::runnable_lifecycle_hooks` and `bound_background_start`).

/// Administrator opt-in: `1` / `true` lets release builds run unsigned extensions' hooks and
/// background services again.
pub const ALLOW_UNSIGNED_CODE_ENV: &str = "WORKMATE_ALLOW_UNSIGNED_EXTENSION_CODE";

/// Reason code reported when a background-service start is refused by this policy (and logged when
/// a lifecycle hook is skipped).
pub const UNSIGNED_CODE_DISABLED: &str = "unsigned_extension_code_disabled";

/// Whether this Host may run code shipped by unsigned extensions (hooks, legacy service spawns).
pub fn unsigned_code_allowed() -> bool {
    decide(
        !cfg!(debug_assertions),
        std::env::var(ALLOW_UNSIGNED_CODE_ENV).ok().as_deref(),
    )
}

/// Pure decision: debug builds always allow; release builds only with an explicit opt-in.
pub(crate) fn decide(release_build: bool, opt_in: Option<&str>) -> bool {
    if !release_build {
        return true;
    }
    matches!(
        opt_in.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true")
    )
}

/// Background-service start gate for the legacy path spawn.
pub(crate) fn gate_unsigned_service_start(allowed: bool) -> Result<(), crate::error::ExtensionError> {
    if allowed {
        Ok(())
    } else {
        Err(crate::error::ExtensionError::ManifestValidation(format!(
            "background service start refused: {UNSIGNED_CODE_DISABLED} (unsigned extensions do not run background services in release builds)"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_builds_are_unchanged() {
        for v in [None, Some(""), Some("0"), Some("1")] {
            assert!(decide(false, v), "{v:?}");
        }
    }

    #[test]
    fn release_builds_need_an_explicit_admin_opt_in() {
        assert!(!decide(true, None));
        for v in ["", "0", "false", "yes", "2", "on"] {
            assert!(!decide(true, Some(v)), "{v}");
        }
        for v in ["1", "true", "TRUE", " 1 "] {
            assert!(decide(true, Some(v)), "{v}");
        }
    }

    #[test]
    fn refusal_carries_the_catalogued_code() {
        assert!(gate_unsigned_service_start(true).is_ok());
        let text = gate_unsigned_service_start(false).unwrap_err().to_string();
        assert_eq!(crate::reason_catalogue::find_code(&text), Some(UNSIGNED_CODE_DISABLED));
        assert!(crate::reason_catalogue::hint(UNSIGNED_CODE_DISABLED).is_some());
    }
}
