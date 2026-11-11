//! Host-owned association of a signature-checked manifest snapshot with a trusted root FD.
//!
//! The Host must **explicitly** install a binding after it has already:
//! 1. produced a [`SignatureCheckedManifest`] via `check_signed_manifest`, and
//! 2. opened a directory `File` it treats as the authenticated trusted root.
//!
//! This module never opens paths from `LoadedExtension.directory`, never re-parses
//! unsigned disk manifests, and never invents trust. Install requires the map key
//! to equal `checked.name()` and `root` to be a directory FD (metadata only).
//! Not execution authorization, not M9/E13 completion, not a Node/shell unlock.

use std::collections::HashMap;
use std::fs::File;
use std::sync::Mutex;

use crate::hub::signed_manifest::SignatureCheckedManifest;

/// Fixed categories only — no paths, digests, or raw error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignedRootBindingError {
    /// Map key did not equal `SignatureCheckedManifest::name()`.
    IdentityMismatch,
    /// `metadata()` failed on the supplied root FD.
    RootUnavailable,
    /// Supplied FD metadata is not a directory.
    RootNotDirectory,
    /// `install_new` refused because a binding for that extension already exists.
    AlreadyBound,
}

/// One Host-retained `(signed snapshot, trusted root FD)` pair.
///
/// Private fields; no path-string constructor. Debug never dumps bytes/paths.
pub struct SignedRootBinding {
    checked: SignatureCheckedManifest,
    root: File,
}

impl std::fmt::Debug for SignedRootBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignedRootBinding")
            .field("name", &self.checked.name())
            .field("version", &self.checked.version())
            .finish_non_exhaustive()
    }
}

impl SignedRootBinding {
    pub fn checked(&self) -> &SignatureCheckedManifest {
        &self.checked
    }

    pub fn root(&self) -> &File {
        &self.root
    }

    /// Duplicate the retained root FD for a one-shot prepare call (does not reopen by path).
    pub fn try_clone_root(&self) -> std::io::Result<File> {
        self.root.try_clone()
    }
}

/// Process-local store of Host-installed signed-root bindings, keyed by extension name.
#[derive(Default)]
pub struct SignedRootBindingStore {
    inner: Mutex<HashMap<String, SignedRootBinding>>,
}

impl SignedRootBindingStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Explicitly install a Host-authenticated binding.
    ///
    /// `extension_key` must equal `checked.name()`. `root` must already be a Host-opened
    /// directory FD — this API never derives a root from a directory `String`.
    pub fn install(
        &self,
        extension_key: &str,
        checked: SignatureCheckedManifest,
        root: File,
    ) -> Result<(), SignedRootBindingError> {
        if extension_key != checked.name() {
            return Err(SignedRootBindingError::IdentityMismatch);
        }
        let meta = root.metadata().map_err(|_| SignedRootBindingError::RootUnavailable)?;
        if !meta.is_dir() {
            return Err(SignedRootBindingError::RootNotDirectory);
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.insert(extension_key.to_owned(), SignedRootBinding { checked, root });
        Ok(())
    }

    /// Like [`install`](Self::install) but never replaces: if a binding for the extension already
    /// exists the call fails with `AlreadyBound` and the live binding is untouched (checked and
    /// inserted under one lock, so concurrent callers cannot both win).
    pub fn install_new(
        &self,
        extension_key: &str,
        checked: SignatureCheckedManifest,
        root: File,
    ) -> Result<(), SignedRootBindingError> {
        if extension_key != checked.name() {
            return Err(SignedRootBindingError::IdentityMismatch);
        }
        let meta = root.metadata().map_err(|_| SignedRootBindingError::RootUnavailable)?;
        if !meta.is_dir() {
            return Err(SignedRootBindingError::RootNotDirectory);
        }
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.contains_key(extension_key) {
            return Err(SignedRootBindingError::AlreadyBound);
        }
        guard.insert(extension_key.to_owned(), SignedRootBinding { checked, root });
        Ok(())
    }

    /// Snapshot clone of checked bytes + dup'd root FD for invoke prepare outside the map lock.
    pub fn lookup_clone(&self, extension_key: &str) -> Option<(SignatureCheckedManifest, File)> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let binding = guard.get(extension_key)?;
        let root = binding.root.try_clone().ok()?;
        Some((binding.checked.clone(), root))
    }

    /// Clone only the signature-checked snapshot (no root FD dup) for read-only gates.
    pub fn lookup_checked(&self, extension_key: &str) -> Option<SignatureCheckedManifest> {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.get(extension_key).map(|b| b.checked.clone())
    }

    pub fn contains(&self, extension_key: &str) -> bool {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.contains_key(extension_key)
    }

    pub fn remove(&self, extension_key: &str) -> bool {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.remove(extension_key).is_some()
    }

    pub fn clear(&self) {
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.clear();
    }

    pub fn len(&self) -> usize {
        let guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        guard.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::io::Write;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[17; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn checked(raw: &[u8], name: &str, version: &str) -> SignatureCheckedManifest {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name,
                version,
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    #[test]
    fn install_rejects_identity_mismatch() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let temp = tempfile::tempdir().unwrap();
        let root = crate::hub::test_support::open_dir(temp.path());
        let err = store
            .install("other.plugin", checked(raw, "demo.plugin", "1.0.0"), root)
            .unwrap_err();
        assert_eq!(err, SignedRootBindingError::IdentityMismatch);
        assert!(store.is_empty());
    }

    #[test]
    fn install_rejects_non_directory_root() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let mut tmp = tempfile::tempfile().unwrap();
        tmp.write_all(b"not-a-dir").unwrap();
        let err = store
            .install("demo.plugin", checked(raw, "demo.plugin", "1.0.0"), tmp)
            .unwrap_err();
        assert_eq!(err, SignedRootBindingError::RootNotDirectory);
    }

    #[test]
    fn install_lookup_clone_and_remove() {
        let store = SignedRootBindingStore::new();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        let temp = tempfile::tempdir().unwrap();
        let root = crate::hub::test_support::open_dir(temp.path());
        store
            .install("demo.plugin", checked(raw, "demo.plugin", "1.0.0"), root)
            .unwrap();
        assert!(store.contains("demo.plugin"));
        let (snap, root2) = store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.name(), "demo.plugin");
        assert!(root2.metadata().unwrap().is_dir());
        assert_eq!(store.lookup_checked("demo.plugin").unwrap().name(), "demo.plugin");
        // Original binding remains after clone lookup.
        assert!(store.contains("demo.plugin"));
        assert!(store.remove("demo.plugin"));
        assert!(!store.contains("demo.plugin"));
        assert!(store.lookup_clone("demo.plugin").is_none());
    }

    #[test]
    fn never_invents_binding_from_empty_store() {
        let store = SignedRootBindingStore::new();
        assert!(store.lookup_clone("anything").is_none());
        assert!(store.lookup_checked("anything").is_none());
        assert!(!store.contains("anything"));
    }
}
