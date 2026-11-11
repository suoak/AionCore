//! Convert the vendored Orca Dusk & Ember app-theme JSON into WorkMate CSS.
//!
//! ```text
//! workmate-theme-import [--repo-root <dir>] [--check]
//! ```
//!
//! Without `--check`, rewrites `extensions/workmate.theme-dusk-ember/themes/dusk-ember.css`.
//! `--check` writes nothing and exits 3 when the committed CSS is out of date, or 2 when the
//! vendored upstream files do not match the pinned git blob SHAs.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aionui_extension::theme_import::{
    UPSTREAM_COMMIT, convert_vendored, css_matches, default_paths, verify_upstream_blobs,
};

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
                    "workmate-theme-import — Orca Dusk & Ember → WorkMate CSS\n\n  --repo-root <dir>  repository root (default: cwd)\n  --check            write nothing; exit 3 if CSS is stale, 2 if upstream SHAs differ"
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

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("workmate-theme-import: {e}");
            return ExitCode::from(1);
        }
    };
    let paths = default_paths(&args.repo_root);
    let license = match read(&paths.license) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("workmate-theme-import: {e}");
            return ExitCode::from(1);
        }
    };
    let dusk = match read(&paths.dusk) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("workmate-theme-import: {e}");
            return ExitCode::from(1);
        }
    };
    let ember = match read(&paths.ember) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("workmate-theme-import: {e}");
            return ExitCode::from(1);
        }
    };
    if let Err(e) = verify_upstream_blobs(&license, &dusk, &ember) {
        eprintln!("workmate-theme-import: upstream pin {UPSTREAM_COMMIT} mismatch: {e}");
        return ExitCode::from(2);
    }
    if args.check {
        let committed = match read(&paths.css) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("workmate-theme-import: {e}");
                return ExitCode::from(3);
            }
        };
        return match css_matches(&committed, &dusk, &ember) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("workmate-theme-import: {e}");
                ExitCode::from(3)
            }
        };
    }
    let css = match convert_vendored(&dusk, &ember) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("workmate-theme-import: {e}");
            return ExitCode::from(1);
        }
    };
    if let Some(parent) = paths.css.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        eprintln!("workmate-theme-import: {e}");
        return ExitCode::from(1);
    }
    if let Err(e) = std::fs::write(&paths.css, css.as_bytes()) {
        eprintln!("workmate-theme-import: {}: {e}", paths.css.display());
        return ExitCode::from(1);
    }
    println!("wrote {}", paths.css.display());
    ExitCode::SUCCESS
}
