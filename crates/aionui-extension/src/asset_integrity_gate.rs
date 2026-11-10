//! Fail-closed gate for serving panel / workPanelView **entry-point assets** (M9 E13).
//!
//! `serve_extension_asset` reads files by canonical *path* under `LoadedExtension.directory`
//! (a `String`) and returns the bytes with no integrity check. A declared
//! `entryPointIntegrity` cannot be enforced there: there is no honest Host directory `File`
//! (re-opening the directory string would fake a trust root) and the bytes are not routed
//! through `read_checked_source`.
//!
//! So when a `contributes.panels` / `contributes.workPanelViews` entry declares
//! `entryPointIntegrity` — in the loaded manifest, or in a Host-retained signature-checked
//! snapshot — a request for exactly that entry-point file is **refused**. If a retained
//! snapshot cannot be parsed the gate is inconclusive and refuses every asset of that
//! extension. Nothing falls back to an unchecked serve for a declared entry point.
//!
//! Undeclared entry points and non-entry assets (icons, scripts, css) keep legacy serving;
//! "undeclared" is never "verified". Not execution/load authorization, not root/owner
//! proof, not E13 complete. Reasons are fixed strings (no digests, paths, ids).

use std::path::{Path, PathBuf};

use crate::asset_paths::normalize_relative_asset_path;
use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::manifest::parse_manifest;
use crate::types::ExtensionManifest;

/// Fixed refusal categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetServeGateError {
    /// Loaded manifest declares `entryPointIntegrity` for the requested entry point.
    DeclaredIntegrityCannotBeChecked,
    /// Host-retained signed snapshot declares it.
    SignedIntegrityCannotBeChecked,
    /// Host-retained signed snapshot could not be parsed: refuse every asset.
    SignedSnapshotInconclusive,
}

impl AssetServeGateError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::DeclaredIntegrityCannotBeChecked => "entry_point_integrity_declared_unchecked_serve",
            Self::SignedIntegrityCannotBeChecked => "signed_entry_point_integrity_declared_unchecked_serve",
            Self::SignedSnapshotInconclusive => "signed_snapshot_inconclusive",
        }
    }
}

/// Entry-point paths (relative, normalized) that declare `entryPointIntegrity`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AssetIntegrityGate {
    loaded: Vec<PathBuf>,
    signed: Vec<PathBuf>,
}

fn declared_entry_points(manifest: &ExtensionManifest) -> Vec<PathBuf> {
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Vec::new();
    };
    let panels = contributes
        .panels
        .iter()
        .filter(|p| p.entry_point_integrity.is_some())
        .map(|p| p.entry_point.as_str());
    let views = contributes
        .work_panel_views
        .iter()
        .filter(|v| v.entry_point_integrity.is_some())
        .map(|v| v.entry_point.as_str());
    // Paths that fail normalization (traversal, backslash) can never be requested; drop them.
    // A remote URL normalizes to a bogus relative path that no served file equals.
    panels.chain(views).filter_map(normalize_relative_asset_path).collect()
}

impl AssetIntegrityGate {
    /// Build from the loaded manifest and an optional Host-retained signed snapshot.
    pub fn new(
        manifest: &ExtensionManifest,
        signed: Option<&SignatureCheckedManifest>,
    ) -> Result<Self, AssetServeGateError> {
        let signed_paths = match signed {
            None => Vec::new(),
            Some(checked) => {
                let parsed =
                    parse_manifest(checked.raw_bytes()).map_err(|_| AssetServeGateError::SignedSnapshotInconclusive)?;
                declared_entry_points(&parsed)
            }
        };
        Ok(Self {
            loaded: declared_entry_points(manifest),
            signed: signed_paths,
        })
    }

    fn refusal_for(&self, matches: impl Fn(&Path) -> bool) -> Result<(), AssetServeGateError> {
        if self.loaded.iter().any(|p| matches(p)) {
            return Err(AssetServeGateError::DeclaredIntegrityCannotBeChecked);
        }
        if self.signed.iter().any(|p| matches(p)) {
            return Err(AssetServeGateError::SignedIntegrityCannotBeChecked);
        }
        Ok(())
    }

    /// Lexical check on the already-normalized request path.
    pub fn check_requested(&self, requested_relative: &Path) -> Result<(), AssetServeGateError> {
        self.refusal_for(|p| p == requested_relative)
    }

    /// Canonical check: the resolved file is the same file as a declared entry point
    /// (covers symlink aliases). Declared entries that do not resolve are ignored.
    pub async fn check_canonical(
        &self,
        canonical_root: &Path,
        canonical_asset: &Path,
    ) -> Result<(), AssetServeGateError> {
        for (list_is_loaded, paths) in [(true, &self.loaded), (false, &self.signed)] {
            for rel in paths {
                if let Ok(resolved) = tokio::fs::canonicalize(canonical_root.join(rel)).await
                    && resolved == canonical_asset
                {
                    return Err(if list_is_loaded {
                        AssetServeGateError::DeclaredIntegrityCannotBeChecked
                    } else {
                        AssetServeGateError::SignedIntegrityCannotBeChecked
                    });
                }
            }
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.loaded.is_empty() && self.signed.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn sri() -> String {
        sri_sha512(b"entry body")
    }

    fn manifest(contributes: &str) -> ExtensionManifest {
        let raw = format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{contributes}}}"#);
        parse_manifest(raw.as_bytes()).unwrap()
    }

    /// Deserialize without `parse_manifest` validation (which rejects malformed SRIs).
    fn manifest_unvalidated(contributes: &str) -> ExtensionManifest {
        let raw = format!(r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{contributes}}}"#);
        serde_json::from_str(&raw).unwrap()
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let key = SigningKey::from_bytes(&[17; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            key.sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    fn declared_panel() -> String {
        format!(
            r#"{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html","entryPointIntegrity":"{}"}}]}}"#,
            sri()
        )
    }

    #[test]
    fn undeclared_entries_and_other_assets_are_not_gated() {
        let m = manifest(r#"{"panels":[{"id":"p","title":"P","entryPoint":"panels/board.html"}]}"#);
        let gate = AssetIntegrityGate::new(&m, None).unwrap();
        assert!(gate.is_empty());
        assert_eq!(gate.check_requested(Path::new("panels/board.html")), Ok(()));
    }

    #[test]
    fn declared_panel_entry_is_refused_other_assets_served() {
        let gate = AssetIntegrityGate::new(&manifest(&declared_panel()), None).unwrap();
        assert_eq!(
            gate.check_requested(Path::new("panels/board.html")),
            Err(AssetServeGateError::DeclaredIntegrityCannotBeChecked)
        );
        assert_eq!(gate.check_requested(Path::new("panels/board.css")), Ok(()));
        assert_eq!(gate.check_requested(Path::new("icon.svg")), Ok(()));
    }

    #[test]
    fn declared_work_panel_view_entry_is_refused() {
        let json = format!(
            r#"{{"workPanelViews":[{{"id":"v","title":"V","entryPoint":"./views/main.html","entryPointIntegrity":"{}"}}]}}"#,
            sri()
        );
        let gate = AssetIntegrityGate::new(&manifest(&json), None).unwrap();
        assert!(gate.check_requested(Path::new("views/main.html")).is_err());
    }

    #[test]
    fn invalid_declared_sri_still_gates() {
        let m = manifest_unvalidated(
            r#"{"panels":[{"id":"p","title":"P","entryPoint":"a.html","entryPointIntegrity":"bogus"}]}"#,
        );
        let gate = AssetIntegrityGate::new(&m, None).unwrap();
        assert!(gate.check_requested(Path::new("a.html")).is_err());
    }

    #[test]
    fn remote_entry_point_never_matches_a_real_asset() {
        let m = manifest_unvalidated(
            r#"{"panels":[{"id":"p","title":"P","entryPoint":"https://x.test/a.html","entryPointIntegrity":"bogus"}]}"#,
        );
        // Normalizes to a bogus relative path no served file can equal (no scheme handling here).
        let gate = AssetIntegrityGate::new(&m, None).unwrap();
        assert_eq!(gate.check_requested(Path::new("a.html")), Ok(()));
        assert_eq!(gate.check_requested(Path::new("x.test/a.html")), Ok(()));
    }

    #[test]
    fn signed_snapshot_declaration_refuses_even_if_loaded_manifest_stripped() {
        let signed_raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{}}}"#,
            declared_panel()
        );
        let snapshot = checked(signed_raw.as_bytes());
        let stripped = manifest(r#"{"panels":[{"id":"p","title":"P","entryPoint":"panels/board.html"}]}"#);
        let gate = AssetIntegrityGate::new(&stripped, Some(&snapshot)).unwrap();
        assert_eq!(
            gate.check_requested(Path::new("panels/board.html")),
            Err(AssetServeGateError::SignedIntegrityCannotBeChecked)
        );
        assert_eq!(gate.check_requested(Path::new("other.html")), Ok(()));
    }

    #[test]
    fn signed_snapshot_without_declaration_does_not_gate() {
        let snapshot = checked(br#"{"name":"demo.plugin","version":"1.0.0"}"#);
        let m = manifest(r#"{"panels":[{"id":"p","title":"P","entryPoint":"a.html"}]}"#);
        let gate = AssetIntegrityGate::new(&m, Some(&snapshot)).unwrap();
        assert!(gate.is_empty());
    }

    #[test]
    fn same_id_background_service_declaration_does_not_gate_assets() {
        let json = format!(
            r#"{{"backgroundServices":[{{"id":"w","entryPoint":"services/w.sh","entryPointIntegrity":"{}"}}]}}"#,
            sri()
        );
        let gate = AssetIntegrityGate::new(&manifest(&json), None).unwrap();
        assert!(gate.is_empty());
    }

    fn files_only_panel_json() -> String {
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"panels/board.html","integrity":"{}"}}],"contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html"}}]}}}}"#,
            sri()
        )
    }

    #[test]
    fn integrity_inherited_from_files_gates_exactly_like_declared() {
        let inherited = parse_manifest(files_only_panel_json().as_bytes()).unwrap();
        let declared = manifest(&declared_panel());
        let a = AssetIntegrityGate::new(&inherited, None).unwrap();
        let b = AssetIntegrityGate::new(&declared, None).unwrap();
        assert!(!a.is_empty());
        for p in ["panels/board.html", "panels/board.css", "icon.svg", "other.html"] {
            assert_eq!(a.check_requested(Path::new(p)), b.check_requested(Path::new(p)), "{p}");
        }
        assert_eq!(
            a.check_requested(Path::new("panels/board.html")),
            Err(AssetServeGateError::DeclaredIntegrityCannotBeChecked)
        );
    }

    #[test]
    fn files_only_signed_snapshot_refuses_even_if_loaded_manifest_stripped() {
        let snapshot = checked(files_only_panel_json().as_bytes());
        let stripped = manifest(r#"{"panels":[{"id":"p","title":"P","entryPoint":"panels/board.html"}]}"#);
        let gate = AssetIntegrityGate::new(&stripped, Some(&snapshot)).unwrap();
        assert_eq!(
            gate.check_requested(Path::new("panels/board.html")),
            Err(AssetServeGateError::SignedIntegrityCannotBeChecked)
        );
        assert_eq!(gate.check_requested(Path::new("other.html")), Ok(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canonical_check_catches_symlink_alias_of_declared_entry() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("panels")).unwrap();
        std::fs::write(root.join("panels/board.html"), "x").unwrap();
        std::os::unix::fs::symlink(root.join("panels/board.html"), root.join("alias.html")).unwrap();
        std::fs::write(root.join("other.html"), "y").unwrap();
        let gate = AssetIntegrityGate::new(&manifest(&declared_panel()), None).unwrap();
        let alias = std::fs::canonicalize(root.join("alias.html")).unwrap();
        assert_eq!(
            gate.check_canonical(&root, &alias).await,
            Err(AssetServeGateError::DeclaredIntegrityCannotBeChecked)
        );
        let other = std::fs::canonicalize(root.join("other.html")).unwrap();
        assert_eq!(gate.check_canonical(&root, &other).await, Ok(()));
    }
}
