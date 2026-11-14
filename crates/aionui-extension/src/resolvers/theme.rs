use std::fs::File;
use std::path::Path;

use tracing::warn;

use crate::asset_paths::{canonical_relative_path, resolve_extension_asset_url};
use crate::error::ExtensionError;
use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::hub::{FilesIndex, VerifiedAssetError, read_verified_listed};
use crate::types::{ExtTheme, ResolvedTheme};

/// Resolve a single theme contribution by reading CSS file content.
///
/// The CSS file path is relative to the extension directory. Cover images are resolved to
/// an asset URL. Callers that have a Host-retained signed root should use
/// [`resolve_theme_from`] so a bound+`files` snapshot is read through that root.
pub fn resolve_theme(theme: &ExtTheme, extension_name: &str, ext_dir: &Path) -> Result<ResolvedTheme, ExtensionError> {
    resolve_theme_from(theme, extension_name, ext_dir, None)
}

/// Resolve a theme. When `verified` is a binding whose signed snapshot declares `files`, the CSS
/// is read only through the retained root (`O_NOFOLLOW`, listed SRI). There is no fallback to
/// `ext_dir`. Unbound / no-`files` keep the legacy directory read.
pub fn resolve_theme_from(
    theme: &ExtTheme,
    extension_name: &str,
    ext_dir: &Path,
    verified: Option<(&SignatureCheckedManifest, &File)>,
) -> Result<ResolvedTheme, ExtensionError> {
    let css_content = read_theme_css(&theme.css_file, ext_dir, verified)?;

    let cover_image = theme
        .cover_image
        .as_deref()
        .and_then(|img| resolve_extension_asset_url(extension_name, img));

    Ok(ResolvedTheme {
        extension_name: extension_name.to_owned(),
        id: theme.id.clone(),
        name: theme.name.clone(),
        description: theme.description.clone(),
        css_content,
        cover_image,
    })
}

fn read_theme_css(
    css_file: &str,
    ext_dir: &Path,
    verified: Option<(&SignatureCheckedManifest, &File)>,
) -> Result<String, ExtensionError> {
    if let Some((checked, root)) = verified
        && let Some(index) = FilesIndex::from_signed(checked).map_err(theme_css_unverified)?
    {
        return read_verified_theme_css(&index, root, css_file);
    }
    let css_path = ext_dir.join(css_file);
    if !css_path.exists() {
        return Err(ExtensionError::ThemeCssNotFound(css_path.display().to_string()));
    }
    Ok(std::fs::read_to_string(&css_path)?)
}

fn read_verified_theme_css(index: &FilesIndex, root: &File, css_file: &str) -> Result<String, ExtensionError> {
    let canonical = canonical_relative_path(css_file).ok_or(ExtensionError::ThemeCssUnverified(
        "theme CSS path is not a canonical relative path (theme_css_invalid_path)",
    ))?;
    let (listed, integrity) = index.resolve(Path::new(&canonical)).map_err(theme_css_unverified)?;
    let asset = read_verified_listed(root, listed, integrity).map_err(theme_css_unverified)?;
    String::from_utf8(asset.into_bytes())
        .map_err(|_| ExtensionError::ThemeCssUnverified("theme CSS is not valid UTF-8 (theme_css_not_utf8)"))
}

fn theme_css_unverified(err: VerifiedAssetError) -> ExtensionError {
    ExtensionError::ThemeCssUnverified(err.reason())
}

/// Resolve all theme contributions from an extension.
pub fn resolve_themes(themes: &[ExtTheme], extension_name: &str, ext_dir: &Path) -> Vec<ResolvedTheme> {
    resolve_themes_from(themes, extension_name, ext_dir, None)
}

/// Resolve all themes, using the retained-root verifier when `verified` is a bound+`files` snapshot.
pub fn resolve_themes_from(
    themes: &[ExtTheme],
    extension_name: &str,
    ext_dir: &Path,
    verified: Option<(&SignatureCheckedManifest, &File)>,
) -> Vec<ResolvedTheme> {
    themes
        .iter()
        .filter_map(|t| {
            resolve_theme_from(t, extension_name, ext_dir, verified)
                .map_err(|e| {
                    warn!(
                        extension = extension_name,
                        theme_id = t.id,
                        "Failed to resolve theme: {e}"
                    );
                    e
                })
                .ok()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_theme_reads_css() {
        let dir = std::env::temp_dir().join("ext_test_resolve_theme");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("dark.css"), ":root { --bg: #000; }").unwrap();

        let theme = ExtTheme {
            id: "dark-theme".into(),
            name: "Dark Theme".into(),
            description: Some("A dark theme".into()),
            css_file: "dark.css".into(),
            cover_image: Some("images/dark.png".into()),
        };

        let result = resolve_theme(&theme, "my-ext", &dir).unwrap();

        assert_eq!(result.extension_name, "my-ext");
        assert_eq!(result.id, "dark-theme");
        assert_eq!(result.css_content, ":root { --bg: #000; }");
        assert!(result.cover_image.as_ref().unwrap().contains("images/dark.png"));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_theme_css_not_found() {
        let theme = ExtTheme {
            id: "missing-theme".into(),
            name: "Missing".into(),
            description: None,
            css_file: "nonexistent.css".into(),
            cover_image: None,
        };

        let err = resolve_theme(&theme, "my-ext", Path::new("/tmp/no_such_ext")).unwrap_err();
        assert!(matches!(err, ExtensionError::ThemeCssNotFound(_)));
    }

    #[test]
    fn test_resolve_theme_no_cover_image() {
        let dir = std::env::temp_dir().join("ext_test_resolve_theme_no_cover");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("light.css"), "body { color: #333; }").unwrap();

        let theme = ExtTheme {
            id: "light-theme".into(),
            name: "Light".into(),
            description: None,
            css_file: "light.css".into(),
            cover_image: None,
        };

        let result = resolve_theme(&theme, "my-ext", &dir).unwrap();
        assert!(result.cover_image.is_none());
        assert_eq!(result.css_content, "body { color: #333; }");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_themes_skips_missing_css() {
        let dir = std::env::temp_dir().join("ext_test_resolve_themes_skip");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ok.css"), "ok").unwrap();

        let themes = vec![
            ExtTheme {
                id: "good".into(),
                name: "Good".into(),
                description: None,
                css_file: "ok.css".into(),
                cover_image: None,
            },
            ExtTheme {
                id: "bad".into(),
                name: "Bad".into(),
                description: None,
                css_file: "missing.css".into(),
                cover_image: None,
            },
        ];

        let result = resolve_themes(&themes, "my-ext", &dir);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].id, "good");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn theme(css_file: &str) -> ExtTheme {
        ExtTheme {
            id: "dark".into(),
            name: "Dark".into(),
            description: None,
            css_file: css_file.into(),
            cover_image: None,
        }
    }

    fn checked_with_files(
        files: serde_json::Value,
        themes: serde_json::Value,
    ) -> crate::hub::signed_manifest::SignatureCheckedManifest {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::{HashMap, HashSet};
        let raw = serde_json::to_vec(&serde_json::json!({
            "name": "hello",
            "version": "1.0.0",
            "files": files,
            "contributes": { "themes": themes },
        }))
        .unwrap();
        let key = SigningKey::from_bytes(&[17; 32]);
        let policy = CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key.verifying_key())]),
        );
        let integrity = sri_sha512(&raw);
        let signature = hex::encode(
            key.sign(signature_payload("hello", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name: "hello",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            &raw,
        )
        .unwrap()
    }

    #[test]
    fn bound_files_theme_css_comes_from_retained_root_not_ext_dir() {
        use crate::hub::curated::sri_sha512;
        let ext_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ext_dir.path().join("themes")).unwrap();
        std::fs::create_dir_all(root_dir.path().join("themes")).unwrap();
        std::fs::write(ext_dir.path().join("themes/dark.css"), "EVIL { color: red; }").unwrap();
        std::fs::write(root_dir.path().join("themes/dark.css"), ":root { --bg: #000; }").unwrap();
        let css = b":root { --bg: #000; }";
        let checked = checked_with_files(
            serde_json::json!([{"path": "themes/dark.css", "integrity": sri_sha512(css)}]),
            serde_json::json!([{"id": "dark", "name": "Dark", "css_file": "themes/dark.css"}]),
        );
        let root = crate::hub::test_support::open_dir(root_dir.path());
        let result = resolve_theme_from(
            &theme("themes/dark.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap();
        assert_eq!(result.css_content, ":root { --bg: #000; }");
    }

    #[test]
    fn bound_files_theme_css_refuses_unlisted_mismatched_symlink_and_missing() {
        use crate::hub::curated::sri_sha512;
        let ext_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root_dir.path().join("themes")).unwrap();
        std::fs::write(root_dir.path().join("themes/dark.css"), ":root { --bg: #000; }").unwrap();
        let css = b":root { --bg: #000; }";
        let listed = serde_json::json!([{"path": "themes/dark.css", "integrity": sri_sha512(css)}]);
        let themes = serde_json::json!([{"id": "dark", "name": "Dark", "css_file": "themes/dark.css"}]);
        let checked = checked_with_files(listed, themes.clone());
        let root = crate::hub::test_support::open_dir(root_dir.path());

        // Unlisted path (even if the bytes exist on the root).
        let err = resolve_theme_from(
            &theme("themes/other.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap_err();
        assert!(matches!(err, ExtensionError::ThemeCssUnverified(_)), "{err:?}");

        // Integrity mismatch: overwrite retained bytes after the list was signed.
        std::fs::write(root_dir.path().join("themes/dark.css"), "p{color:blue}").unwrap();
        let err = resolve_theme_from(
            &theme("themes/dark.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap_err();
        assert!(matches!(err, ExtensionError::ThemeCssUnverified(_)), "{err:?}");

        // Missing listed file.
        std::fs::remove_file(root_dir.path().join("themes/dark.css")).unwrap();
        let err = resolve_theme_from(
            &theme("themes/dark.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap_err();
        assert!(matches!(err, ExtensionError::ThemeCssUnverified(_)), "{err:?}");

        // Symlink with matching bytes is still refused; ext_dir is never consulted.
        std::fs::write(ext_dir.path().join("dark.css"), css).unwrap();
        std::fs::write(root_dir.path().join("real.css"), css).unwrap();
        assert!(crate::hub::test_support::link_file(
            &root_dir.path().join("real.css"),
            &root_dir.path().join("themes/dark.css")
        ));
        let err = resolve_theme_from(
            &theme("themes/dark.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap_err();
        assert!(matches!(err, ExtensionError::ThemeCssUnverified(_)), "{err:?}");
    }

    #[test]
    fn bound_files_theme_css_refuses_non_utf8() {
        use crate::hub::curated::sri_sha512;
        let bytes: &[u8] = &[0xff, 0xfe, b' ', b'x'];
        let ext_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root_dir.path().join("themes")).unwrap();
        std::fs::write(root_dir.path().join("themes/dark.css"), bytes).unwrap();
        let checked = checked_with_files(
            serde_json::json!([{"path": "themes/dark.css", "integrity": sri_sha512(bytes)}]),
            serde_json::json!([{"id": "dark", "name": "Dark", "css_file": "themes/dark.css"}]),
        );
        let root = crate::hub::test_support::open_dir(root_dir.path());
        let err = resolve_theme_from(
            &theme("themes/dark.css"),
            "hello",
            ext_dir.path(),
            Some((&checked, &root)),
        )
        .unwrap_err();
        assert!(
            matches!(err, ExtensionError::ThemeCssUnverified(r) if r.contains("UTF-8")),
            "{err:?}"
        );
        let _ = ext_dir;
    }

    #[test]
    fn unbound_or_no_files_theme_css_still_reads_ext_dir() {
        let ext_dir = tempfile::tempdir().unwrap();
        std::fs::write(ext_dir.path().join("legacy.css"), "body { color: #333; }").unwrap();
        let result = resolve_theme(&theme("legacy.css"), "hello", ext_dir.path()).unwrap();
        assert_eq!(result.css_content, "body { color: #333; }");

        // Binding whose snapshot has no files keeps the legacy read.
        let checked = checked_with_files(
            serde_json::json!([]),
            serde_json::json!([{"id": "dark", "name": "Dark", "css_file": "legacy.css"}]),
        );
        // empty files list: from_signed returns None only when files is empty; parse of empty
        // files is allowed (legacy). Need a valid empty list? files: [] is empty → Ok(None).
        let root_dir = tempfile::tempdir().unwrap();
        let root = crate::hub::test_support::open_dir(root_dir.path());
        let result =
            resolve_theme_from(&theme("legacy.css"), "hello", ext_dir.path(), Some((&checked, &root))).unwrap();
        assert_eq!(result.css_content, "body { color: #333; }");
    }
}
