//! Listing authorization for panel / work-panel-view contributions (M9 E13 slice B3).
//!
//! `GET /api/extensions/panels` and `/work-panel-views` (and the panel hint of a command invoke)
//! only list an entry when the **Host-retained signed snapshot** vouches for it:
//!
//! 1. the extension is *bound* (it has a signature-checked snapshot in the binding store),
//! 2. that snapshot declares `files` and lists the entry-point path **exactly** (the same
//!    case-sensitive lookup the verified asset serve uses, so the page the UI would load is
//!    served from the retained root with a per-file SRI check),
//! 3. the contribution `id` is unique in the signed manifest,
//! 4. the entry point is a local canonical relative path (never a remote URL), and
//! 5. the URL the registry resolved for the entry equals the one derived from the signed entry
//!    point, so a divergent loaded manifest cannot point the UI somewhere else.
//!
//! Everything else is not listed (fail closed). This is a pure function of signed bytes and the
//! resolved item: no filesystem access, no response-shape change, not a load or execution
//! permit by itself.
use super::signed_manifest::SignatureCheckedManifest;
use super::verified_asset::FilesIndex;
use crate::asset_paths::{canonical_relative_path, is_remote_asset_url, resolve_extension_asset_url};
use crate::manifest::parse_manifest;
use crate::signed_root_binding::SignedRootBindingStore;
use std::path::Path;

/// Which `contributes` array is being authorized.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContributionKind {
    Panel,
    WorkPanelView,
}

/// Strip the registry's `ext-{extension}-` prefix from a resolved contribution id.
fn raw_contribution_id<'a>(extension_name: &str, resolved_id: &'a str) -> Option<&'a str> {
    resolved_id
        .strip_prefix("ext-")?
        .strip_prefix(extension_name)?
        .strip_prefix('-')
        .filter(|rest| !rest.is_empty())
}

/// `true` iff the signed snapshot authorizes listing this resolved contribution (see module docs).
pub fn listing_authorized_by_snapshot(
    checked: &SignatureCheckedManifest,
    kind: ContributionKind,
    extension_name: &str,
    resolved_id: &str,
    resolved_entry_url: &str,
) -> bool {
    let Some(raw_id) = raw_contribution_id(extension_name, resolved_id) else {
        return false;
    };
    let Ok(manifest) = parse_manifest(checked.raw_bytes()) else {
        return false;
    };
    let Some(contributes) = manifest.contributes.as_ref() else {
        return false;
    };
    let entries: Vec<&str> = match kind {
        ContributionKind::Panel => contributes
            .panels
            .iter()
            .filter(|p| p.id == raw_id)
            .map(|p| p.entry_point.as_str())
            .collect(),
        ContributionKind::WorkPanelView => contributes
            .work_panel_views
            .iter()
            .filter(|v| v.id == raw_id)
            .map(|v| v.entry_point.as_str())
            .collect(),
    };
    let [entry] = entries.as_slice() else {
        return false; // missing or ambiguous id
    };
    if entry.is_empty() || is_remote_asset_url(entry) {
        return false;
    }
    let Some(canonical) = canonical_relative_path(entry) else {
        return false;
    };
    let Ok(Some(index)) = FilesIndex::from_signed(checked) else {
        return false;
    };
    if index.resolve(Path::new(&canonical)).is_err() {
        return false;
    }
    resolve_extension_asset_url(extension_name, &canonical).as_deref() == Some(resolved_entry_url)
}

/// Store-backed form used by the routes: unbound extensions are never listed.
pub fn listing_authorized(
    store: &SignedRootBindingStore,
    kind: ContributionKind,
    extension_name: &str,
    resolved_id: &str,
    resolved_entry_url: &str,
) -> bool {
    store.lookup_checked(extension_name).is_some_and(|checked| {
        listing_authorized_by_snapshot(&checked, kind, extension_name, resolved_id, resolved_entry_url)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use serde_json::{Value, json};
    use std::collections::{HashMap, HashSet};

    const URL: &str = "/api/extensions/hello/assets/panels/board.html";

    fn check(manifest: &Value) -> SignatureCheckedManifest {
        try_check(manifest).unwrap()
    }

    fn try_check(
        manifest: &Value,
    ) -> Result<SignatureCheckedManifest, crate::hub::signed_manifest::SignedManifestError> {
        let raw = serde_json::to_vec(manifest).unwrap();
        let key = SigningKey::from_bytes(&[23; 32]);
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
    }

    fn files(paths: &[&str]) -> Value {
        Value::Array(
            paths
                .iter()
                .map(|p| json!({"path": p, "integrity": sri_sha512(p.as_bytes())}))
                .collect(),
        )
    }

    fn manifest(files_value: Option<Value>, panels: Value, views: Value) -> Value {
        let mut m = json!({
            "name": "hello", "version": "1.0.0",
            "contributes": {"panels": panels, "workPanelViews": views},
        });
        if let Some(f) = files_value {
            m["files"] = f;
        }
        m
    }

    fn panel(id: &str, entry: &str) -> Value {
        json!({"id": id, "title": "T", "entryPoint": entry})
    }

    fn auth(m: &Value, kind: ContributionKind, id: &str, url: &str) -> bool {
        listing_authorized_by_snapshot(&check(m), kind, "hello", id, url)
    }

    #[test]
    fn listed_entry_is_authorized_for_both_kinds() {
        let m = manifest(
            Some(files(&["panels/board.html"])),
            json!([panel("board", "panels/board.html")]),
            json!([panel("view", "panels/board.html")]),
        );
        assert!(auth(&m, ContributionKind::Panel, "ext-hello-board", URL));
        assert!(auth(&m, ContributionKind::WorkPanelView, "ext-hello-view", URL));
    }

    #[test]
    fn kinds_do_not_cross_authorize() {
        let m = manifest(
            Some(files(&["panels/board.html"])),
            json!([panel("board", "panels/board.html")]),
            json!([]),
        );
        assert!(!auth(&m, ContributionKind::WorkPanelView, "ext-hello-board", URL));
    }

    #[test]
    fn no_files_index_is_not_authorized() {
        let m = manifest(None, json!([panel("board", "panels/board.html")]), json!([]));
        assert!(!auth(&m, ContributionKind::Panel, "ext-hello-board", URL));
        let m = manifest(None, json!([]), json!([panel("view", "panels/board.html")]));
        assert!(!auth(&m, ContributionKind::WorkPanelView, "ext-hello-view", URL));
    }

    #[test]
    fn remote_empty_and_noncanonical_entries_without_files_are_not_authorized() {
        for entry in [
            "https://evil.example/x.html",
            "",
            "./panels/board.html",
            "panels//board.html",
        ] {
            let m = manifest(None, json!([panel("board", entry)]), json!([]));
            assert!(
                !auth(&m, ContributionKind::Panel, "ext-hello-board", URL),
                "entry {entry:?}"
            );
        }
    }

    /// The signed-manifest layer already refuses a declared `files` list that does not contain a
    /// contribution entry (case-sensitive), remote or non-canonical entry points, so such a snapshot
    /// can never exist; the listing check above is defense in depth behind that guarantee.
    #[test]
    fn manifest_layer_refuses_snapshots_the_listing_check_would_reject() {
        let cases = [
            ("panels/other.html", "panels/board.html"),
            ("panels/Board.html", "panels/board.html"),
            ("panels/board.html", "https://evil.example/x.html"),
            ("panels/board.html", "./panels/board.html"),
            ("panels/board.html", "panels//board.html"),
        ];
        for (listed, entry) in cases {
            let m = manifest(Some(files(&[listed])), json!([panel("board", entry)]), json!([]));
            assert!(try_check(&m).is_err(), "listed {listed:?} entry {entry:?}");
        }
    }

    #[test]
    fn duplicate_or_missing_ids_and_bad_prefixes_are_not_authorized() {
        let dup = manifest(
            Some(files(&["panels/board.html"])),
            json!([panel("board", "panels/board.html"), panel("board", "panels/board.html")]),
            json!([]),
        );
        assert!(!auth(&dup, ContributionKind::Panel, "ext-hello-board", URL));
        let ok = manifest(
            Some(files(&["panels/board.html"])),
            json!([panel("board", "panels/board.html")]),
            json!([]),
        );
        for id in [
            "ext-hello-missing",
            "board",
            "ext-hello-",
            "ext-other-board",
            "ext-hello",
        ] {
            assert!(!auth(&ok, ContributionKind::Panel, id, URL), "id {id:?}");
        }
    }

    #[test]
    fn resolved_url_must_match_the_signed_entry() {
        let m = manifest(
            Some(files(&["panels/board.html"])),
            json!([panel("board", "panels/board.html")]),
            json!([]),
        );
        for url in [
            "/api/extensions/hello/assets/panels/other.html",
            "/api/extensions/evil/assets/panels/board.html",
            "https://evil.example/board.html",
            "",
        ] {
            assert!(
                !auth(&m, ContributionKind::Panel, "ext-hello-board", url),
                "url {url:?}"
            );
        }
    }

    #[test]
    fn extension_names_containing_dashes_resolve_their_ids() {
        assert_eq!(raw_contribution_id("a-b", "ext-a-b-board"), Some("board"));
        assert_eq!(raw_contribution_id("a-b", "ext-a-board"), None);
        assert_eq!(raw_contribution_id("a", "ext-a-b-board"), Some("b-board"));
    }

    #[test]
    fn unbound_store_lists_nothing() {
        let store = SignedRootBindingStore::default();
        assert!(!listing_authorized(
            &store,
            ContributionKind::Panel,
            "hello",
            "ext-hello-board",
            URL
        ));
    }
}
