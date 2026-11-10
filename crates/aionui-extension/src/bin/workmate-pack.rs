//! `workmate-pack` — CSBU WorkMate author-side packing helper (M9 E13).
//!
//! Computes the top-level `files` list (canonical paths + sha512 SRI) for an extension source
//! directory and builds a deterministic zip the Host installer accepts. Read-only on the source
//! directory unless `--update-manifest` is given. No network, no exec, no Host access; signing is a
//! separate curator step (the report prints the manifest SRI a signature must cover).

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use aionui_extension::pack::{PackError, PackOptions, PackOutput, is_inside, pack_extension_dir};
use serde_json::json;

const USAGE: &str = "workmate-pack — CSBU WorkMate plugin packing helper

Usage:
  workmate-pack <dir> [--zip-out <file>] [--manifest-out <file>] [--update-manifest]
                      [--exclude <relative-path>]... [--force] [--check]

  <dir>               extension source directory (contains aion-extension.json)
  --zip-out <file>    write the deterministic package zip here
  --manifest-out <f>  write the manifest (with the computed `files`) here
  --update-manifest   rewrite <dir>/aion-extension.json in place with the computed `files`
  --exclude <path>    leave out an exact relative file or directory (repeatable);
                      .git, node_modules and .DS_Store are always left out
  --force             overwrite existing output files
  --check             write nothing; exit 3 when the manifest `files` list is out of date

Without any output option nothing is written; a JSON report is printed to stdout.
Outputs must not be inside <dir>. Symlinks, special files, non-canonical or case-colliding
names and anything over the install limits are refused.
";

#[derive(Default)]
struct Args {
    dir: Option<PathBuf>,
    zip_out: Option<PathBuf>,
    manifest_out: Option<PathBuf>,
    update_manifest: bool,
    excludes: Vec<String>,
    force: bool,
    check: bool,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut args = Args::default();
    while let Some(a) = it.next() {
        let mut value = |flag: &str| it.next().ok_or_else(|| format!("{flag} needs a value"));
        match a.as_str() {
            "--zip-out" => args.zip_out = Some(PathBuf::from(value("--zip-out")?)),
            "--manifest-out" => args.manifest_out = Some(PathBuf::from(value("--manifest-out")?)),
            "--exclude" => args.excludes.push(value("--exclude")?),
            "--update-manifest" => args.update_manifest = true,
            "--force" => args.force = true,
            "--check" => args.check = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            _ if args.dir.is_none() => args.dir = Some(PathBuf::from(a)),
            _ => return Err("only one source directory may be given".into()),
        }
    }
    if args.check && (args.zip_out.is_some() || args.manifest_out.is_some() || args.update_manifest) {
        return Err("--check writes nothing and cannot be combined with output options".into());
    }
    Ok(args)
}

/// Write `bytes` to `out` without ever following a symlink at `out`. Refuses to replace an
/// existing file unless `force`.
fn write_output(out: &Path, bytes: &[u8], force: bool) -> Result<(), String> {
    match out.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            return Err(format!("{}: output exists and is not a regular file", out.display()));
        }
        Ok(_) if !force => return Err(format!("{}: already exists (use --force)", out.display())),
        Ok(_) | Err(_) => {}
    }
    // Temp file next to the destination, then rename: a failure never leaves a half-written output.
    let name = out
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a file path", out.display()))?;
    let tmp = out.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, out)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("{}: {}", out.display(), e.kind()));
    }
    Ok(())
}

fn check_outside(dir: &Path, out: &Path) -> Result<(), String> {
    let parent = out
        .parent()
        .map(|p| if p.as_os_str().is_empty() { Path::new(".") } else { p })
        .unwrap_or_else(|| Path::new("."));
    if is_inside(dir, parent) {
        return Err(format!(
            "{}: outputs must not be written inside the source directory",
            out.display()
        ));
    }
    Ok(())
}

fn report(out: &PackOutput, written: &[&Path]) -> serde_json::Value {
    json!({
        "name": out.name,
        "version": out.version,
        "manifestIntegrity": out.manifest_integrity,
        "fileCount": out.files.len(),
        "totalBytes": out.files.iter().map(|f| f.size).sum::<u64>(),
        "filesChanged": !out.diff.is_empty(),
        "diff": {"added": out.diff.added, "removed": out.diff.removed, "updated": out.diff.updated},
        "files": out.files.iter().map(|f| json!({"path": f.path, "integrity": f.integrity, "size": f.size})).collect::<Vec<_>>(),
        "excluded": out.excluded,
        "written": written.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
    })
}

fn run() -> Result<ExitCode, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.is_empty() || argv.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(if argv.is_empty() {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        });
    }
    let args = parse_args(argv.into_iter())?;
    let dir = args.dir.ok_or("a source directory is required")?;
    if !dir.is_dir() {
        return Err(format!("{}: not a directory", dir.display()));
    }
    for out in [args.zip_out.as_deref(), args.manifest_out.as_deref()]
        .into_iter()
        .flatten()
    {
        check_outside(&dir, out)?;
    }
    let options = PackOptions {
        extra_excludes: args.excludes,
        ..PackOptions::default()
    };
    let packed = pack_extension_dir(&dir, &options).map_err(|e: PackError| e.to_string())?;

    let mut written: Vec<&Path> = Vec::new();
    let manifest_in_place = dir.join("aion-extension.json");
    if let Some(out) = args.zip_out.as_deref() {
        write_output(out, &packed.zip, args.force)?;
        written.push(out);
    }
    if let Some(out) = args.manifest_out.as_deref() {
        write_output(out, &packed.manifest, args.force)?;
        written.push(out);
    }
    if args.update_manifest {
        write_output(&manifest_in_place, &packed.manifest, true)?;
        written.push(&manifest_in_place);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&report(&packed, &written)).map_err(|e| e.to_string())?
    );
    if args.check && !packed.diff.is_empty() {
        eprintln!("workmate-pack: the manifest `files` list is out of date");
        return Ok(ExitCode::from(3));
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(message) => {
            eprintln!("workmate-pack: {message}");
            ExitCode::from(1)
        }
    }
}
