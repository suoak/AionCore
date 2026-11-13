use std::collections::HashMap;
use std::fs::File;
use std::path::Path;

use tracing::warn;

use crate::asset_paths::canonical_relative_path;
use crate::error::ExtensionError;
use crate::hub::signed_manifest::SignatureCheckedManifest;
use crate::hub::{FilesIndex, read_verified_listed};
use crate::types::I18nConfig;

/// Retained-root view of a bound extension (signed snapshot + dup'd root FD).
pub(crate) type VerifiedRoot<'a> = (&'a SignatureCheckedManifest, &'a File);

/// Relative path of a locale file: `{directory}/{locale}.json`.
pub(crate) fn locale_file_path(config: &I18nConfig, locale: &str) -> String {
    format!("{}/{locale}.json", config.directory)
}

/// Load i18n messages for a specific locale from an extension.
///
/// Looks for `{ext_dir}/{directory}/{locale}.json` where `directory` defaults to "i18n".
/// Returns a flat key-value map of message strings.
pub fn load_extension_i18n(
    i18n_config: &I18nConfig,
    locale: &str,
    extension_name: &str,
    ext_dir: &Path,
) -> Result<HashMap<String, String>, ExtensionError> {
    load_extension_i18n_from(i18n_config, locale, extension_name, ext_dir, None)
}

/// Like [`load_extension_i18n`]. When `verified` is a binding whose signed snapshot declares
/// `files`, the locale file is read only through the retained root (listed path, `O_NOFOLLOW`,
/// SRI of the bytes read). A bound locale file that is unlisted, missing, mismatched, a symlink
/// or unreadable is an error (never an empty map, never a read from `ext_dir`). Unbound /
/// no-`files` keep the legacy directory read.
pub(crate) fn load_extension_i18n_from(
    i18n_config: &I18nConfig,
    locale: &str,
    extension_name: &str,
    ext_dir: &Path,
    verified: Option<VerifiedRoot<'_>>,
) -> Result<HashMap<String, String>, ExtensionError> {
    if !i18n_config.locales.contains(&locale.to_owned()) {
        return Ok(HashMap::new());
    }

    if let Some((checked, root)) = verified
        && let Some(index) = FilesIndex::from_signed(checked).map_err(|e| unverified(extension_name, e.reason()))?
    {
        return load_verified(&index, root, i18n_config, locale, extension_name);
    }

    let i18n_dir = ext_dir.join(&i18n_config.directory);
    let file_path = i18n_dir.join(format!("{locale}.json"));

    if !file_path.exists() {
        tracing::debug!(
            extension = extension_name,
            locale = locale,
            path = %file_path.display(),
            "i18n file not found, returning empty map"
        );
        return Ok(HashMap::new());
    }

    let content = std::fs::read_to_string(&file_path)?;
    let messages: HashMap<String, String> =
        serde_json::from_str(&content).map_err(|e| ExtensionError::ResolutionFailed {
            extension_name: extension_name.to_owned(),
            reason: format!("Invalid i18n JSON for locale '{locale}': {e}"),
        })?;

    Ok(messages)
}

fn unverified(extension_name: &str, reason: &str) -> ExtensionError {
    ExtensionError::ResolutionFailed {
        extension_name: extension_name.to_owned(),
        reason: format!("i18n file could not be verified: {reason}"),
    }
}

fn load_verified(
    index: &FilesIndex,
    root: &File,
    config: &I18nConfig,
    locale: &str,
    extension_name: &str,
) -> Result<HashMap<String, String>, ExtensionError> {
    let joined = locale_file_path(config, locale);
    let canonical = canonical_relative_path(&joined)
        .ok_or_else(|| unverified(extension_name, "locale path is not a canonical relative path"))?;
    let (listed, integrity) = index
        .resolve(Path::new(&canonical))
        .map_err(|e| unverified(extension_name, e.reason()))?;
    let asset = read_verified_listed(root, listed, integrity).map_err(|e| unverified(extension_name, e.reason()))?;
    serde_json::from_slice(asset.bytes()).map_err(|_| ExtensionError::ResolutionFailed {
        extension_name: extension_name.to_owned(),
        reason: format!("Invalid i18n JSON for locale '{locale}'"),
    })
}

/// Load i18n data for a given locale across multiple extensions.
///
/// Returns `HashMap<extension_name, HashMap<key, value>>`.
pub fn resolve_i18n_for_locale(
    extensions: &[(String, Option<I18nConfig>, String)], // (name, i18n_config, ext_dir)
    locale: &str,
) -> HashMap<String, HashMap<String, String>> {
    let with_roots: Vec<_> = extensions
        .iter()
        .map(|(n, c, d)| (n.clone(), c.clone(), d.clone(), None))
        .collect();
    resolve_i18n_for_locale_bound(&with_roots, locale)
}

/// Owned retained-root binding for one extension (checked snapshot + dup'd root FD).
pub(crate) type OwnedBinding = Option<(SignatureCheckedManifest, File)>;

pub(crate) fn resolve_i18n_for_locale_bound(
    extensions: &[(String, Option<I18nConfig>, String, OwnedBinding)],
    locale: &str,
) -> HashMap<String, HashMap<String, String>> {
    let mut result = HashMap::new();

    for (name, i18n_config, ext_dir, binding) in extensions {
        let Some(config) = i18n_config else {
            continue;
        };

        let verified = binding.as_ref().map(|(checked, root)| (checked, root));
        match load_extension_i18n_from(config, locale, name, Path::new(ext_dir), verified) {
            Ok(messages) if !messages.is_empty() => {
                result.insert(name.clone(), messages);
            }
            Ok(_) => {}
            Err(e) => {
                warn!(
                    extension = name.as_str(),
                    locale = locale,
                    "Failed to load i18n data: {e}"
                );
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_i18n_config(locales: Vec<&str>) -> I18nConfig {
        I18nConfig {
            locales: locales.into_iter().map(String::from).collect(),
            directory: "i18n".to_owned(),
        }
    }

    #[test]
    fn test_load_i18n_supported_locale() {
        let dir = std::env::temp_dir().join("ext_test_i18n_load");
        let i18n_dir = dir.join("i18n");
        std::fs::create_dir_all(&i18n_dir).unwrap();
        std::fs::write(
            i18n_dir.join("en.json"),
            r#"{"greeting": "Hello", "farewell": "Goodbye"}"#,
        )
        .unwrap();

        let config = make_i18n_config(vec!["en", "zh-CN"]);
        let result = load_extension_i18n(&config, "en", "my-ext", &dir).unwrap();

        assert_eq!(result.len(), 2);
        assert_eq!(result["greeting"], "Hello");
        assert_eq!(result["farewell"], "Goodbye");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_load_i18n_unsupported_locale_returns_empty() {
        let config = make_i18n_config(vec!["en"]);
        let result = load_extension_i18n(&config, "fr", "my-ext", Path::new("/tmp")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_load_i18n_file_not_found_returns_empty() {
        let dir = std::env::temp_dir().join("ext_test_i18n_missing");
        std::fs::create_dir_all(&dir).unwrap();

        let config = make_i18n_config(vec!["en"]);
        let result = load_extension_i18n(&config, "en", "my-ext", &dir).unwrap();
        assert!(result.is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_load_i18n_invalid_json_returns_error() {
        let dir = std::env::temp_dir().join("ext_test_i18n_bad_json");
        let i18n_dir = dir.join("i18n");
        std::fs::create_dir_all(&i18n_dir).unwrap();
        std::fs::write(i18n_dir.join("en.json"), "not valid json").unwrap();

        let config = make_i18n_config(vec!["en"]);
        let err = load_extension_i18n(&config, "en", "my-ext", &dir).unwrap_err();
        assert!(matches!(err, ExtensionError::ResolutionFailed { .. }));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_resolve_i18n_for_locale_multiple_extensions() {
        let dir1 = std::env::temp_dir().join("ext_test_i18n_multi_1");
        let dir2 = std::env::temp_dir().join("ext_test_i18n_multi_2");
        let i18n1 = dir1.join("i18n");
        let i18n2 = dir2.join("i18n");
        std::fs::create_dir_all(&i18n1).unwrap();
        std::fs::create_dir_all(&i18n2).unwrap();
        std::fs::write(i18n1.join("en.json"), r#"{"key1": "val1"}"#).unwrap();
        std::fs::write(i18n2.join("en.json"), r#"{"key2": "val2"}"#).unwrap();

        let extensions = vec![
            (
                "ext-a".to_owned(),
                Some(make_i18n_config(vec!["en"])),
                dir1.to_string_lossy().into_owned(),
            ),
            (
                "ext-b".to_owned(),
                Some(make_i18n_config(vec!["en"])),
                dir2.to_string_lossy().into_owned(),
            ),
            (
                "ext-c".to_owned(),
                None, // no i18n config
                "/tmp".to_owned(),
            ),
        ];

        let result = resolve_i18n_for_locale(&extensions, "en");
        assert_eq!(result.len(), 2);
        assert_eq!(result["ext-a"]["key1"], "val1");
        assert_eq!(result["ext-b"]["key2"], "val2");

        std::fs::remove_dir_all(&dir1).unwrap();
        std::fs::remove_dir_all(&dir2).unwrap();
    }

    fn checked_with_files(files: serde_json::Value) -> SignatureCheckedManifest {
        use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
        use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
        use ed25519_dalek::{Signer, SigningKey};
        use std::collections::HashSet;
        // The signed snapshot must itself be well-formed: declare exactly the listed locales.
        let locales: Vec<String> = files
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|f| {
                let p = f["path"].as_str()?;
                Some(p.strip_prefix("i18n/")?.strip_suffix(".json")?.to_owned())
            })
            .collect();
        let i18n = if locales.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::json!({"locales": locales, "directory": "i18n"})
        };
        let mut doc = serde_json::json!({"name": "hello", "version": "1.0.0", "files": files});
        if !i18n.is_null() {
            doc["i18n"] = i18n;
        }
        let raw = serde_json::to_vec(&doc).unwrap();
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
    fn bound_files_i18n_reads_retained_root_not_ext_dir() {
        use crate::hub::curated::sri_sha512;
        let ext_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ext_dir.path().join("i18n")).unwrap();
        std::fs::create_dir_all(root_dir.path().join("i18n")).unwrap();
        std::fs::write(ext_dir.path().join("i18n/en.json"), r#"{"k":"EVIL"}"#).unwrap();
        let good = br#"{"k":"good"}"#;
        std::fs::write(root_dir.path().join("i18n/en.json"), good).unwrap();
        std::fs::write(root_dir.path().join("i18n/zh.json"), good).unwrap();
        let checked = checked_with_files(serde_json::json!([
            {"path": "i18n/en.json", "integrity": sri_sha512(good)},
            {"path": "i18n/zh.json", "integrity": sri_sha512(good)},
        ]));
        let root = crate::hub::test_support::open_dir(root_dir.path());
        let config = make_i18n_config(vec!["en", "zh"]);
        let out = load_extension_i18n_from(&config, "en", "hello", ext_dir.path(), Some((&checked, &root))).unwrap();
        assert_eq!(out["k"], "good");
        // Unsupported locale is still just empty.
        let out = load_extension_i18n_from(&config, "fr", "hello", ext_dir.path(), Some((&checked, &root))).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn bound_files_i18n_refuses_unlisted_missing_mismatched_symlink_and_bad_json() {
        use crate::hub::curated::sri_sha512;
        let ext_dir = tempfile::tempdir().unwrap();
        let root_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root_dir.path().join("i18n")).unwrap();
        // The loaded dir has a perfectly valid file for every case: it must never be used.
        std::fs::create_dir_all(ext_dir.path().join("i18n")).unwrap();
        for l in ["en", "zh", "fr"] {
            std::fs::write(ext_dir.path().join(format!("i18n/{l}.json")), r#"{"k":"dir"}"#).unwrap();
        }
        let good = br#"{"k":"good"}"#;
        std::fs::write(root_dir.path().join("i18n/en.json"), good).unwrap();
        let checked = checked_with_files(serde_json::json!([
            {"path": "i18n/en.json", "integrity": sri_sha512(good)},
        ]));
        let root = crate::hub::test_support::open_dir(root_dir.path());
        let config = make_i18n_config(vec!["en", "zh", "fr"]);
        let load = |l: &str| load_extension_i18n_from(&config, l, "hello", ext_dir.path(), Some((&checked, &root)));

        // Declared locale whose file is not listed (and absent on the root).
        assert!(matches!(load("zh"), Err(ExtensionError::ResolutionFailed { .. })));
        assert!(matches!(load("fr"), Err(ExtensionError::ResolutionFailed { .. })));
        // Mismatch.
        std::fs::write(root_dir.path().join("i18n/en.json"), r#"{"k":"tampered"}"#).unwrap();
        assert!(matches!(load("en"), Err(ExtensionError::ResolutionFailed { .. })));
        // Missing listed file is an error, not an empty map.
        std::fs::remove_file(root_dir.path().join("i18n/en.json")).unwrap();
        assert!(matches!(load("en"), Err(ExtensionError::ResolutionFailed { .. })));
        // Symlink with matching bytes.
        std::fs::write(root_dir.path().join("real.json"), good).unwrap();
        assert!(crate::hub::test_support::link_file(
            &root_dir.path().join("real.json"),
            &root_dir.path().join("i18n/en.json")
        ));
        assert!(matches!(load("en"), Err(ExtensionError::ResolutionFailed { .. })));
        // Listed + verified but not a string map.
        let bad = b"[1,2]";
        std::fs::remove_file(root_dir.path().join("i18n/en.json")).unwrap();
        std::fs::write(root_dir.path().join("i18n/en.json"), bad).unwrap();
        let checked_bad = checked_with_files(serde_json::json!([
            {"path": "i18n/en.json", "integrity": sri_sha512(bad)},
        ]));
        assert!(matches!(
            load_extension_i18n_from(&config, "en", "hello", ext_dir.path(), Some((&checked_bad, &root))),
            Err(ExtensionError::ResolutionFailed { .. })
        ));
    }

    #[test]
    fn unbound_or_no_files_binding_keeps_legacy_directory_read() {
        let ext_dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(ext_dir.path().join("i18n")).unwrap();
        std::fs::write(ext_dir.path().join("i18n/en.json"), r#"{"k":"dir"}"#).unwrap();
        let config = make_i18n_config(vec!["en"]);
        assert_eq!(
            load_extension_i18n_from(&config, "en", "hello", ext_dir.path(), None).unwrap()["k"],
            "dir"
        );
        // Snapshot without files: not applicable, legacy read (missing file stays an empty map).
        let checked = checked_with_files(serde_json::json!([]));
        let root_dir = tempfile::tempdir().unwrap();
        let root = crate::hub::test_support::open_dir(root_dir.path());
        assert_eq!(
            load_extension_i18n_from(&config, "en", "hello", ext_dir.path(), Some((&checked, &root))).unwrap()["k"],
            "dir"
        );
        std::fs::remove_file(ext_dir.path().join("i18n/en.json")).unwrap();
        assert!(
            load_extension_i18n_from(&config, "en", "hello", ext_dir.path(), Some((&checked, &root)))
                .unwrap()
                .is_empty()
        );
    }
}
