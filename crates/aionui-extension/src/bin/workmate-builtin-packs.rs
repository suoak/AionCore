//! Pack and test-sign the curated built-in extension sources (M9 E13 slice B2).
//!
//! ```text
//! workmate-builtin-packs [--repo-root <dir>] [--check]
//! ```
//!
//! Always uses the throwaway test/dev curator key in `tools/builtin-sign/workmate-test-dev.seed`
//! (id `workmate-test-dev`). This tool will not read, embed, or write a production key.
//! Without `--check`, it updates each source manifest's `files` list, writes `builtin/*.zip`
//! and `builtin/index.json`. `--check` writes nothing and exits 3 when any committed artefact
//! differs from a fresh pack+sign.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aionui_extension::hub::curated::signature_payload;
use aionui_extension::pack::{PackOptions, pack_extension_dir};
use aionui_extension::theme_import::{convert_vendored, css_matches, default_paths, verify_upstream_blobs};
use ed25519_dalek::{Signer, SigningKey, VerifyingKey};
use serde_json::{Value, json};

const TEST_KEY_ID: &str = "workmate-test-dev";
const TEST_KEY_REL: &str = "tools/builtin-sign/workmate-test-dev.seed";
const INDEX_REL: &str = "builtin/index.json";

struct PackDef {
    name: &'static str,
    version: &'static str,
    source_rel: &'static str,
    zip_rel: &'static str,
}

const PACKS: &[PackDef] = &[PackDef {
    name: "workmate.theme-dusk-ember",
    version: "1.0.0",
    source_rel: "extensions/workmate.theme-dusk-ember",
    zip_rel: "builtin/workmate.theme-dusk-ember.zip",
}];

struct Args {
    repo_root: PathBuf,
    check: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut repo_root = None;
    let mut check = false;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--repo-root" => {
                repo_root = Some(PathBuf::from(it.next().ok_or("--repo-root needs a value")?));
            }
            "--check" => check = true,
            "--help" | "-h" => {
                println!(
                    "workmate-builtin-packs — pack and test-sign curated built-ins\n\n  --repo-root <dir>  repository root (default: cwd)\n  --check            write nothing; exit 3 if committed artefacts are stale\n\nUses only the throwaway test/dev key (tools/builtin-sign/). No production key."
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown option {other}")),
        }
    }
    Ok(Args {
        repo_root: repo_root.unwrap_or_else(|| PathBuf::from(".")),
        check,
    })
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))
}

fn load_test_key(repo_root: &Path) -> Result<SigningKey, String> {
    let path = repo_root.join(TEST_KEY_REL);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let hex: String = text.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 64 {
        return Err(format!(
            "{}: expected 32-byte hex seed, got {} hex chars",
            path.display(),
            hex.len()
        ));
    }
    let mut seed = [0u8; 32];
    hex::decode_to_slice(&hex, &mut seed).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

fn sign_pack(key: &SigningKey, name: &str, version: &str, integrity: &str) -> String {
    hex::encode(
        key.sign(signature_payload(name, version, integrity).as_bytes())
            .to_bytes(),
    )
}

fn public_key_hex(key: &VerifyingKey) -> String {
    hex::encode(key.to_bytes())
}

fn pretty(value: &Value) -> Vec<u8> {
    let mut bytes = serde_json::to_vec_pretty(value).expect("index is valid JSON");
    if !bytes.ends_with(b"\n") {
        bytes.push(b'\n');
    }
    bytes
}

fn check_theme_css(repo_root: &Path) -> Result<(), (u8, String)> {
    let paths = default_paths(repo_root);
    let license = read(&paths.license).map_err(|e| (1, e))?;
    let dusk = read(&paths.dusk).map_err(|e| (1, e))?;
    let ember = read(&paths.ember).map_err(|e| (1, e))?;
    verify_upstream_blobs(&license, &dusk, &ember).map_err(|e| (2, format!("upstream pin mismatch: {e}")))?;
    let committed = read(&paths.css).map_err(|e| (3, e))?;
    css_matches(&committed, &dusk, &ember).map_err(|e| (3, e))?;
    Ok(())
}

fn write_theme_css(repo_root: &Path) -> Result<(), String> {
    let paths = default_paths(repo_root);
    let license = read(&paths.license)?;
    let dusk = read(&paths.dusk)?;
    let ember = read(&paths.ember)?;
    verify_upstream_blobs(&license, &dusk, &ember)?;
    let css = convert_vendored(&dusk, &ember)?;
    if let Some(parent) = paths.css.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    std::fs::write(&paths.css, css.as_bytes()).map_err(|e| format!("{}: {e}", paths.css.display()))?;
    Ok(())
}

struct Built {
    name: String,
    version: String,
    integrity: String,
    signature: String,
    zip_rel: String,
    zip: Vec<u8>,
    manifest: Vec<u8>,
    source_manifest: PathBuf,
}

fn build_all(repo_root: &Path, key: &SigningKey) -> Result<Vec<Built>, String> {
    let mut out = Vec::new();
    for def in PACKS {
        let source = repo_root.join(def.source_rel);
        let packed =
            pack_extension_dir(&source, &PackOptions::default()).map_err(|e| format!("{}: {e}", source.display()))?;
        if packed.name != def.name || packed.version != def.version {
            return Err(format!(
                "{}: packed identity {}@{} != {}@{}",
                source.display(),
                packed.name,
                packed.version,
                def.name,
                def.version
            ));
        }
        if packed.diff.added != 0 || packed.diff.removed != 0 || packed.diff.updated != 0 {
            // The zip always carries the computed files list; a stale source manifest is still a
            // check failure (write mode rewrites it).
        }
        let signature = sign_pack(key, def.name, def.version, &packed.manifest_integrity);
        out.push(Built {
            name: def.name.to_owned(),
            version: def.version.to_owned(),
            integrity: packed.manifest_integrity,
            signature,
            zip_rel: def.zip_rel.to_owned(),
            zip: packed.zip,
            manifest: packed.manifest,
            source_manifest: source.join("aion-extension.json"),
        });
    }
    Ok(out)
}

fn index_bytes(key: &VerifyingKey, built: &[Built]) -> Vec<u8> {
    let packs: Vec<Value> = built
        .iter()
        .map(|b| {
            json!({
                "name": b.name,
                "version": b.version,
                "pubkeyId": TEST_KEY_ID,
                "integrity": b.integrity,
                "signature": b.signature,
                "zip": Path::new(&b.zip_rel).file_name().and_then(|n| n.to_str()).unwrap(),
            })
        })
        .collect();
    pretty(&json!({
        "warning": "Test/dev signatures only. The production curator key is not in this repository and is not used by this pipeline.",
        "pubkeyId": TEST_KEY_ID,
        "publicKeyHex": public_key_hex(key),
        "packs": packs,
    }))
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("workmate-builtin-packs: {e}");
            return ExitCode::from(1);
        }
    };
    let key = match load_test_key(&args.repo_root) {
        Ok(k) => k,
        Err(e) => {
            eprintln!("workmate-builtin-packs: {e}");
            return ExitCode::from(1);
        }
    };

    if args.check {
        if let Err((code, e)) = check_theme_css(&args.repo_root) {
            eprintln!("workmate-builtin-packs: {e}");
            return ExitCode::from(code);
        }
        let built = match build_all(&args.repo_root, &key) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("workmate-builtin-packs: {e}");
                return ExitCode::from(1);
            }
        };
        let index = index_bytes(&key.verifying_key(), &built);
        let committed_index = match read(&args.repo_root.join(INDEX_REL)) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("workmate-builtin-packs: {e}");
                return ExitCode::from(3);
            }
        };
        if committed_index != index {
            eprintln!("workmate-builtin-packs: {INDEX_REL} is out of date");
            return ExitCode::from(3);
        }
        for b in &built {
            let committed_zip = match read(&args.repo_root.join(&b.zip_rel)) {
                Ok(z) => z,
                Err(e) => {
                    eprintln!("workmate-builtin-packs: {e}");
                    return ExitCode::from(3);
                }
            };
            if committed_zip != b.zip {
                eprintln!("workmate-builtin-packs: {} is out of date", b.zip_rel);
                return ExitCode::from(3);
            }
            let source_manifest = match read(&b.source_manifest) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!("workmate-builtin-packs: {e}");
                    return ExitCode::from(3);
                }
            };
            if source_manifest != b.manifest {
                eprintln!(
                    "workmate-builtin-packs: {} files list is out of date",
                    b.source_manifest.display()
                );
                return ExitCode::from(3);
            }
        }
        return ExitCode::SUCCESS;
    }

    if let Err(e) = write_theme_css(&args.repo_root) {
        eprintln!("workmate-builtin-packs: {e}");
        return ExitCode::from(1);
    }
    let built = match build_all(&args.repo_root, &key) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("workmate-builtin-packs: {e}");
            return ExitCode::from(1);
        }
    };
    for b in &built {
        if let Err(e) = std::fs::write(&b.source_manifest, &b.manifest) {
            eprintln!("workmate-builtin-packs: {}: {e}", b.source_manifest.display());
            return ExitCode::from(1);
        }
        let zip_path = args.repo_root.join(&b.zip_rel);
        if let Some(parent) = zip_path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("workmate-builtin-packs: {}: {e}", parent.display());
            return ExitCode::from(1);
        }
        if let Err(e) = std::fs::write(&zip_path, &b.zip) {
            eprintln!("workmate-builtin-packs: {}: {e}", zip_path.display());
            return ExitCode::from(1);
        }
        println!("wrote {}", b.zip_rel);
    }
    let index_path = args.repo_root.join(INDEX_REL);
    if let Err(e) = std::fs::write(&index_path, index_bytes(&key.verifying_key(), &built)) {
        eprintln!("workmate-builtin-packs: {}: {e}", index_path.display());
        return ExitCode::from(1);
    }
    println!("wrote {INDEX_REL} (key id {TEST_KEY_ID}, test/dev only)");
    ExitCode::SUCCESS
}
