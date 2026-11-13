//! Orca Dusk & Ember → WorkMate CSS converter (M9 E13 slice B2 / B5 theme).
//!
//! A **build helper**, not a runtime API. It reads the vendored Orca app-theme JSON
//! (`tools/theme-import/upstream/{dusk,ember}.json` at commit `4589eb93e28d8a4e4fdd3a77670297da804e7abe`)
//! and emits one WorkMate stylesheet: `:root` is Ember (light), `[data-theme='dark']` is Dusk
//! (dark). Terminal themes and `tokenColors` are ignored. Colour values come from the 24 Orca
//! tokens; a few WorkMate-only slots (success / warning / danger / text-white) are documented
//! defaults, not upstream.
//!
//! Mapping (Orca token → WorkMate variables):
//! * `--background` → `--color-bg-1`, `--bg-1`
//! * `--card` → `--color-bg-2`, `--bg-2`, `--bg-base`, `--fill-0`, `--dialog-fill-0`, `--inverse`
//! * `--editor-surface` → `--fill`, `--color-fill`
//! * `--muted` / `--secondary` → `--color-bg-3`, `--bg-3`, `--bg-hover`
//! * `--accent` → `--color-bg-4`, `--bg-4`, `--bg-active`
//! * `--foreground` → `--color-text-1`, `--text-primary`, `--text-0`
//! * `--muted-foreground` → `--color-text-2`, `--text-secondary`, `--color-text-3`
//! * `--border` → `--color-border`, `--color-border-1`, `--border-base`
//! * `--primary` → `--color-primary`, `--primary`, `--brand`, `--color-brand-fill`, `--info`, `--bg-8`
//! * `--primary-foreground` → `--primary-foreground`
//! * `--sidebar*` → `--sider-bg` / `--sider-fg` / `--sider-border` / `--sider-accent` / `--sider-accent-fg`
//! * `--primary-rgb` and `--color-primary-light-*` / `--color-primary-dark-1` / `--aou-1`–`--aou-10`
//!   / `--brand-light` / `--brand-hover` / `--bg-5`–`--bg-10` are **derived** (tint / shade), not
//!   extra upstream colours.

use crate::hub::curated::sri_sha512;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

/// Pinned upstream commit (stablyai/orca-plugins `plugins/dusk-ember`).
pub const UPSTREAM_COMMIT: &str = "4589eb93e28d8a4e4fdd3a77670297da804e7abe";

/// Git blob SHAs recorded in the item-1 design (and `tools/theme-import/upstream/SHAS`).
pub const BLOB_SHA_LICENSE: &str = "fbf46fc17dacb1f65e03e2b6290eb53ba94b149d";
pub const BLOB_SHA_DUSK: &str = "99b70bf0ec0313b27ac6e5be5c7233b45dcbd9ae";
pub const BLOB_SHA_EMBER: &str = "099b9cd51a00d43f0dac8828af69207bd8015739";

/// The 24 Orca app-theme token names we read. Anything else in `tokens` is ignored.
pub const ORCA_TOKEN_NAMES: &[&str] = &[
    "--background",
    "--editor-surface",
    "--foreground",
    "--card",
    "--card-foreground",
    "--popover",
    "--popover-foreground",
    "--primary",
    "--primary-foreground",
    "--secondary",
    "--secondary-foreground",
    "--muted",
    "--muted-foreground",
    "--accent",
    "--accent-foreground",
    "--border",
    "--input",
    "--ring",
    "--sidebar",
    "--sidebar-foreground",
    "--sidebar-accent",
    "--sidebar-accent-foreground",
    "--sidebar-border",
    "--sidebar-ring",
];

/// Git blob SHA-1 (`sha1("blob {len}\\0{bytes}")`) of a file's exact bytes.
pub fn git_blob_sha1(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    let mut h = Sha1::new();
    h.update(format!("blob {}\0", bytes.len()).as_bytes());
    h.update(bytes);
    hex::encode(h.finalize())
}

/// Confirm the vendored upstream files match the pinned blob SHAs.
pub fn verify_upstream_blobs(license: &[u8], dusk: &[u8], ember: &[u8]) -> Result<(), String> {
    let got_license = git_blob_sha1(license);
    let got_dusk = git_blob_sha1(dusk);
    let got_ember = git_blob_sha1(ember);
    if got_license != BLOB_SHA_LICENSE {
        return Err(format!("LICENSE blob SHA {got_license} != {BLOB_SHA_LICENSE}"));
    }
    if got_dusk != BLOB_SHA_DUSK {
        return Err(format!("dusk.json blob SHA {got_dusk} != {BLOB_SHA_DUSK}"));
    }
    if got_ember != BLOB_SHA_EMBER {
        return Err(format!("ember.json blob SHA {got_ember} != {BLOB_SHA_EMBER}"));
    }
    Ok(())
}

/// Paths under a repo root for the vendored inputs and the generated CSS.
pub fn default_paths(repo_root: &Path) -> ThemeImportPaths {
    ThemeImportPaths {
        license: repo_root.join("tools/theme-import/upstream/LICENSE"),
        dusk: repo_root.join("tools/theme-import/upstream/dusk.json"),
        ember: repo_root.join("tools/theme-import/upstream/ember.json"),
        css: repo_root.join("extensions/workmate.theme-dusk-ember/themes/dusk-ember.css"),
    }
}

#[derive(Debug, Clone)]
pub struct ThemeImportPaths {
    pub license: std::path::PathBuf,
    pub dusk: std::path::PathBuf,
    pub ember: std::path::PathBuf,
    pub css: std::path::PathBuf,
}

/// Convert the two vendored JSON files into the WorkMate stylesheet.
pub fn convert_vendored(dusk_json: &[u8], ember_json: &[u8]) -> Result<String, String> {
    let dusk = parse_app_theme(dusk_json, "dusk")?;
    let ember = parse_app_theme(ember_json, "ember")?;
    if dusk.base != "dark" {
        return Err(format!("dusk.json base is {:?}, expected dark", dusk.base));
    }
    if ember.base != "light" {
        return Err(format!("ember.json base is {:?}, expected light", ember.base));
    }
    Ok(render_stylesheet(&ember.tokens, &dusk.tokens))
}

/// `--check`: regenerate and compare to the committed CSS (LF, exact bytes).
pub fn css_matches(committed: &[u8], dusk_json: &[u8], ember_json: &[u8]) -> Result<(), String> {
    let generated = convert_vendored(dusk_json, ember_json)?;
    if committed != generated.as_bytes() {
        return Err("generated CSS does not match the committed themes/dusk-ember.css".into());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct AppThemeFile {
    base: String,
    tokens: BTreeMap<String, String>,
    /// Present in upstream; never imported.
    #[serde(default, rename = "tokenColors")]
    token_colors: serde_json::Value,
}

struct AppTheme {
    base: String,
    tokens: BTreeMap<String, Rgb>,
}

fn parse_app_theme(bytes: &[u8], label: &str) -> Result<AppTheme, String> {
    let file: AppThemeFile = serde_json::from_slice(bytes).map_err(|e| format!("{label}.json: {e}"))?;
    let mut tokens = BTreeMap::new();
    for name in ORCA_TOKEN_NAMES {
        let raw = file
            .tokens
            .get(*name)
            .ok_or_else(|| format!("{label}.json: missing token {name}"))?;
        let rgb = Rgb::parse(raw).ok_or_else(|| format!("{label}.json: {name} is not #rrggbb ({raw})"))?;
        tokens.insert((*name).to_owned(), rgb);
    }
    // tokenColors must not leak into the output; we only assert it was present or absent harmlessly.
    let _ = file.token_colors;
    Ok(AppTheme {
        base: file.base,
        tokens,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

impl Rgb {
    fn parse(s: &str) -> Option<Self> {
        let hex = s.trim().strip_prefix('#')?;
        if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        let n = u32::from_str_radix(hex, 16).ok()?;
        Some(Self {
            r: (n >> 16) as u8,
            g: (n >> 8) as u8,
            b: n as u8,
        })
    }

    fn hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }

    fn css_rgb(self) -> String {
        format!("{}, {}, {}", self.r, self.g, self.b)
    }

    fn mix(self, other: Self, t: f64) -> Self {
        let m = |a: u8, b: u8| (a as f64 + (b as f64 - a as f64) * t).round().clamp(0.0, 255.0) as u8;
        Self {
            r: m(self.r, other.r),
            g: m(self.g, other.g),
            b: m(self.b, other.b),
        }
    }

    fn lighten(self, t: f64) -> Self {
        self.mix(Self { r: 255, g: 255, b: 255 }, t)
    }

    fn darken(self, t: f64) -> Self {
        self.mix(Self { r: 0, g: 0, b: 0 }, t)
    }

    fn luma(self) -> f64 {
        0.2126 * f64::from(self.r) + 0.7152 * f64::from(self.g) + 0.0722 * f64::from(self.b)
    }

    fn is_dark(self) -> bool {
        self.luma() < 128.0
    }
}

fn tok(tokens: &BTreeMap<String, Rgb>, name: &str) -> Rgb {
    tokens[name]
}

fn workmate_vars(tokens: &BTreeMap<String, Rgb>) -> Vec<(&'static str, String)> {
    let background = tok(tokens, "--background");
    let editor = tok(tokens, "--editor-surface");
    let foreground = tok(tokens, "--foreground");
    let card = tok(tokens, "--card");
    let primary = tok(tokens, "--primary");
    let primary_fg = tok(tokens, "--primary-foreground");
    let secondary = tok(tokens, "--secondary");
    let muted = tok(tokens, "--muted");
    let muted_fg = tok(tokens, "--muted-foreground");
    let accent = tok(tokens, "--accent");
    let border = tok(tokens, "--border");
    let sidebar = tok(tokens, "--sidebar");
    let sidebar_fg = tok(tokens, "--sidebar-foreground");
    let sidebar_accent = tok(tokens, "--sidebar-accent");
    let sidebar_accent_fg = tok(tokens, "--sidebar-accent-foreground");
    let sidebar_border = tok(tokens, "--sidebar-border");

    let toward_contrast = if background.is_dark() {
        Rgb { r: 255, g: 255, b: 255 }
    } else {
        Rgb { r: 0, g: 0, b: 0 }
    };
    let aou = [
        background.mix(primary, 0.08),
        background.mix(primary, 0.18),
        background.mix(primary, 0.32),
        background.mix(primary, 0.48),
        background.mix(primary, 0.64),
        background.mix(primary, 0.82),
        primary,
        primary.mix(toward_contrast, 0.18),
        primary.mix(toward_contrast, 0.36),
        primary.mix(toward_contrast, 0.52),
    ];
    // Documented WorkMate defaults — not from Orca. Light vs dark only so contrast holds.
    let (success, warning, danger, text_white) = if background.is_dark() {
        (
            Rgb::parse("#34d399").unwrap(),
            Rgb::parse("#fbbf24").unwrap(),
            Rgb::parse("#f87171").unwrap(),
            Rgb::parse("#ffffff").unwrap(),
        )
    } else {
        (
            Rgb::parse("#16a34a").unwrap(),
            Rgb::parse("#d97706").unwrap(),
            Rgb::parse("#dc2626").unwrap(),
            Rgb::parse("#ffffff").unwrap(),
        )
    };

    let v = |c: Rgb| c.hex();
    vec![
        ("--color-primary", v(primary)),
        ("--primary", v(primary)),
        ("--color-primary-light-1", v(primary.lighten(0.15))),
        ("--color-primary-light-2", v(primary.lighten(0.30))),
        ("--color-primary-light-3", v(primary.lighten(0.45))),
        ("--color-primary-dark-1", v(primary.darken(0.15))),
        ("--primary-rgb", primary.css_rgb()),
        ("--primary-foreground", v(primary_fg)),
        ("--brand", v(primary)),
        ("--brand-light", v(background.mix(primary, 0.12))),
        (
            "--brand-hover",
            v(if background.is_dark() {
                primary.lighten(0.12)
            } else {
                primary.darken(0.12)
            }),
        ),
        ("--color-brand-fill", v(primary)),
        ("--color-brand-bg", v(background.mix(primary, 0.12))),
        ("--aou-1", v(aou[0])),
        ("--aou-2", v(aou[1])),
        ("--aou-3", v(aou[2])),
        ("--aou-4", v(aou[3])),
        ("--aou-5", v(aou[4])),
        ("--aou-6", v(aou[5])),
        ("--aou-7", v(aou[6])),
        ("--aou-8", v(aou[7])),
        ("--aou-9", v(aou[8])),
        ("--aou-10", v(aou[9])),
        ("--color-bg-1", v(background)),
        ("--bg-1", v(background)),
        ("--color-bg-2", v(card)),
        ("--bg-2", v(card)),
        ("--color-bg-3", v(muted)),
        ("--bg-3", v(muted)),
        ("--color-bg-4", v(accent)),
        ("--bg-4", v(accent)),
        ("--bg-base", v(card)),
        ("--bg-5", v(accent.mix(primary, 0.25))),
        ("--bg-6", v(accent.mix(primary, 0.50))),
        ("--bg-8", v(primary)),
        ("--bg-9", v(sidebar.mix(toward_contrast, 0.15))),
        ("--bg-10", v(sidebar.mix(toward_contrast, 0.30))),
        ("--bg-hover", v(muted)),
        ("--bg-active", v(accent)),
        ("--fill", v(editor)),
        ("--color-fill", v(editor)),
        ("--fill-0", v(card)),
        ("--fill-white-to-black", v(card)),
        ("--dialog-fill-0", v(card)),
        ("--inverse", v(card)),
        ("--color-text-1", v(foreground)),
        ("--text-primary", v(foreground)),
        ("--color-text-2", v(muted_fg)),
        ("--text-secondary", v(muted_fg)),
        ("--color-text-3", v(muted_fg.mix(background, 0.25))),
        ("--text-disabled", v(border)),
        ("--text-0", v(foreground)),
        ("--text-white", v(text_white)),
        ("--color-border", v(border)),
        ("--color-border-1", v(border)),
        ("--color-border-2", v(border.mix(background, 0.45))),
        ("--border-base", v(border)),
        ("--border-light", v(border.mix(background, 0.45))),
        ("--border-special", "var(--bg-3)".into()),
        ("--success", v(success)),
        ("--warning", v(warning)),
        ("--danger", v(danger)),
        ("--info", v(primary)),
        ("--message-user-bg", v(background.mix(primary, 0.14))),
        ("--message-tips-bg", v(muted)),
        ("--workspace-btn-bg", v(secondary)),
        ("--sider-bg", v(sidebar)),
        ("--sider-fg", v(sidebar_fg)),
        ("--sider-border", v(sidebar_border)),
        ("--sider-accent", v(sidebar_accent)),
        ("--sider-accent-fg", v(sidebar_accent_fg)),
    ]
}

fn render_block(selector: &str, tokens: &BTreeMap<String, Rgb>) -> String {
    let mut out = format!("{selector} {{\n");
    for (name, value) in workmate_vars(tokens) {
        out.push_str(&format!("  {name}: {value};\n"));
    }
    out.push_str("}\n");
    out
}

fn render_stylesheet(ember: &BTreeMap<String, Rgb>, dusk: &BTreeMap<String, Rgb>) -> String {
    let mut out = String::new();
    out.push_str("/* Dusk & Ember (CSBU WorkMate)\n");
    out.push_str(" * Colour values derived from Orca \"Dusk & Ember Themes\" (orca-dusk-ember 1.1.0),\n");
    out.push_str(" * https://github.com/stablyai/orca-plugins/tree/main/plugins/dusk-ember at commit\n");
    out.push_str(&format!(" * {UPSTREAM_COMMIT}.\n"));
    out.push_str(" * :root = Ember (light); [data-theme='dark'] = Dusk (dark).\n");
    out.push_str(" * Terminal colours, tokenColors and Orca images were not used.\n");
    out.push_str(" * Generated by workmate-theme-import; do not edit by hand — change the converter.\n");
    out.push_str(" */\n\n");
    out.push_str(&render_block(":root", ember));
    out.push('\n');
    out.push_str(&render_block("[data-theme='dark']", dusk));
    out.push_str(
        r#"
.layout-sider {
  background-color: var(--sider-bg);
  border-right: 1px solid var(--sider-border);
  color: var(--sider-fg);
}

[data-theme='dark'] .layout-sider {
  background-color: var(--sider-bg);
  border-right: 1px solid var(--sider-border);
  color: var(--sider-fg);
}

.layout-sider-header {
  background: var(--color-primary);
  color: var(--primary-foreground);
}

[data-theme='dark'] .layout-sider-header {
  background: var(--color-primary);
  color: var(--primary-foreground);
}
"#,
    );
    out
}

/// sha512 SRI of bytes, for the SHAS sidecar.
pub fn recorded_sri(bytes: &[u8]) -> String {
    sri_sha512(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LICENSE: &[u8] = include_bytes!("../../../tools/theme-import/upstream/LICENSE");
    const DUSK: &[u8] = include_bytes!("../../../tools/theme-import/upstream/dusk.json");
    const EMBER: &[u8] = include_bytes!("../../../tools/theme-import/upstream/ember.json");

    #[test]
    fn vendored_files_match_the_pinned_git_blob_shas() {
        verify_upstream_blobs(LICENSE, DUSK, EMBER).unwrap();
    }

    #[test]
    fn ember_is_light_dusk_is_dark_and_primary_is_copied_verbatim() {
        let css = convert_vendored(DUSK, EMBER).unwrap();
        assert!(css.contains(":root {"));
        assert!(css.contains("[data-theme='dark'] {"));
        // Split on the actual selectors, not the comment that mentions them.
        let after_root = css.split_once(":root {").unwrap().1;
        let (root, rest) = after_root.split_once("[data-theme='dark'] {").unwrap();
        assert!(root.contains("--color-primary: #b4410f;"));
        assert!(root.contains("--color-bg-1: #fff6ec;"));
        assert!(root.contains("--primary-rgb: 180, 65, 15;"));
        assert!(root.contains("--sider-bg: #fde6cf;"));
        assert!(rest.contains("--color-primary: #c38bff;"));
        assert!(rest.contains("--color-bg-1: #1d1533;"));
        assert!(rest.contains("--primary-rgb: 195, 139, 255;"));
        assert!(rest.contains("--sider-bg: #150f28;"));
        // tokenColors (syntax / comments) must not appear.
        assert!(!css.to_lowercase().contains("comment"));
        assert!(!css.contains("#ff7ad9"), "a tokenColors-only colour leaked");
        // Header records the pin.
        assert!(css.contains(UPSTREAM_COMMIT));
    }

    #[test]
    fn missing_or_non_hex_token_is_refused() {
        let mut dusk: serde_json::Value = serde_json::from_slice(DUSK).unwrap();
        dusk["tokens"]["--primary"] = serde_json::json!("not-a-color");
        assert!(convert_vendored(&serde_json::to_vec(&dusk).unwrap(), EMBER).is_err());
        dusk["tokens"].as_object_mut().unwrap().remove("--primary");
        assert!(convert_vendored(&serde_json::to_vec(&dusk).unwrap(), EMBER).is_err());
    }

    #[test]
    fn convert_is_deterministic() {
        let a = convert_vendored(DUSK, EMBER).unwrap();
        let b = convert_vendored(DUSK, EMBER).unwrap();
        assert_eq!(a, b);
        assert!(a.ends_with('\n'));
        assert!(!a.contains('\r'));
    }
}

#[cfg(all(test, any(unix, windows)))]
mod install_artifacts {
    use super::*;
    use crate::hub::curated::signature_payload;
    use crate::hub::test_support::open_host_dir;
    use crate::hub::{
        BuiltinKey, BuiltinOutcome, BuiltinPack, BuiltinSet, CuratedInstallPolicy, HostExtensionsParent,
        HostSignedInstall, InstallKind, merge_builtin_policy, read_package_manifest,
    };
    use crate::signed_root_binding::SignedRootBindingStore;
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::path::PathBuf;
    use std::sync::Arc;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    #[test]
    fn committed_theme_pack_verifies_with_the_test_key_and_ensure_installs_it() {
        let root = repo_root();
        let seed_hex: String = std::fs::read_to_string(root.join("tools/builtin-sign/workmate-test-dev.hex"))
            .unwrap()
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .collect();
        let mut seed = [0u8; 32];
        hex::decode_to_slice(&seed_hex, &mut seed).unwrap();
        let key = SigningKey::from_bytes(&seed);

        let index: serde_json::Value =
            serde_json::from_slice(&std::fs::read(root.join("builtin/index.json")).unwrap()).unwrap();
        assert_eq!(index["pubkeyId"], "workmate-test-dev");
        assert_eq!(index["publicKeyHex"], hex::encode(key.verifying_key().to_bytes()));
        let entry = index["packs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "workmate.theme-dusk-ember")
            .expect("theme pack in index");
        assert_eq!(entry["version"], "1.0.0");

        let zip = std::fs::read(root.join("builtin").join(entry["zip"].as_str().unwrap())).unwrap();
        let raw = read_package_manifest(&zip).expect("zip has a manifest");
        let integrity = entry["integrity"].as_str().unwrap();
        let signature = entry["signature"].as_str().unwrap();
        assert_eq!(integrity, sri_sha512(&raw));
        assert_eq!(
            signature,
            hex::encode(
                key.sign(signature_payload("workmate.theme-dusk-ember", "1.0.0", integrity).as_bytes())
                    .to_bytes()
            )
        );

        let tmp = tempfile::tempdir().unwrap();
        let parent_path = tmp.path().join("extensions");
        std::fs::create_dir(&parent_path).unwrap();
        let store = Arc::new(SignedRootBindingStore::new());
        let parent = HostExtensionsParent::new(open_host_dir(&parent_path)).unwrap();
        let keys = [BuiltinKey {
            id: "workmate-test-dev",
            public_key: key.verifying_key().to_bytes(),
        }];
        let packs = [BuiltinPack {
            name: "workmate.theme-dusk-ember",
            version: "1.0.0",
            pubkey_id: "workmate-test-dev",
            integrity,
            signature,
            package_zip: &zip,
        }];
        let set = BuiltinSet {
            keys: &keys,
            packs: &packs,
        };
        let (policy, report) =
            merge_builtin_policy(CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new()), &set);
        assert!(report.conflicting_key_ids.is_empty() && report.invalid_key_ids.is_empty());
        let ctx = HostSignedInstall::new(parent, policy, store.clone())
            .unwrap()
            .with_builtin_pins(&set);
        assert_eq!(
            ctx.ensure_builtin_packs(&set).outcomes[0].1,
            BuiltinOutcome::Installed(InstallKind::Fresh)
        );
        let checked = store.lookup_checked("workmate.theme-dusk-ember").unwrap();
        assert_eq!(
            (checked.name(), checked.version()),
            ("workmate.theme-dusk-ember", "1.0.0")
        );
        assert_eq!(
            ctx.ensure_builtin_packs(&set).outcomes[0].1,
            BuiltinOutcome::AlreadyCurrent
        );
    }
}
