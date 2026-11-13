//! Built-in (curated, shipped-with-the-app) signed packs (M9 E13 slices B1 / S3b-1).
//!
//! A [`BuiltinPack`] is a signed package zip that is compiled into the app together with the
//! public key it is signed with ([`BuiltinKey`]). At Host start, after the sweep + restore,
//! `HostSignedInstall::ensure_builtin_packs` installs each one through the **same** verified chain
//! as any other signed package (`check_signed_manifest`, whole-archive validation, per-file SRIs,
//! handle-relative install, binding). Nothing here reads a path, trusts a directory or runs code.
//!
//! # Policy
//! * **The app binary is the only authority (design §T.1 / §T.7).** Every built-in name is pinned
//!   by [`BuiltinPins`] to the exact signed identity the app ships: name, version, key id, the
//!   embedded key bytes and the manifest SRI (`integrity`, which in turn pins every file through the
//!   signed `files` list). An installed copy of a built-in name is accepted (restored, or left as
//!   is by `ensure`) only when the signature verifies **and** it equals that pin — either alone is
//!   not enough. App update ⇒ the newer pin replaces the old copy; app rollback ⇒ the pinned older
//!   version replaces the newer copy (downgrade is allowed for built-in names only; every other
//!   package still gets `DowngradeRefused`).
//! * **Name reservation (§T.8).** A built-in name (compared ASCII case-insensitively, so look-alike
//!   names are reserved too) can only ever hold the pinned built-in: a same-named package from the
//!   hub or any other signer is refused at install, and one found on disk is not restored.
//! * **Self-healing.** A built-in that failed restore (tampered file, bad marker, pin mismatch,
//!   missing binding) is unbound at that point, so `ensure` reinstalls it from the embedded,
//!   signature-checked copy.
//! * **Fail closed on keys.** The built-in public keys are merged into the Host signature policy by
//!   [`merge_builtin_policy`]. A key id that the Host already uses for a *different* key is never
//!   overridden: the built-in key is dropped and every pack signed with it is skipped (its name
//!   stays reserved, so nothing else can take it either). The embedded key bytes must be a valid
//!   Ed25519 point, otherwise the key is dropped.
//! * **No ambient key.** The library contains no key and no pack; the app supplies the set.
//! * An existing allowlist (restrictive when non-empty) gets the built-in pack names added, so
//!   restore keeps accepting them; the built-in install itself ignores the soft allowlist and
//!   relies on the signature and the pin.
//! * A user uninstall of a built-in lasts until the next start (it is ensured again); disabling it
//!   through the normal extension state is the supported way to hide it.
//! * The hub index `bundled` flag plays no part in any of this (it is a writable on-disk flag).
//!
//! Outcomes carry fixed categories and the built-in's own (compile-time) name only.

use std::collections::HashSet;

use ed25519_dalek::VerifyingKey;

use super::curated::{CuratedInstallPolicy, sri_sha512};
use super::host_package_install::InstallKind;
use super::local_package_install::LocalPackageError;
use super::signed_manifest::SignatureCheckedManifest;

/// One compiled-in public key (Ed25519, 32 bytes) with the id packs reference it by.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinKey<'a> {
    pub id: &'a str,
    pub public_key: [u8; 32],
}

/// One compiled-in signed pack: the same metadata a hub index entry carries, plus the zip bytes.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinPack<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub pubkey_id: &'a str,
    pub integrity: &'a str,
    pub signature: &'a str,
    pub package_zip: &'a [u8],
}

/// The keys and packs the app ships. [`BuiltinSet::EMPTY`] is the production value until the owner
/// provisions a production key.
#[derive(Debug, Clone, Copy)]
pub struct BuiltinSet<'a> {
    pub keys: &'a [BuiltinKey<'a>],
    pub packs: &'a [BuiltinPack<'a>],
}

impl BuiltinSet<'static> {
    pub const EMPTY: BuiltinSet<'static> = BuiltinSet { keys: &[], packs: &[] };
}

impl BuiltinSet<'_> {
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty() && self.packs.is_empty()
    }
}

/// What [`merge_builtin_policy`] had to drop (key ids only, never key bytes).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeReport {
    /// Built-in key ids that collide with a different Host key of the same id.
    pub conflicting_key_ids: Vec<String>,
    /// Built-in key ids whose bytes are not a valid Ed25519 public key.
    pub invalid_key_ids: Vec<String>,
}

/// Merge the built-in keys (and, for a restrictive allowlist, the pack names) into the Host policy.
///
/// `require_signature` is untouched. With an empty set the policy is returned unchanged, so an app
/// with no built-ins and no configured policy stays "not configured".
pub fn merge_builtin_policy(
    mut policy: CuratedInstallPolicy,
    set: &BuiltinSet<'_>,
) -> (CuratedInstallPolicy, MergeReport) {
    let mut report = MergeReport::default();
    for key in set.keys {
        let Ok(verifying) = VerifyingKey::from_bytes(&key.public_key) else {
            report.invalid_key_ids.push(key.id.to_owned());
            continue;
        };
        match policy.pubkeys.get(key.id) {
            Some(existing) if existing.to_bytes() != verifying.to_bytes() => {
                report.conflicting_key_ids.push(key.id.to_owned());
            }
            Some(_) => {}
            None => {
                policy.pubkeys.insert(key.id.to_owned(), verifying);
            }
        }
    }
    if !policy.allowlist.is_empty() {
        for pack in set.packs {
            policy.allowlist.insert(pack.name.to_owned());
        }
    }
    (policy, report)
}

/// Why a built-in was not installed (nothing was changed).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinSkip {
    /// The pack's key id is not in the Host policy with the embedded key bytes (unknown, invalid or
    /// colliding with a different Host key).
    KeyUnavailable,
    /// The pack is not exactly the identity the Host was pinned with (`HostSignedInstall::
    /// with_builtin_pins` was given a different set, or the set names one pack twice).
    NotPinned,
}

/// Per-pack result of `ensure_builtin_packs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinOutcome {
    /// Installed or upgraded (or reinstalled to repair an unbound copy).
    Installed(InstallKind),
    /// Exactly the pinned built-in is already bound.
    AlreadyCurrent,
    Skipped(BuiltinSkip),
    /// The verified install chain refused the pack (nothing replaced the previous state).
    Failed(LocalPackageError),
}

/// Outcomes in the order of the set's packs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuiltinReport {
    pub outcomes: Vec<(String, BuiltinOutcome)>,
}

impl BuiltinReport {
    pub fn installed(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|(_, o)| matches!(o, BuiltinOutcome::Installed(_)))
            .count()
    }
}

/// One built-in identity exactly as the app binary ships it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct BuiltinPin {
    name: String,
    version: String,
    pubkey_id: String,
    /// The embedded key bytes for `pubkey_id`; `None` when the set ships no key of that id.
    public_key: Option<[u8; 32]>,
    integrity: String,
}

/// How an identity relates to the app's built-in pins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinVerdict {
    /// The name is not a built-in name (not even ASCII case-insensitively).
    NotReserved,
    /// Exactly the pinned built-in: name, version, key id, Host key bytes and integrity all match.
    Pinned,
    /// A built-in name (or a case variant of one) with anything else: refuse it.
    Mismatch,
}

/// The built-in names the app reserves, each pinned to the exact signed identity it ships
/// (design §T.1 double pin, §T.7 binary authority, §T.8 name reservation).
///
/// Owned (no borrow of the `'static` set) so the Host context can hold it. The default is empty:
/// nothing is reserved and every verdict is [`PinVerdict::NotReserved`].
#[derive(Debug, Clone, Default)]
pub struct BuiltinPins {
    pins: Vec<BuiltinPin>,
    /// Names the set lists more than once with different metadata: reserved, never pinned.
    conflicted: HashSet<String>,
}

impl BuiltinPins {
    /// Pin every pack in `set` (whether or not its key ended up usable: an unusable key leaves the
    /// name reserved with no acceptable identity, which is the fail-closed outcome).
    pub fn from_set(set: &BuiltinSet<'_>) -> Self {
        let mut out = Self::default();
        for pack in set.packs {
            let pin = BuiltinPin {
                name: pack.name.to_owned(),
                version: pack.version.to_owned(),
                pubkey_id: pack.pubkey_id.to_owned(),
                public_key: set.keys.iter().find(|k| k.id == pack.pubkey_id).map(|k| k.public_key),
                integrity: pack.integrity.to_owned(),
            };
            match out.pins.iter().find(|p| p.name == pin.name) {
                Some(existing) if *existing != pin => {
                    out.conflicted.insert(pin.name);
                }
                Some(_) => {}
                None => out.pins.push(pin),
            }
        }
        out
    }

    pub fn is_empty(&self) -> bool {
        self.pins.is_empty()
    }

    /// Whether `name` is a built-in name, compared ASCII case-insensitively (a case variant of a
    /// built-in name is reserved too: it would share the directory on a case-insensitive FS and
    /// is a look-alike everywhere else).
    pub fn is_reserved(&self, name: &str) -> bool {
        self.pins.iter().any(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// Verdict for signed metadata (marker or index entry). `Pinned` also requires the Host policy
    /// to hold the pinned key id with exactly the embedded key bytes.
    pub fn verdict(
        &self,
        policy: &CuratedInstallPolicy,
        name: &str,
        version: &str,
        pubkey_id: &str,
        integrity: &str,
    ) -> PinVerdict {
        if !self.is_reserved(name) {
            return PinVerdict::NotReserved;
        }
        if self.conflicted.contains(name) {
            return PinVerdict::Mismatch;
        }
        let Some(pin) = self.pins.iter().find(|p| p.name == name) else {
            // Only a case variant of a built-in name.
            return PinVerdict::Mismatch;
        };
        let key_matches = pin.public_key.is_some_and(|embedded| {
            policy
                .pubkeys
                .get(&pin.pubkey_id)
                .is_some_and(|host| host.to_bytes() == embedded)
        });
        if key_matches && pin.version == version && pin.pubkey_id == pubkey_id && pin.integrity == integrity {
            PinVerdict::Pinned
        } else {
            PinVerdict::Mismatch
        }
    }

    /// Verdict for a signature-checked snapshot (a live binding): its integrity is recomputed from
    /// the checked manifest bytes, nothing recorded next to it is trusted.
    pub fn verdict_checked(&self, policy: &CuratedInstallPolicy, checked: &SignatureCheckedManifest) -> PinVerdict {
        self.verdict(
            policy,
            checked.name(),
            checked.version(),
            checked.key_id(),
            &sri_sha512(checked.raw_bytes()),
        )
    }

    /// Verdict for a built-in pack itself (is it exactly what the Host was pinned with?).
    pub(super) fn verdict_pack(&self, policy: &CuratedInstallPolicy, pack: &BuiltinPack<'_>) -> PinVerdict {
        self.verdict(policy, pack.name, pack.version, pack.pubkey_id, pack.integrity)
    }
}

/// Key ids the host policy holds with exactly the embedded bytes (the only ones usable for packs).
pub(super) fn usable_key_ids(policy: &CuratedInstallPolicy, set: &BuiltinSet<'_>) -> HashSet<String> {
    set.keys
        .iter()
        .filter(|k| {
            policy
                .pubkeys
                .get(k.id)
                .is_some_and(|installed| installed.to_bytes() == k.public_key)
        })
        .map(|k| k.id.to_owned())
        .collect()
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::*;
    use crate::hub::curated::{signature_payload, sri_sha512};
    use crate::hub::host_package_install::HostExtensionsParent;
    use crate::hub::host_package_restore::RestoreRejection;
    use crate::hub::host_signed_install::HostSignedInstall;
    use crate::hub::local_package_install::LocalPackageError;
    use crate::hub::signed_manifest::SignedManifestError;
    use crate::hub::test_support::open_host_dir;
    use crate::pack::{PackOptions, pack_extension_dir};
    use crate::signed_root_binding::SignedRootBindingStore;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    const KEY_ID: &str = "builtin-test";

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[41; 32])
    }

    /// An owned built-in pack (the app's real ones borrow `'static` data).
    struct Owned {
        name: String,
        version: String,
        key_id: String,
        integrity: String,
        signature: String,
        zip: Vec<u8>,
    }

    impl Owned {
        fn borrow(&self) -> BuiltinPack<'_> {
            BuiltinPack {
                name: &self.name,
                version: &self.version,
                pubkey_id: &self.key_id,
                integrity: &self.integrity,
                signature: &self.signature,
                package_zip: &self.zip,
            }
        }
    }

    fn make_pack(name: &str, version: &str) -> Owned {
        make_pack_signed(name, version, &signing_key(), KEY_ID)
    }

    /// A pack signed by `key` under `key_id` (a third-party signer when not the built-in key).
    fn make_pack_signed(name: &str, version: &str, key: &SigningKey, key_id: &str) -> Owned {
        let author = tempfile::tempdir().unwrap();
        let write = |rel: &str, bytes: &[u8]| {
            let path = author.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, bytes).unwrap();
        };
        write(
            "aion-extension.json",
            &serde_json::to_vec(&serde_json::json!({
                "name": name, "version": version,
                "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
            }))
            .unwrap(),
        );
        write("panels/board.html", format!("<p>{name} {version}</p>").as_bytes());
        let packed = pack_extension_dir(author.path(), &PackOptions::default()).unwrap();
        let signature = hex::encode(
            key.sign(signature_payload(name, version, &packed.manifest_integrity).as_bytes())
                .to_bytes(),
        );
        Owned {
            name: name.into(),
            version: version.into(),
            key_id: key_id.into(),
            integrity: packed.manifest_integrity,
            signature,
            zip: packed.zip,
        }
    }

    fn key_entry() -> BuiltinKey<'static> {
        BuiltinKey {
            id: KEY_ID,
            public_key: signing_key().verifying_key().to_bytes(),
        }
    }

    fn host_policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(Default::default(), false, HashMap::new())
    }

    struct Env {
        tmp: tempfile::TempDir,
    }

    impl Env {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::create_dir(tmp.path().join("extensions")).unwrap();
            Self { tmp }
        }

        fn parent_path(&self) -> PathBuf {
            self.tmp.path().join("extensions")
        }

        /// A "Host start": fresh binding store over the same directory, merged policy, sweep + restore.
        fn start(
            &self,
            host: CuratedInstallPolicy,
            set: &BuiltinSet<'_>,
        ) -> (
            HostSignedInstall,
            Arc<SignedRootBindingStore>,
            crate::hub::StartupReport,
        ) {
            let (policy, _) = merge_builtin_policy(host, set);
            let store = Arc::new(SignedRootBindingStore::new());
            let parent = HostExtensionsParent::new(open_host_dir(&self.parent_path())).unwrap();
            let ctx = HostSignedInstall::new(parent, policy, store.clone())
                .expect("policy configured")
                .with_builtin_pins(set);
            let report = ctx.startup_sweep_and_restore();
            (ctx, store, report)
        }
    }

    fn set<'a>(keys: &'a [BuiltinKey<'a>], packs: &'a [BuiltinPack<'a>]) -> BuiltinSet<'a> {
        BuiltinSet { keys, packs }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn only(report: &BuiltinReport) -> BuiltinOutcome {
        assert_eq!(report.outcomes.len(), 1);
        report.outcomes[0].1
    }

    // ---- merge_builtin_policy ----

    #[test]
    fn empty_set_leaves_the_policy_unchanged_and_unconfigured() {
        let (policy, report) = merge_builtin_policy(host_policy(), &BuiltinSet::EMPTY);
        assert!(!crate::hub::signature_policy_configured(&policy));
        assert_eq!(report, MergeReport::default());
        assert!(BuiltinSet::EMPTY.is_empty());
    }

    #[test]
    fn builtin_key_enables_the_signed_path_and_is_added() {
        let keys = [key_entry()];
        let (policy, report) = merge_builtin_policy(host_policy(), &set(&keys, &[]));
        assert!(crate::hub::signature_policy_configured(&policy));
        assert!(policy.pubkeys.contains_key(KEY_ID));
        assert_eq!(report, MergeReport::default());
    }

    #[test]
    fn same_id_same_key_is_fine_and_a_different_key_is_never_overridden() {
        let keys = [key_entry()];
        let mut host = host_policy();
        host.pubkeys.insert(KEY_ID.into(), signing_key().verifying_key());
        let (_, ok) = merge_builtin_policy(host, &set(&keys, &[]));
        assert_eq!(ok, MergeReport::default());

        let other = SigningKey::from_bytes(&[42; 32]).verifying_key();
        let mut host = host_policy();
        host.pubkeys.insert(KEY_ID.into(), other);
        let (policy, report) = merge_builtin_policy(host, &set(&keys, &[]));
        assert_eq!(report.conflicting_key_ids, vec![KEY_ID.to_owned()]);
        assert_eq!(policy.pubkeys[KEY_ID].to_bytes(), other.to_bytes());
    }

    #[test]
    fn invalid_key_bytes_are_dropped() {
        // y = 2 is not on the curve (no square root), so it does not decode to a point.
        let mut bad = [0u8; 32];
        bad[0] = 2;
        assert!(VerifyingKey::from_bytes(&bad).is_err());
        let keys = [BuiltinKey {
            id: "bad",
            public_key: bad,
        }];
        let (policy, report) = merge_builtin_policy(host_policy(), &set(&keys, &[]));
        assert_eq!(report.invalid_key_ids, vec!["bad".to_owned()]);
        assert!(policy.pubkeys.is_empty());
    }

    #[test]
    fn restrictive_allowlist_gains_the_builtin_names_only_when_non_empty() {
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let mut host = host_policy();
        host.allowlist.insert("other".into());
        let (policy, _) = merge_builtin_policy(host, &set(&keys, &packs));
        assert!(policy.allowlist.contains("demo.builtin") && policy.allowlist.contains("other"));
        let (policy, _) = merge_builtin_policy(host_policy(), &set(&keys, &packs));
        assert!(policy.allowlist.is_empty());
    }

    // ---- ensure_builtin_packs ----

    #[test]
    fn fresh_install_then_second_ensure_is_a_no_op() {
        let env = Env::new();
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let (ctx, store, _) = env.start(host_policy(), &s);
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Installed(InstallKind::Fresh)
        );
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
        let before = names(&env.parent_path().join("demo.builtin"));
        assert_eq!(only(&ctx.ensure_builtin_packs(&s)), BuiltinOutcome::AlreadyCurrent);
        assert_eq!(names(&env.parent_path().join("demo.builtin")), before);
    }

    #[test]
    fn restart_restores_the_builtin_and_ensure_leaves_it() {
        let env = Env::new();
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        {
            let (ctx, _store, _) = env.start(host_policy(), &s);
            ctx.ensure_builtin_packs(&s);
        }
        let (ctx, store, report) = env.start(host_policy(), &s);
        assert_eq!(report.restored, 1);
        assert!(store.contains("demo.builtin"));
        assert_eq!(only(&ctx.ensure_builtin_packs(&s)), BuiltinOutcome::AlreadyCurrent);
    }

    #[test]
    fn app_update_upgrades_and_app_rollback_downgrades_to_the_pinned_version() {
        let env = Env::new();
        let v1 = make_pack("demo.builtin", "1.0.0");
        let v2 = make_pack("demo.builtin", "1.1.0");
        let keys = [key_entry()];
        let (p1, p2) = ([v1.borrow()], [v2.borrow()]);
        let (s1, s2) = (set(&keys, &p1), set(&keys, &p2));
        {
            let (ctx, _store, _) = env.start(host_policy(), &s1);
            ctx.ensure_builtin_packs(&s1);
        }
        // App update: the 1.0.0 copy is not the pin any more, so it is not re-bound; ensure upgrades.
        {
            let (ctx, store, report) = env.start(host_policy(), &s2);
            assert_eq!(report.restored, 0);
            assert_eq!(
                report.rejected,
                vec![("demo.builtin".to_owned(), RestoreRejection::BuiltinPinMismatch)]
            );
            assert_eq!(
                only(&ctx.ensure_builtin_packs(&s2)),
                BuiltinOutcome::Installed(InstallKind::Upgraded)
            );
            assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.1.0");
        }
        // App rollback: the binary is the authority, the built-in goes back to 1.0.0.
        {
            let (ctx, store, report) = env.start(host_policy(), &s1);
            assert_eq!(
                report.rejected,
                vec![("demo.builtin".to_owned(), RestoreRejection::BuiltinPinMismatch)]
            );
            assert_eq!(
                only(&ctx.ensure_builtin_packs(&s1)),
                BuiltinOutcome::Installed(InstallKind::Downgraded)
            );
            assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
        }
        // And it restores cleanly at the next start of the same app.
        let (ctx, store, report) = env.start(host_policy(), &s1);
        assert_eq!((report.restored, report.rejected.len()), (1, 0));
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
        assert_eq!(only(&ctx.ensure_builtin_packs(&s1)), BuiltinOutcome::AlreadyCurrent);
    }

    #[test]
    fn rollback_without_a_restore_still_downgrades_from_the_on_disk_marker() {
        let env = Env::new();
        let v1 = make_pack("demo.builtin", "1.0.0");
        let v2 = make_pack("demo.builtin", "1.1.0");
        let keys = [key_entry()];
        let (p1, p2) = ([v1.borrow()], [v2.borrow()]);
        let (s1, s2) = (set(&keys, &p1), set(&keys, &p2));
        {
            let (ctx, _store, _) = env.start(host_policy(), &s2);
            ctx.ensure_builtin_packs(&s2);
        }
        // Fresh store, no restore: only the on-disk marker knows 1.1.0.
        let (policy, _) = merge_builtin_policy(host_policy(), &s1);
        let store = Arc::new(SignedRootBindingStore::new());
        let parent = HostExtensionsParent::new(open_host_dir(&env.parent_path())).unwrap();
        let ctx = HostSignedInstall::new(parent, policy, store.clone())
            .unwrap()
            .with_builtin_pins(&s1);
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s1)),
            BuiltinOutcome::Installed(InstallKind::Downgraded)
        );
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().version(), "1.0.0");
    }

    // ---- BuiltinPins: double pin + name reservation (§T.1 / §T.7 / §T.8) ----

    fn third_party_key() -> SigningKey {
        SigningKey::from_bytes(&[43; 32])
    }

    /// Host policy that also trusts a third-party signer (e.g. a `WORKMATE_HUB_SIGNING_PUBKEYS` key).
    fn host_policy_with_third_party() -> CuratedInstallPolicy {
        let mut host = host_policy();
        host.pubkeys.insert("third".into(), third_party_key().verifying_key());
        host
    }

    #[test]
    fn pin_verdict_requires_every_pinned_field_and_the_embedded_key_bytes() {
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let pins = BuiltinPins::from_set(&s);
        let (policy, _) = merge_builtin_policy(host_policy(), &s);
        let v = |name: &str, version: &str, key_id: &str, integrity: &str| {
            pins.verdict(&policy, name, version, key_id, integrity)
        };

        assert!(!pins.is_empty());
        assert_eq!(v("demo.builtin", "1.0.0", KEY_ID, &pack.integrity), PinVerdict::Pinned);
        assert_eq!(
            v("demo.other", "1.0.0", KEY_ID, &pack.integrity),
            PinVerdict::NotReserved
        );
        assert_eq!(
            v("demo.builtin", "1.0.1", KEY_ID, &pack.integrity),
            PinVerdict::Mismatch
        );
        assert_eq!(
            v("demo.builtin", "1.0.0", "third", &pack.integrity),
            PinVerdict::Mismatch
        );
        assert_eq!(
            v("demo.builtin", "1.0.0", KEY_ID, &sri_sha512(b"other manifest")),
            PinVerdict::Mismatch
        );
        // A case variant is reserved but can never be the pin.
        assert!(pins.is_reserved("Demo.Builtin"));
        assert_eq!(
            v("Demo.Builtin", "1.0.0", KEY_ID, &pack.integrity),
            PinVerdict::Mismatch
        );

        // The Host holding a different key under the pinned id: nothing is ever pinned.
        let mut colliding = host_policy();
        colliding
            .pubkeys
            .insert(KEY_ID.into(), third_party_key().verifying_key());
        assert_eq!(
            pins.verdict(&colliding, "demo.builtin", "1.0.0", KEY_ID, &pack.integrity),
            PinVerdict::Mismatch
        );
        // No embedded key for the pack's key id: reserved, never pinned.
        let orphan = BuiltinPins::from_set(&set(&[], &packs));
        assert_eq!(
            orphan.verdict(&policy, "demo.builtin", "1.0.0", KEY_ID, &pack.integrity),
            PinVerdict::Mismatch
        );
        // The empty set reserves nothing.
        let empty = BuiltinPins::from_set(&BuiltinSet::EMPTY);
        assert!(empty.is_empty() && !empty.is_reserved("demo.builtin"));
    }

    #[test]
    fn a_set_naming_one_pack_twice_with_different_metadata_pins_neither() {
        let v1 = make_pack("demo.builtin", "1.0.0");
        let v2 = make_pack("demo.builtin", "1.1.0");
        let packs = [v1.borrow(), v2.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let pins = BuiltinPins::from_set(&s);
        let (policy, _) = merge_builtin_policy(host_policy(), &s);
        assert!(pins.is_reserved("demo.builtin"));
        for p in [&v1, &v2] {
            assert_eq!(
                pins.verdict(&policy, &p.name, &p.version, KEY_ID, &p.integrity),
                PinVerdict::Mismatch
            );
        }
        // ...so ensure installs nothing for it.
        let env = Env::new();
        let (ctx, store, _) = env.start(host_policy(), &s);
        let report = ctx.ensure_builtin_packs(&s);
        assert!(
            report
                .outcomes
                .iter()
                .all(|(_, o)| *o == BuiltinOutcome::Skipped(BuiltinSkip::NotPinned))
        );
        assert!(store.is_empty() && names(&env.parent_path()).is_empty());
    }

    #[test]
    fn a_third_party_package_under_a_builtin_name_is_not_restored_and_is_replaced_by_the_pin() {
        let env = Env::new();
        let builtin = make_pack("demo.builtin", "1.0.0");
        let packs = [builtin.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        // A newer, validly signed package of the same name from another trusted signer lands on
        // disk (e.g. installed before the name became a built-in).
        let squatter = make_pack_signed("demo.builtin", "9.0.0", &third_party_key(), "third");
        {
            let (policy, _) = merge_builtin_policy(host_policy_with_third_party(), &BuiltinSet::EMPTY);
            let store = Arc::new(SignedRootBindingStore::new());
            let parent = HostExtensionsParent::new(open_host_dir(&env.parent_path())).unwrap();
            let ctx = HostSignedInstall::new(parent, policy, store).unwrap();
            let entry = hub_entry(&squatter);
            ctx.install_package(&entry, &squatter.zip)
                .expect("no built-ins pinned yet");
        }
        // Same Host trust, now with the built-in pinned: the squatter is not re-bound...
        let (ctx, store, report) = env.start(host_policy_with_third_party(), &s);
        assert_eq!(
            report.rejected,
            vec![("demo.builtin".to_owned(), RestoreRejection::BuiltinPinMismatch)]
        );
        assert!(store.is_empty());
        // ...and the built-in provisioner puts the pinned copy back (binary authority).
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Installed(InstallKind::Downgraded)
        );
        let bound = store.lookup_checked("demo.builtin").unwrap();
        assert_eq!((bound.version(), bound.key_id()), ("1.0.0", KEY_ID));
        assert_eq!(sri_sha512(bound.raw_bytes()), builtin.integrity);
        // Next start restores the pin, untouched.
        drop((ctx, store));
        let (_ctx, store, report) = env.start(host_policy_with_third_party(), &s);
        assert_eq!((report.restored, report.rejected.len()), (1, 0));
        assert_eq!(store.lookup_checked("demo.builtin").unwrap().key_id(), KEY_ID);
    }

    fn hub_entry(pack: &Owned) -> crate::hub::index_manager::HubIndexEntry {
        crate::hub::index_manager::HubIndexEntry {
            name: pack.name.clone(),
            version: pack.version.clone(),
            display_name: None,
            description: None,
            author: None,
            icon: None,
            tags: vec![],
            bundled: false,
            download_url: None,
            curated: true,
            integrity: Some(pack.integrity.clone()),
            signature: Some(pack.signature.clone()),
            pubkey_id: Some(pack.key_id.clone()),
        }
    }

    #[test]
    fn hub_install_of_a_builtin_name_is_refused_for_every_signer_and_flag() {
        use crate::hub::host_signed_install::SignedPackageError;
        let env = Env::new();
        let builtin = make_pack("demo.builtin", "1.0.0");
        let packs = [builtin.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let (ctx, store, _) = env.start(host_policy_with_third_party(), &s);
        assert!(ctx.is_builtin_name("demo.builtin") && ctx.is_builtin_name("DEMO.builtin"));
        assert!(!ctx.is_builtin_name("demo.other"));

        // Third-party signer, newer version.
        let squatter = make_pack_signed("demo.builtin", "9.0.0", &third_party_key(), "third");
        assert_eq!(
            ctx.install_package(&hub_entry(&squatter), &squatter.zip),
            Err(SignedPackageError::ReservedByBuiltin)
        );
        // The built-in key itself, another version (the binary is the only source of built-ins).
        let curator_other = make_pack("demo.builtin", "1.0.1");
        assert_eq!(
            ctx.install_package(&hub_entry(&curator_other), &curator_other.zip),
            Err(SignedPackageError::ReservedByBuiltin)
        );
        // Even the exact pinned bytes, and with the on-disk `bundled` flag set.
        let mut exact = hub_entry(&builtin);
        exact.bundled = true;
        assert_eq!(
            ctx.install_package(&exact, &builtin.zip),
            Err(SignedPackageError::ReservedByBuiltin)
        );
        // A case variant of the name.
        let variant = make_pack_signed("Demo.Builtin", "1.0.0", &third_party_key(), "third");
        assert_eq!(
            ctx.install_package(&hub_entry(&variant), &variant.zip),
            Err(SignedPackageError::ReservedByBuiltin)
        );
        assert!(store.is_empty() && names(&env.parent_path()).is_empty());

        // An unrelated name from the same third party still installs.
        let other = make_pack_signed("demo.other", "1.0.0", &third_party_key(), "third");
        assert!(ctx.install_package(&hub_entry(&other), &other.zip).is_ok());
        assert!(store.contains("demo.other"));
    }

    #[test]
    fn ensure_skips_a_pack_the_host_was_not_pinned_with() {
        let env = Env::new();
        let v1 = make_pack("demo.builtin", "1.0.0");
        let v2 = make_pack("demo.builtin", "1.1.0");
        let keys = [key_entry()];
        let (p1, p2) = ([v1.borrow()], [v2.borrow()]);
        let (s1, s2) = (set(&keys, &p1), set(&keys, &p2));
        let (ctx, store, _) = env.start(host_policy(), &s1);
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s2)),
            BuiltinOutcome::Skipped(BuiltinSkip::NotPinned)
        );
        assert!(store.is_empty() && names(&env.parent_path()).is_empty());
    }

    #[test]
    fn a_colliding_host_key_keeps_the_builtin_name_reserved() {
        let env = Env::new();
        let builtin = make_pack("demo.builtin", "1.0.0");
        let packs = [builtin.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        // The Host uses the built-in key id for another key, and something signed with that Host
        // key under the built-in name is on disk.
        let mut host = host_policy();
        host.pubkeys.insert(KEY_ID.into(), third_party_key().verifying_key());
        let squatter = make_pack_signed("demo.builtin", "1.0.0", &third_party_key(), KEY_ID);
        {
            let store = Arc::new(SignedRootBindingStore::new());
            let parent = HostExtensionsParent::new(open_host_dir(&env.parent_path())).unwrap();
            let ctx = HostSignedInstall::new(parent, host.clone(), store).unwrap();
            ctx.install_package(&hub_entry(&squatter), &squatter.zip).unwrap();
        }
        let (ctx, store, report) = env.start(host, &s);
        assert_eq!(
            report.rejected,
            vec![("demo.builtin".to_owned(), RestoreRejection::BuiltinPinMismatch)]
        );
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Skipped(BuiltinSkip::KeyUnavailable)
        );
        assert!(store.is_empty());
    }

    #[test]
    fn a_tampered_install_is_unbound_at_restore_and_healed_by_ensure() {
        let env = Env::new();
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        {
            let (ctx, _store, _) = env.start(host_policy(), &s);
            ctx.ensure_builtin_packs(&s);
        }
        let file = env
            .parent_path()
            .join("demo.builtin")
            .join("1.0.0")
            .join("panels")
            .join("board.html");
        std::fs::write(&file, b"EVIL").unwrap();
        let (ctx, store, report) = env.start(host_policy(), &s);
        assert_eq!(report.restored, 0);
        assert!(!store.contains("demo.builtin"));
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Installed(InstallKind::Reinstalled)
        );
        assert_eq!(
            std::fs::read(&file).unwrap(),
            format!("<p>{} {}</p>", pack.name, pack.version).into_bytes()
        );
        drop((ctx, store));
        let (_ctx, store, report) = env.start(host_policy(), &s);
        assert_eq!(report.restored, 1);
        assert!(store.contains("demo.builtin"));
    }

    #[test]
    fn builtins_survive_a_restrictive_allowlist_at_install_and_restore() {
        let env = Env::new();
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let mut host = host_policy();
        host.allowlist.insert("something.else".into());
        {
            let (ctx, _store, _) = env.start(host.clone(), &s);
            assert_eq!(
                only(&ctx.ensure_builtin_packs(&s)),
                BuiltinOutcome::Installed(InstallKind::Fresh)
            );
        }
        let (_ctx, store, report) = env.start(host, &s);
        assert_eq!(report.restored, 1);
        assert!(store.contains("demo.builtin"));
    }

    #[test]
    fn a_key_the_host_already_uses_for_a_different_key_skips_the_pack_without_touching_disk() {
        let env = Env::new();
        let pack = make_pack("demo.builtin", "1.0.0");
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let mut host = host_policy();
        host.pubkeys
            .insert(KEY_ID.into(), SigningKey::from_bytes(&[42; 32]).verifying_key());
        let (ctx, store, _) = env.start(host, &s);
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Skipped(BuiltinSkip::KeyUnavailable)
        );
        assert!(store.is_empty());
        assert!(names(&env.parent_path()).is_empty());
    }

    #[test]
    fn a_pack_referencing_an_unknown_key_id_is_skipped() {
        let env = Env::new();
        let mut pack = make_pack("demo.builtin", "1.0.0");
        pack.key_id = "not-shipped".into();
        let packs = [pack.borrow()];
        let keys = [key_entry()];
        let s = set(&keys, &packs);
        let (ctx, store, _) = env.start(host_policy(), &s);
        assert_eq!(
            only(&ctx.ensure_builtin_packs(&s)),
            BuiltinOutcome::Skipped(BuiltinSkip::KeyUnavailable)
        );
        assert!(store.is_empty() && names(&env.parent_path()).is_empty());
    }

    #[test]
    fn bad_signature_or_tampered_zip_is_refused_with_no_filesystem_effect() {
        let env = Env::new();
        let keys = [key_entry()];
        let mut bad_sig = make_pack("demo.builtin", "1.0.0");
        bad_sig.signature = hex::encode([7u8; 64]);
        let mut wrong_integrity = make_pack("demo.other", "1.0.0");
        wrong_integrity.integrity = sri_sha512(b"something else");
        let garbage = Owned {
            zip: b"not a zip".to_vec(),
            ..make_pack("demo.garbage", "1.0.0")
        };
        let packs = [bad_sig.borrow(), wrong_integrity.borrow(), garbage.borrow()];
        let s = set(&keys, &packs);
        let (ctx, store, _) = env.start(host_policy(), &s);
        let report = ctx.ensure_builtin_packs(&s);
        assert!(matches!(
            report.outcomes[0].1,
            BuiltinOutcome::Failed(LocalPackageError::Signature(SignedManifestError::InvalidSignature))
        ));
        assert!(matches!(
            report.outcomes[1].1,
            BuiltinOutcome::Failed(LocalPackageError::Signature(SignedManifestError::IntegrityMismatch))
        ));
        assert_eq!(
            report.outcomes[2].1,
            BuiltinOutcome::Failed(LocalPackageError::InvalidPackage)
        );
        assert!(store.is_empty());
        assert!(names(&env.parent_path()).is_empty(), "{:?}", names(&env.parent_path()));
    }

    #[test]
    fn one_failing_pack_does_not_stop_the_others() {
        let env = Env::new();
        let keys = [key_entry()];
        let mut bad = make_pack("demo.bad", "1.0.0");
        bad.signature = hex::encode([7u8; 64]);
        let good = make_pack("demo.good", "1.0.0");
        let packs = [bad.borrow(), good.borrow()];
        let s = set(&keys, &packs);
        let (ctx, store, _) = env.start(host_policy(), &s);
        let report = ctx.ensure_builtin_packs(&s);
        assert!(matches!(report.outcomes[0].1, BuiltinOutcome::Failed(_)));
        assert_eq!(report.outcomes[1].1, BuiltinOutcome::Installed(InstallKind::Fresh));
        assert_eq!(report.installed(), 1);
        assert!(store.contains("demo.good") && !store.contains("demo.bad"));
    }

    #[test]
    fn empty_set_installs_nothing() {
        let env = Env::new();
        // A Host with its own key configured, no built-ins.
        let mut host = host_policy();
        host.pubkeys.insert(KEY_ID.into(), signing_key().verifying_key());
        let (ctx, store, _) = env.start(host, &BuiltinSet::EMPTY);
        assert!(ctx.ensure_builtin_packs(&BuiltinSet::EMPTY).outcomes.is_empty());
        assert!(store.is_empty() && names(&env.parent_path()).is_empty());
    }
}
