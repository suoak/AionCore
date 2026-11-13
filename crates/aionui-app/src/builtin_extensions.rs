//! The app's built-in (shipped) signed extension packs (M9 E13 slices B1 / B2 / B5).
//!
//! The install / upgrade / self-heal machinery lives in `aionui_extension::hub::builtin_packs`; this
//! module only names **what the app ships**: the CSBU WorkMate curator **public** key and the three
//! curated packs, each with the detached signature the curator produced off-repo over
//! `signature_payload(name, version, integrity)`.
//!
//! * Only public material is here. The curator private key never enters this repository.
//! * The zips are the committed `builtin/*.zip` files (the same bytes `workmate-builtin-packs
//!   --check` rebuilds and compares in CI). `builtin/index.json` keeps the throwaway
//!   `workmate-test-dev` signatures for that CI check; the production signatures live only here.
//! * Shipping a new pack means adding its zip (`include_bytes!`) and its curator-signed metadata
//!   here; the tests below refuse anything that does not verify under the embedded key.

use aionui_extension::hub::{BuiltinKey, BuiltinPack, BuiltinSet};

/// Id the production packs reference the curator key by.
pub(crate) const CURATOR_KEY_ID: &str = "csbu-workmate-curator-2026";

/// CSBU WorkMate curator Ed25519 public key
/// (Base64 `pl8V3gUDf8/TSKkJ8Yqyim3W5vuLtIqLpq9TUUU+20o=`,
/// SHA-256 `270afadaef77b76b723d64cd23b29f628aa4251b0a1c476d061f6a3b92d37d53`).
const CURATOR_PUBLIC_KEY: [u8; 32] = [
    166, 95, 21, 222, 5, 3, 127, 207, 211, 72, 169, 9, 241, 138, 178, 138, 109, 214, 230, 251, 139, 180, 138, 139, 166,
    175, 83, 81, 69, 62, 219, 74,
];

static KEYS: [BuiltinKey<'static>; 1] = [BuiltinKey {
    id: CURATOR_KEY_ID,
    public_key: CURATOR_PUBLIC_KEY,
}];

static PACKS: [BuiltinPack<'static>; 3] = [
    BuiltinPack {
        name: "workmate.theme-dusk-ember",
        version: "1.0.0",
        pubkey_id: CURATOR_KEY_ID,
        integrity: "sha512-DOyvouLAJB3J/PD9fSlu7/0eGVfz45vKL4XcETWxYhzV9L0DWtyJv62FQXRTFUJZQ0qqBZyL1zJHeySbkzO5ow==",
        signature: "bd71e55740ca164a89bf95fd122d01f9cdd617582e924807205e47b33e2987c6\
                    eaf99750348d13f70454f1b3155fe3fc6b83d981797b48283b9f6929d175f206",
        package_zip: include_bytes!("../../../builtin/workmate.theme-dusk-ember.zip"),
    },
    BuiltinPack {
        name: "workmate.sample-shortcuts",
        version: "1.0.0",
        pubkey_id: CURATOR_KEY_ID,
        integrity: "sha512-TOaM4dfAmKrZ5O5VmhgzYw22w6YZt2C509etgf22W2bRmEYC2c2nfvhgGFrwojxLHgif5+fkhyaik7/hy19p/A==",
        signature: "664fe60d2d5032872bda22951c4b756410a77e8861cb58d78f431f968535d52a\
                    2a0e23b8b14550944b5de1939d1b0602d2ff7dbdee63515a61e5745c2bd3a80c",
        package_zip: include_bytes!("../../../builtin/workmate.sample-shortcuts.zip"),
    },
    BuiltinPack {
        name: "workmate.sample-checklist",
        version: "1.0.0",
        pubkey_id: CURATOR_KEY_ID,
        integrity: "sha512-+X0o6KB7eTf7wol0I4SN3q6C+CMTzz0knRGEkMYpPOyXNju915+YlhaEcK9ko92YBcy4o+Gt/RlP8mSuyshhQQ==",
        signature: "85f91b749606e4c19d8ece3430b7ea1b41f5b769b1c43e59eb9edf74eb24f425\
                    54fb69efcd84f040b2360b7c1172fde7cacaddbf795d14062aa589a8c1fa8207",
        package_zip: include_bytes!("../../../builtin/workmate.sample-checklist.zip"),
    },
];

static SET: BuiltinSet<'static> = BuiltinSet {
    keys: &KEYS,
    packs: &PACKS,
};

/// The production set: the curator key and the three curated packs.
pub(crate) fn builtins() -> &'static BuiltinSet<'static> {
    &SET
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_extension::hub::curated::{signature_payload, sri_sha512};
    use ed25519_dalek::{Signature, VerifyingKey};
    use std::io::Read;

    const EXPECTED: [&str; 3] = [
        "workmate.theme-dusk-ember",
        "workmate.sample-shortcuts",
        "workmate.sample-checklist",
    ];

    fn manifest_bytes(zip: &[u8]) -> Vec<u8> {
        let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        let mut entry = archive.by_name("aion-extension.json").unwrap();
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).unwrap();
        bytes
    }

    #[test]
    fn production_set_is_one_curator_key_and_the_three_curated_packs() {
        let set = builtins();
        assert!(!set.is_empty());
        assert_eq!(set.keys.len(), 1);
        assert_eq!(set.keys[0].id, "csbu-workmate-curator-2026");
        assert_eq!(set.packs.iter().map(|p| p.name).collect::<Vec<_>>(), EXPECTED);
        for pack in set.packs {
            assert_eq!(pack.version, "1.0.0", "{}", pack.name);
            assert_eq!(pack.pubkey_id, CURATOR_KEY_ID, "{}", pack.name);
        }
    }

    #[test]
    fn no_test_dev_key_or_signature_ships_in_production() {
        let set = builtins();
        assert!(set.keys.iter().all(|k| !k.id.contains("test")));
        assert!(set.packs.iter().all(|p| p.pubkey_id != "workmate-test-dev"));

        // The CI-only index carries the throwaway key; none of its material may appear here.
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(repo.join("builtin/index.json")).unwrap()).unwrap();
        assert_eq!(index["pubkeyId"], "workmate-test-dev");
        let test_key = hex::decode(index["publicKeyHex"].as_str().unwrap()).unwrap();
        assert_ne!(test_key.as_slice(), CURATOR_PUBLIC_KEY.as_slice());
        for entry in index["packs"].as_array().unwrap() {
            let pack = set.packs.iter().find(|p| p.name == entry["name"]).unwrap();
            assert_ne!(pack.signature, entry["signature"].as_str().unwrap(), "{}", pack.name);
            // Same committed zip, so the same manifest integrity under both keys.
            assert_eq!(pack.integrity, entry["integrity"].as_str().unwrap(), "{}", pack.name);
        }
    }

    #[test]
    fn curator_public_key_matches_its_published_fingerprint() {
        use base64::Engine as _;
        use sha2::Digest as _;
        assert_eq!(
            base64::engine::general_purpose::STANDARD.encode(CURATOR_PUBLIC_KEY),
            "pl8V3gUDf8/TSKkJ8Yqyim3W5vuLtIqLpq9TUUU+20o="
        );
        assert_eq!(
            hex::encode(sha2::Sha256::digest(CURATOR_PUBLIC_KEY)),
            "270afadaef77b76b723d64cd23b29f628aa4251b0a1c476d061f6a3b92d37d53"
        );
        assert!(VerifyingKey::from_bytes(&CURATOR_PUBLIC_KEY).is_ok());
    }

    #[test]
    fn every_pack_signature_verifies_strictly_under_the_embedded_key() {
        let set = builtins();
        for pack in set.packs {
            let key = set.keys.iter().find(|k| k.id == pack.pubkey_id).unwrap();
            let key = VerifyingKey::from_bytes(&key.public_key).unwrap();
            let raw: [u8; 64] = hex::decode(pack.signature).unwrap().try_into().unwrap();
            let payload = signature_payload(pack.name, pack.version, pack.integrity);
            key.verify_strict(payload.as_bytes(), &Signature::from_bytes(&raw))
                .unwrap_or_else(|_| panic!("{}: signature does not verify", pack.name));

            // Any change to the signed fields breaks it.
            let other = signature_payload(pack.name, "1.0.1", pack.integrity);
            assert!(
                key.verify_strict(other.as_bytes(), &Signature::from_bytes(&raw))
                    .is_err()
            );
        }
    }

    #[test]
    fn every_pack_integrity_is_the_sri_of_the_manifest_inside_its_zip() {
        for pack in builtins().packs {
            let manifest = manifest_bytes(pack.package_zip);
            assert_eq!(sri_sha512(&manifest), pack.integrity, "{}", pack.name);
            let parsed: serde_json::Value = serde_json::from_slice(&manifest).unwrap();
            assert_eq!(parsed["name"], pack.name);
            assert_eq!(parsed["version"], pack.version);
        }
    }

    #[test]
    fn embedded_zips_are_the_committed_builtin_zips() {
        let repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        for pack in builtins().packs {
            let committed = std::fs::read(repo.join("builtin").join(format!("{}.zip", pack.name))).unwrap();
            assert_eq!(pack.package_zip, committed.as_slice(), "{}", pack.name);
        }
    }
}
