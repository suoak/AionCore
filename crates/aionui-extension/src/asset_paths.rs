use std::path::{Component, Path, PathBuf};

/// Return `true` when the asset reference is already a remote URL.
pub(crate) fn is_remote_asset_url(value: &str) -> bool {
    value.starts_with("http://") || value.starts_with("https://")
}

/// Normalize a user-supplied relative asset path and reject traversal.
pub(crate) fn normalize_relative_asset_path(path: &str) -> Option<PathBuf> {
    if path.contains('\\') {
        return None;
    }

    let mut normalized = PathBuf::new();

    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    if normalized.as_os_str().is_empty() {
        return None;
    }

    Some(normalized)
}

/// Exact canonical relative form (`a/b.txt`) used for signed package paths.
///
/// Returns `Some` only when the input already is its own normalization: no `..`,
/// absolute or drive prefix, backslash, NUL, empty string, `./a`, `a//b` or `a/./b`.
pub(crate) fn canonical_relative_path(name: &str) -> Option<String> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return None;
    }
    let normalized = normalize_relative_asset_path(name)?;
    let joined = normalized.to_str()?.replace(std::path::MAIN_SEPARATOR, "/");
    (joined == name).then_some(joined)
}

/// Convert a normalized relative path into a URL path with forward slashes.
pub(crate) fn normalized_asset_url_path(path: &str) -> Option<String> {
    let normalized = normalize_relative_asset_path(path)?;
    Some(
        normalized
            .components()
            .filter_map(|component| match component {
                Component::Normal(part) => Some(part.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/"),
    )
}

/// Resolve an extension-scoped asset reference into a backend-served URL.
pub(crate) fn resolve_extension_asset_url(extension_name: &str, raw: &str) -> Option<String> {
    if is_remote_asset_url(raw) {
        return Some(raw.to_owned());
    }

    let relative = normalized_asset_url_path(raw)?;
    Some(format!("/api/extensions/{extension_name}/assets/{relative}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_relative_path_accepts_only_exact_canonical_form() {
        assert_eq!(canonical_relative_path("a/b.txt").as_deref(), Some("a/b.txt"));
        for bad in [
            "", "./a", "a//b", "a/./b", "../a", "/a", "a/", "a\\b", "a\0b", "a/../b", "C:\\x",
        ] {
            assert!(canonical_relative_path(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn remote_asset_url_detects_http_and_https() {
        assert!(is_remote_asset_url("http://example.com/icon.png"));
        assert!(is_remote_asset_url("https://example.com/icon.png"));
        assert!(!is_remote_asset_url("/local/icon.png"));
    }

    #[test]
    fn normalize_relative_asset_path_rejects_traversal_and_absolute_paths() {
        assert!(normalize_relative_asset_path("../secret.txt").is_none());
        assert!(normalize_relative_asset_path("/etc/passwd").is_none());
        assert!(normalize_relative_asset_path("C:\\Windows\\System32").is_none());
    }

    #[test]
    fn normalize_relative_asset_path_preserves_nested_relative_paths() {
        let path = normalize_relative_asset_path("./settings/ui/index.html").unwrap();
        assert_eq!(path, PathBuf::from("settings/ui/index.html"));
        assert_eq!(
            normalized_asset_url_path("./settings/ui/index.html").as_deref(),
            Some("settings/ui/index.html")
        );
    }
}
