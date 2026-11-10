//! Verified asset serve from a Host-retained signed root (M9 E13 slice F).
//!
//! For an extension whose Host-retained signed snapshot declares top-level `files`, an asset is
//! served only when
//! 1. its path is **exactly** a listed `files` path (anything else is `NotListed`),
//! 2. it is read through the retained root `File` with FD-relative `openat(O_NOFOLLOW)` (never a
//!    path string, never `LoadedExtension.directory`), as a bounded regular file, and
//! 3. the bytes read match the listed SHA-512 SRI. The returned bytes are the very bytes that were
//!    hashed (one read, one snapshot), so a swap after the check cannot change what is returned.
//!
//! The listing comes from the signature-checked snapshot's own bytes (re-parsed with
//! `parse_manifest`), never from the loaded on-disk manifest. A snapshot with no `files` is
//! [`FilesIndex::from_signed`] `Ok(None)`: this module is then not applicable and the caller
//! keeps its existing behaviour. There is no fallback from a failure here to a path read.
//! Fixed error categories only; never paths, digests or bytes.

use std::collections::BTreeMap;
use std::fs::File;
use std::path::{Component, Path};

use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::hub::sri_sha512;
use crate::manifest::parse_manifest;

/// Per-file serve bound (same as the install default per-file limit).
pub const MAX_VERIFIED_ASSET_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifiedAssetError {
    /// The signed snapshot could not be parsed (cannot happen for a checked snapshot; fail closed).
    InvalidSnapshot,
    /// Requested path is not exactly a listed `files` path.
    NotListed,
    /// Listed file is absent from the retained root.
    Missing,
    /// Not a regular file, symlinked component, too large, or other read failure.
    Unreadable,
    /// Bytes read do not match the listed SRI.
    IntegrityMismatch,
    /// Platform has no FD-relative reader; refuse rather than read by path.
    Unavailable,
}

impl VerifiedAssetError {
    pub fn reason(self) -> &'static str {
        match self {
            Self::InvalidSnapshot => "signed snapshot is invalid",
            Self::NotListed => "path is not listed in the signed files list",
            Self::Missing => "listed file is missing",
            Self::Unreadable => "listed file cannot be read safely",
            Self::IntegrityMismatch => "file does not match its signed integrity",
            Self::Unavailable => "verified serve is unavailable on this platform",
        }
    }
}

/// `path -> integrity` of the signed snapshot's top-level `files`.
#[derive(Debug)]
pub struct FilesIndex {
    files: BTreeMap<String, String>,
}

/// A listed file whose bytes matched its signed SRI.
pub struct VerifiedAsset {
    path: String,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for VerifiedAsset {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedAsset")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl VerifiedAsset {
    /// Canonical listed path (for content-type selection).
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl FilesIndex {
    /// `Ok(None)` when the signed snapshot declares no `files` (legacy: not applicable).
    pub fn from_signed(checked: &SignatureCheckedManifest) -> Result<Option<Self>, VerifiedAssetError> {
        let manifest = parse_manifest(checked.raw_bytes()).map_err(|_| VerifiedAssetError::InvalidSnapshot)?;
        if manifest.files.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            files: manifest.files.into_iter().map(|f| (f.path, f.integrity)).collect(),
        }))
    }

    /// Resolve a normalized relative request to its **exact** listed `(path, integrity)`.
    /// Case-sensitive; no prefix, directory or manifest-file matches.
    pub fn resolve(&self, requested: &Path) -> Result<(&str, &str), VerifiedAssetError> {
        let mut joined = String::new();
        for component in requested.components() {
            let Component::Normal(part) = component else {
                return Err(VerifiedAssetError::NotListed);
            };
            let part = part.to_str().ok_or(VerifiedAssetError::NotListed)?;
            if !joined.is_empty() {
                joined.push('/');
            }
            joined.push_str(part);
        }
        self.files
            .get_key_value(joined.as_str())
            .map(|(p, i)| (p.as_str(), i.as_str()))
            .ok_or(VerifiedAssetError::NotListed)
    }
}

/// Blocking: read the listed file below the retained `root` and compare it to `integrity`.
///
/// Uses the platform verified reader ([`super::verified_reader`]): FD-relative `openat(O_NOFOLLOW)`
/// on unix, handle-relative `NtCreateFile` with reparse refusal on Windows, `Unsupported`
/// (→ [`VerifiedAssetError::Unavailable`]) elsewhere. Never a path read.
pub fn read_verified_listed(root: &File, path: &str, integrity: &str) -> Result<VerifiedAsset, VerifiedAssetError> {
    let bytes =
        super::verified_reader::read_file_under(root, path, MAX_VERIFIED_ASSET_BYTES).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => VerifiedAssetError::Missing,
            std::io::ErrorKind::Unsupported => VerifiedAssetError::Unavailable,
            _ => VerifiedAssetError::Unreadable,
        })?;
    if sri_sha512(&bytes) != integrity {
        return Err(VerifiedAssetError::IntegrityMismatch);
    }
    Ok(VerifiedAsset {
        path: path.to_owned(),
        bytes,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let key = SigningKey::from_bytes(&[23; 32]);
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

    const HTML: &[u8] = b"<html>board</html>";
    const CSS: &[u8] = b"p{color:red}";

    fn signed_files() -> Vec<u8> {
        format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"panels/board.html","integrity":"{}"}},{{"path":"panels/board.css","integrity":"{}"}}],"contributes":{{"panels":[{{"id":"p","title":"P","entryPoint":"panels/board.html"}}]}}}}"#,
            sri_sha512(HTML),
            sri_sha512(CSS)
        )
        .into_bytes()
    }

    fn root_with_files() -> (tempfile::TempDir, File) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("panels")).unwrap();
        std::fs::write(dir.path().join("panels/board.html"), HTML).unwrap();
        std::fs::write(dir.path().join("panels/board.css"), CSS).unwrap();
        let root = File::open(dir.path()).unwrap();
        (dir, root)
    }

    fn serve(
        checked: &SignatureCheckedManifest,
        root: &File,
        req: &str,
    ) -> Result<Option<Vec<u8>>, VerifiedAssetError> {
        let Some(index) = FilesIndex::from_signed(checked)? else {
            return Ok(None);
        };
        let (path, integrity) = index.resolve(Path::new(req))?;
        read_verified_listed(root, path, integrity).map(|a| Some(a.into_bytes()))
    }

    #[test]
    fn serves_entry_and_other_listed_files_with_verified_bytes() {
        let (_d, root) = root_with_files();
        let c = checked(&signed_files());
        assert_eq!(serve(&c, &root, "panels/board.html"), Ok(Some(HTML.to_vec())));
        assert_eq!(serve(&c, &root, "panels/board.css"), Ok(Some(CSS.to_vec())));
    }

    #[test]
    fn unlisted_paths_manifest_directories_and_case_variants_are_refused() {
        let (d, root) = root_with_files();
        std::fs::write(d.path().join("secret.txt"), b"x").unwrap();
        std::fs::write(d.path().join("aion-extension.json"), b"{}").unwrap();
        let c = checked(&signed_files());
        for req in [
            "secret.txt",
            "aion-extension.json",
            "panels",
            "panels/",
            "panels/BOARD.html",
            "panels/board.htm",
            "panels/board.html/x",
            "../secret.txt",
        ] {
            assert_eq!(serve(&c, &root, req), Err(VerifiedAssetError::NotListed), "{req}");
        }
    }

    #[test]
    fn tampered_missing_and_oversize_listed_files_are_refused() {
        let (d, root) = root_with_files();
        let c = checked(&signed_files());
        std::fs::write(d.path().join("panels/board.css"), b"p{color:blue}").unwrap();
        assert_eq!(
            serve(&c, &root, "panels/board.css"),
            Err(VerifiedAssetError::IntegrityMismatch)
        );
        std::fs::remove_file(d.path().join("panels/board.html")).unwrap();
        assert_eq!(serve(&c, &root, "panels/board.html"), Err(VerifiedAssetError::Missing));
        let big = vec![b'a'; MAX_VERIFIED_ASSET_BYTES as usize + 1];
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"big.bin","integrity":"{}"}}]}}"#,
            sri_sha512(&big)
        );
        std::fs::write(d.path().join("big.bin"), &big).unwrap();
        assert_eq!(
            serve(&checked(raw.as_bytes()), &root, "big.bin"),
            Err(VerifiedAssetError::Unreadable)
        );
    }

    #[test]
    fn symlinks_fifos_and_directories_are_never_followed_or_served() {
        let (d, root) = root_with_files();
        let c = checked(&signed_files());
        // Symlinked listed file pointing at a file with matching bytes still refused.
        std::fs::write(d.path().join("real.css"), CSS).unwrap();
        std::fs::remove_file(d.path().join("panels/board.css")).unwrap();
        std::os::unix::fs::symlink(d.path().join("real.css"), d.path().join("panels/board.css")).unwrap();
        assert_eq!(
            serve(&c, &root, "panels/board.css"),
            Err(VerifiedAssetError::Unreadable)
        );
        // Symlinked directory component.
        std::fs::remove_file(d.path().join("panels/board.css")).unwrap();
        std::fs::remove_dir(d.path().join("panels")).unwrap_err(); // still holds board.html
        std::fs::rename(d.path().join("panels"), d.path().join("elsewhere")).unwrap();
        std::os::unix::fs::symlink(d.path().join("elsewhere"), d.path().join("panels")).unwrap();
        assert_eq!(
            serve(&c, &root, "panels/board.html"),
            Err(VerifiedAssetError::Unreadable)
        );
        // Listed path that is a directory.
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","files":[{{"path":"elsewhere","integrity":"{}"}}]}}"#,
            sri_sha512(b"")
        );
        assert_eq!(
            serve(&checked(raw.as_bytes()), &root, "elsewhere"),
            Err(VerifiedAssetError::Unreadable)
        );
    }

    #[test]
    fn snapshot_without_files_is_not_applicable_never_served_here() {
        let (_d, root) = root_with_files();
        let raw = br#"{"name":"demo.plugin","version":"1.0.0"}"#;
        assert_eq!(serve(&checked(raw), &root, "panels/board.html"), Ok(None));
        let raw = br#"{"name":"demo.plugin","version":"1.0.0","files":[]}"#;
        assert_eq!(serve(&checked(raw), &root, "panels/board.html"), Ok(None));
    }

    #[test]
    fn debug_never_leaks_bytes_or_paths() {
        let (_d, root) = root_with_files();
        let c = checked(&signed_files());
        let index = FilesIndex::from_signed(&c).unwrap().unwrap();
        let (p, i) = index.resolve(Path::new("panels/board.html")).unwrap();
        let asset = read_verified_listed(&root, p, i).unwrap();
        let dbg = format!("{asset:?}");
        assert!(!dbg.contains("board") && !dbg.contains("html"));
    }
}
