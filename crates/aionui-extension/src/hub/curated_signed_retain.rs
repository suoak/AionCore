//! Host-facing retain path: strict signed check → optional bind into [`SignedRootBindingStore`].
//!
//! Curated Hub install (`HubInstaller`) only holds install **path strings**. It cannot
//! honestly obtain a Host-opened trusted directory `File` without inventing trust
//! (e.g. `File::open(ext_dir)` / treating `LoadedExtension.directory` as authenticated).
//! Therefore this module is the retain seam for a higher Host layer that already:
//! 1. ran (or is about to run) [`check_signed_manifest`], and
//! 2. holds a Host-opened directory `File` it treats as the trusted root — or honestly
//!    admits it does not have one.
//!
//! Fail-closed:
//! - signature / strict check fails → never install a binding;
//! - trusted root `File` absent → leave unbound (invoke stays `integrity_binding_unavailable`);
//! - never invent a root from a path `String`.
//!
//! Not execution authorization, not Node/shell unlock, not M9/E13 completion, not 3.0.0.

use std::fs::File;

use super::curated::CuratedInstallPolicy;
use super::signed_manifest::{
    SignatureCheckedManifest, SignedManifestEntry, SignedManifestError, check_signed_manifest,
};
use crate::signed_root_binding::{SignedRootBindingError, SignedRootBindingStore};

/// Result of an attempted curated signed-root retain. Fixed categories only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CuratedSignedRetainOutcome {
    /// `SignedRootBindingStore::install` succeeded.
    Retained,
    /// Strict [`check_signed_manifest`] rejected the snapshot — store unchanged.
    SignatureRejected(SignedManifestError),
    /// Host did not supply a trusted directory `File` — left unbound; no path invent.
    RootHandleMissing,
    /// Store rejected the pair (identity / root metadata) — store unchanged for that attempt.
    BindingRejected(SignedRootBindingError),
}

impl CuratedSignedRetainOutcome {
    pub fn is_retained(self) -> bool {
        matches!(self, Self::Retained)
    }
}

/// Retain an **already** signature-checked snapshot when the Host also has a trusted root FD.
///
/// `trusted_root == None` → [`CuratedSignedRetainOutcome::RootHandleMissing`] (no invent).
/// Does not re-check the signature; call [`try_retain_curated_signed_root`] when the Host
/// still needs `check_signed_manifest`.
pub fn retain_checked_signed_root(
    store: &SignedRootBindingStore,
    checked: SignatureCheckedManifest,
    trusted_root: Option<File>,
) -> CuratedSignedRetainOutcome {
    let Some(root) = trusted_root else {
        return CuratedSignedRetainOutcome::RootHandleMissing;
    };
    let key = checked.name().to_owned();
    match store.install(&key, checked, root) {
        Ok(()) => CuratedSignedRetainOutcome::Retained,
        Err(err) => CuratedSignedRetainOutcome::BindingRejected(err),
    }
}

/// Strict check then explicit retain — the Host-facing curated install retain seam.
///
/// Order is intentional: signature failure short-circuits **before** considering the root
/// handle, so a bad signature never installs even when a directory `File` was supplied.
/// A missing root after a successful check leaves the store unbound (fail-closed for invoke).
pub fn try_retain_curated_signed_root(
    store: &SignedRootBindingStore,
    policy: &CuratedInstallPolicy,
    entry: &SignedManifestEntry<'_>,
    raw_manifest: &[u8],
    trusted_root: Option<File>,
) -> CuratedSignedRetainOutcome {
    let checked = match check_signed_manifest(policy, entry, raw_manifest) {
        Ok(checked) => checked,
        Err(err) => return CuratedSignedRetainOutcome::SignatureRejected(err),
    };
    retain_checked_signed_root(store, checked, trusted_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{signature_payload, sri_sha512};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::io::Write;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[19; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn signed_raw(raw: &[u8], name: &str, version: &str) -> (String, String) {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        (integrity, signature)
    }

    fn entry<'a>(name: &'a str, version: &'a str, integrity: &'a str, signature: &'a str) -> SignedManifestEntry<'a> {
        SignedManifestEntry {
            name,
            version,
            pubkey_id: Some("fixture"),
            integrity: Some(integrity),
            signature: Some(signature),
        }
    }

    #[test]
    fn success_retains_when_check_passes_and_root_file_supplied() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let (integrity, signature) = signed_raw(raw, "demo.plugin", "1.0.0");
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        let outcome = try_retain_curated_signed_root(
            &store,
            &policy(),
            &entry("demo.plugin", "1.0.0", &integrity, &signature),
            raw,
            Some(root),
        );
        assert_eq!(outcome, CuratedSignedRetainOutcome::Retained);
        assert!(store.contains("demo.plugin"));
        let (snap, root2) = store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.name(), "demo.plugin");
        assert!(root2.metadata().unwrap().is_dir());
    }

    #[test]
    fn failed_signature_does_not_retain_even_with_root_file() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let (integrity, signature) = signed_raw(raw, "demo.plugin", "1.0.0");
        // Flip one hex nibble → invalid signature; root must not be installed.
        let mut sig_bytes = signature.into_bytes();
        sig_bytes[0] = if sig_bytes[0] == b'0' { b'1' } else { b'0' };
        let signature = String::from_utf8(sig_bytes).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        let outcome = try_retain_curated_signed_root(
            &store,
            &policy(),
            &entry("demo.plugin", "1.0.0", &integrity, &signature),
            raw,
            Some(root),
        );
        assert!(matches!(
            outcome,
            CuratedSignedRetainOutcome::SignatureRejected(SignedManifestError::InvalidSignature)
        ));
        assert!(store.is_empty());
        assert!(!store.contains("demo.plugin"));
    }

    #[test]
    fn missing_root_handle_leaves_unbound_without_inventing_from_path() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let (integrity, signature) = signed_raw(raw, "demo.plugin", "1.0.0");
        // Caller could have a path string; we deliberately pass None instead of File::open(path).
        let _path_string_must_not_be_used = "/tmp/untrusted-extension-dir";
        let outcome = try_retain_curated_signed_root(
            &store,
            &policy(),
            &entry("demo.plugin", "1.0.0", &integrity, &signature),
            raw,
            None,
        );
        assert_eq!(outcome, CuratedSignedRetainOutcome::RootHandleMissing);
        assert!(store.is_empty());
        assert!(!store.contains("demo.plugin"));
    }

    #[test]
    fn retain_checked_rejects_non_directory_root_without_binding() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let (integrity, signature) = signed_raw(raw, "demo.plugin", "1.0.0");
        let checked =
            check_signed_manifest(&policy(), &entry("demo.plugin", "1.0.0", &integrity, &signature), raw).unwrap();
        let mut tmp = tempfile::tempfile().unwrap();
        tmp.write_all(b"not-a-dir").unwrap();
        let outcome = retain_checked_signed_root(&store, checked, Some(tmp));
        assert_eq!(
            outcome,
            CuratedSignedRetainOutcome::BindingRejected(SignedRootBindingError::RootNotDirectory)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn integrity_mismatch_does_not_retain() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let (integrity, signature) = signed_raw(raw, "demo.plugin", "1.0.0");
        let other = br#"{"name":"demo.plugin","version":"1.0.0","x":1}"#;
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        let outcome = try_retain_curated_signed_root(
            &store,
            &policy(),
            &entry("demo.plugin", "1.0.0", &integrity, &signature),
            other,
            Some(root),
        );
        assert!(matches!(
            outcome,
            CuratedSignedRetainOutcome::SignatureRejected(SignedManifestError::IntegrityMismatch)
        ));
        assert!(store.is_empty());
    }
}
