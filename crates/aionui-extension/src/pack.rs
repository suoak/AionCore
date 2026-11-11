//! Author-side package helper (M9 E13): turn an extension **source directory** into the exact
//! two artifacts the Host installer accepts — a manifest whose top-level `files` list covers every
//! package file, and a deterministic zip.
//!
//! This is a build-time tool for plugin authors, not a Host feature: it adds no route, performs no
//! network access and never executes anything. It only *reads* the source directory (the caller
//! decides where, if anywhere, the results are written — see the `workmate-pack` binary). Signing
//! stays with the curator; this module only reports the manifest SRI a signature must cover.
//!
//! # What is produced
//! * `files`: one `{path, integrity}` entry per package file, sorted by canonical relative path
//!   (`/` separators), `integrity` = `sha512-<base64>` of the exact bytes that go into the zip. The
//!   manifest itself is never listed. The list is *recomputed from the directory* (an existing
//!   `files` list is replaced, so stale entries cannot survive).
//! * The manifest is re-serialized as pretty JSON (2 spaces, keys sorted, trailing newline). The
//!   result is run through [`parse_manifest`], i.e. every rule the Host enforces on a signed
//!   manifest (listed served assets, canonical paths, integrity inheritance, …) is checked at pack
//!   time, so a manifest that would be refused at install is refused here.
//! * The zip contains only regular files (no directory entries, so empty directories are not
//!   packaged): the manifest first, then every listed file in path order, fixed timestamp
//!   (1980-01-01), fixed mode `0644`, deflate level 6. The same inputs give the same bytes on the
//!   same build of this crate. The zip itself is not signed — the signature covers the manifest,
//!   the manifest covers each file's SRI.
//!
//! # Fail closed
//! Symlinks and every non-regular, non-directory entry; names that are not UTF-8, not canonical
//! (`\`, `..`, empty components, …) or that contain control characters; names that cannot exist on
//! Windows (DOS device names, trailing dot/space, `:` and other reserved characters, over 255
//! UTF-16 units — Windows is a required platform); case-fold collisions
//! (including with the manifest name); `$file:` references in the manifest (the Host never
//! resolves them on a signed manifest); and anything over the install limits
//! ([`LocalPackageLimits`], the manifest size cap) are all refused with a message that names the
//! offending relative path. Nothing is skipped silently except the fixed exclusions
//! ([`DEFAULT_EXCLUDED_NAMES`] and caller-supplied exact paths), which are reported in
//! [`PackOutput::excluded`] and are not inspected (so `node_modules/.bin` links do not matter).
//!
//! The source tree must not change while packing; bytes are read once and the digest and the zip
//! entry come from those same bytes.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

use serde_json::Value;

use crate::asset_paths::canonical_relative_path;
use crate::constants::EXTENSION_MANIFEST_FILE;
use crate::hub::curated::sri_sha512;
use crate::hub::local_package_install::LocalPackageLimits;
use crate::hub::signed_manifest::{MAX_MANIFEST_BYTES, MAX_NAME_BYTES, MAX_VERSION_BYTES};
use crate::manifest::parse_manifest;
use crate::types::ManifestFile;

/// Entry names (files or directories, at any depth) that are never packaged and never inspected.
pub const DEFAULT_EXCLUDED_NAMES: &[&str] = &[".git", "node_modules", ".DS_Store"];

/// Directory nesting safety valve (author-side; the installer has no depth limit of its own).
const MAX_DEPTH: usize = 64;

/// Fixed zip settings (determinism).
const ZIP_DEFLATE_LEVEL: i64 = 6;
const ZIP_FILE_MODE: u32 = 0o644;

#[derive(Debug, Clone, Default)]
pub struct PackOptions {
    /// Install limits to enforce (entries include the manifest). Defaults to the Host defaults.
    pub limits: LocalPackageLimits,
    /// Extra exact canonical relative paths (a file or a whole directory) to leave out.
    pub extra_excludes: Vec<String>,
}

/// One packaged (non-manifest) file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedFile {
    pub path: String,
    pub integrity: String,
    pub size: u64,
}

/// What changed in the `files` list compared with the source manifest.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FilesDiff {
    pub added: usize,
    pub removed: usize,
    pub updated: usize,
}

impl FilesDiff {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.updated == 0
    }
}

#[derive(Debug, Clone)]
pub struct PackOutput {
    pub name: String,
    pub version: String,
    /// The manifest bytes to sign and to ship in the zip.
    pub manifest: Vec<u8>,
    /// `sha512-…` SRI of [`Self::manifest`] (the value a signature entry's `integrity` must carry).
    pub manifest_integrity: String,
    pub zip: Vec<u8>,
    /// Packaged files, sorted by path (manifest not included).
    pub files: Vec<PackedFile>,
    /// Relative paths left out by an exclusion rule.
    pub excluded: Vec<String>,
    /// Difference between the source manifest's `files` list and the computed one.
    pub diff: FilesDiff,
}

#[derive(Debug, thiserror::Error)]
pub enum PackError {
    #[error("cannot read {path}: {reason}")]
    Io { path: String, reason: String },
    #[error("source directory has no regular {EXTENSION_MANIFEST_FILE}")]
    ManifestMissing,
    #[error("invalid manifest: {0}")]
    ManifestInvalid(String),
    #[error("manifest contains a `$file:` reference at {0}; the Host never resolves it on a signed manifest")]
    FileReference(String),
    #[error("{0}: symlinks and special files are not allowed in a package")]
    SymlinkOrSpecial(String),
    #[error("{0}: path is not valid UTF-8")]
    NonUtf8(String),
    #[error("{0}: not a canonical package path (or contains control characters)")]
    NonCanonical(String),
    #[error(
        "{0}: this name cannot exist on Windows (DOS device name such as con/aux/nul/com1, trailing dot or space, or one of : * ? \" < > | \\, or over 255 characters); rename it so the package installs on Windows"
    )]
    WindowsInvalidName(String),
    #[error("{0} and {1} collide when case-folded")]
    CaseFoldCollision(String, String),
    #[error("{0}: exceeds the per-file size limit")]
    FileTooLarge(String),
    #[error("package exceeds the entry or total size limit")]
    PackageTooLarge,
    #[error("{0}: directory nesting is too deep")]
    TooDeep(String),
    #[error("manifest exceeds the size limit")]
    ManifestTooLarge,
    #[error("failed to build the zip: {0}")]
    Zip(String),
}

fn io_err(path: &str, e: &std::io::Error) -> PackError {
    PackError::Io {
        path: path.to_owned(),
        reason: e.kind().to_string(),
    }
}

/// Open `path` as a regular file without following a final symlink, and read it (bounded).
fn read_regular_file(path: &Path, rel: &str, max: u64) -> Result<Vec<u8>, PackError> {
    // Check the type before opening: opening a FIFO or device would block or have side effects.
    match path.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => return Err(PackError::SymlinkOrSpecial(rel.to_owned())),
        Ok(_) => {}
        Err(e) => return Err(io_err(rel, &e)),
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path).map_err(|e| {
        // ELOOP (O_NOFOLLOW on a symlink) is reported as a symlink, not as an I/O problem.
        if path
            .symlink_metadata()
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        {
            PackError::SymlinkOrSpecial(rel.to_owned())
        } else {
            io_err(rel, &e)
        }
    })?;
    let meta = file.metadata().map_err(|e| io_err(rel, &e))?;
    if !meta.is_file() {
        return Err(PackError::SymlinkOrSpecial(rel.to_owned()));
    }
    if meta.len() > max {
        return Err(PackError::FileTooLarge(rel.to_owned()));
    }
    let mut bytes = Vec::new();
    file.take(max + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_err(rel, &e))?;
    if bytes.len() as u64 > max {
        return Err(PackError::FileTooLarge(rel.to_owned()));
    }
    Ok(bytes)
}

fn path_absent(path: &Path) -> bool {
    matches!(path.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
}

struct Walk<'a> {
    excluded_names: &'a [&'a str],
    extra_excludes: &'a [String],
    files: Vec<String>,
    excluded: Vec<String>,
    max_entries: usize,
}

impl Walk<'_> {
    fn is_excluded(&self, name: &str, rel: &str) -> bool {
        self.excluded_names.contains(&name) || self.extra_excludes.iter().any(|e| e == rel)
    }

    fn walk(&mut self, dir: &Path, prefix: &str, depth: usize) -> Result<(), PackError> {
        if depth > MAX_DEPTH {
            return Err(PackError::TooDeep(prefix.to_owned()));
        }
        let shown = if prefix.is_empty() { "." } else { prefix };
        let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(dir)
            .map_err(|e| io_err(shown, &e))?
            .map(|entry| entry.map(|e| e.file_name()))
            .collect::<Result<_, _>>()
            .map_err(|e| io_err(shown, &e))?;
        names.sort();
        for os_name in names {
            let lossy = os_name.to_string_lossy().into_owned();
            let rel_lossy = if prefix.is_empty() {
                lossy.clone()
            } else {
                format!("{prefix}/{lossy}")
            };
            let name = os_name.to_str().ok_or_else(|| PackError::NonUtf8(rel_lossy.clone()))?;
            let rel = rel_lossy;
            if self.is_excluded(name, &rel) {
                self.excluded.push(rel);
                continue;
            }
            if name.chars().any(|c| c.is_control()) || canonical_relative_path(&rel).is_none() {
                return Err(PackError::NonCanonical(rel));
            }
            let path = dir.join(&os_name);
            let meta = std::fs::symlink_metadata(&path).map_err(|e| io_err(&rel, &e))?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                return Err(PackError::SymlinkOrSpecial(rel));
            }
            if ft.is_dir() {
                self.walk(&path, &rel, depth + 1)?;
            } else if ft.is_file() {
                if rel == EXTENSION_MANIFEST_FILE {
                    continue; // read separately
                }
                self.files.push(rel);
                // +1: the manifest. Fail early on huge trees.
                if self.files.len() + 1 > self.max_entries {
                    return Err(PackError::PackageTooLarge);
                }
            } else {
                return Err(PackError::SymlinkOrSpecial(rel));
            }
        }
        Ok(())
    }
}

/// Refuse any `$file:` string anywhere in the manifest (JSON pointer of the first hit).
fn find_file_reference(value: &Value, at: &str) -> Option<String> {
    match value {
        Value::String(s) if s.starts_with("$file:") => Some(if at.is_empty() { "/".into() } else { at.to_owned() }),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_file_reference(v, &format!("{at}/{i}"))),
        Value::Object(map) => map
            .iter()
            .find_map(|(k, v)| find_file_reference(v, &format!("{at}/{k}"))),
        _ => None,
    }
}

fn previous_files(manifest: &Value) -> BTreeMap<String, String> {
    manifest
        .get("files")
        .and_then(|v| serde_json::from_value::<Vec<ManifestFile>>(v.clone()).ok())
        .map(|list| list.into_iter().map(|f| (f.path, f.integrity)).collect())
        .unwrap_or_default()
}

fn diff_files(previous: &BTreeMap<String, String>, now: &[PackedFile]) -> FilesDiff {
    let mut diff = FilesDiff::default();
    for file in now {
        match previous.get(&file.path) {
            None => diff.added += 1,
            Some(old) if *old != file.integrity => diff.updated += 1,
            Some(_) => {}
        }
    }
    diff.removed = previous.keys().filter(|p| !now.iter().any(|f| &f.path == *p)).count();
    diff
}

/// Pack the extension source directory `dir`. Read-only on `dir`.
pub fn pack_extension_dir(dir: &Path, options: &PackOptions) -> Result<PackOutput, PackError> {
    let limits = options.limits;
    for extra in &options.extra_excludes {
        if canonical_relative_path(extra).is_none() {
            return Err(PackError::NonCanonical(extra.clone()));
        }
    }

    // 1. Manifest (bounded, regular file, no symlink).
    let manifest_path = dir.join(EXTENSION_MANIFEST_FILE);
    let source_manifest = match read_regular_file(&manifest_path, EXTENSION_MANIFEST_FILE, MAX_MANIFEST_BYTES as u64) {
        Ok(bytes) => bytes,
        Err(PackError::Io { .. }) if path_absent(&manifest_path) => return Err(PackError::ManifestMissing),
        Err(PackError::FileTooLarge(_)) => return Err(PackError::ManifestTooLarge),
        Err(e) => return Err(e),
    };
    let mut manifest_json: Value =
        serde_json::from_slice(&source_manifest).map_err(|e| PackError::ManifestInvalid(e.to_string()))?;
    if let Some(at) = find_file_reference(&manifest_json, "") {
        return Err(PackError::FileReference(at));
    }
    let previous = previous_files(&manifest_json);

    // 2. Walk (symlink/special/canonical/exclusion rules).
    let mut walk = Walk {
        excluded_names: DEFAULT_EXCLUDED_NAMES,
        extra_excludes: &options.extra_excludes,
        files: Vec::new(),
        excluded: Vec::new(),
        max_entries: limits.max_entries,
    };
    walk.walk(dir, "", 0)?;
    let Walk {
        mut files, excluded, ..
    } = walk;
    files.sort();

    // 2b. Every packaged path must also be a valid Windows name: Windows is a required platform
    // and a package that only fails there at install time is refused at pack time instead.
    for rel in &files {
        if crate::hub::verified_reader::windows_portable_components(rel).is_err() {
            return Err(PackError::WindowsInvalidName(rel.clone()));
        }
    }

    // 3. Case-fold collisions (also against the manifest name).
    let mut folded: BTreeMap<String, String> = BTreeMap::new();
    folded.insert(
        EXTENSION_MANIFEST_FILE.to_lowercase(),
        EXTENSION_MANIFEST_FILE.to_owned(),
    );
    for rel in &files {
        if let Some(other) = folded.insert(rel.to_lowercase(), rel.clone()) {
            return Err(PackError::CaseFoldCollision(other, rel.clone()));
        }
    }
    // A file may not also be a directory prefix of another path, case-folded (cannot happen on a
    // real directory, but case-insensitive collisions between `a` and `A/b` can).
    for fold in folded.keys() {
        let mut end = 0;
        while let Some(pos) = fold[end..].find('/') {
            end += pos;
            if let Some(other) = folded.get(&fold[..end]) {
                return Err(PackError::CaseFoldCollision(other.clone(), folded[fold].clone()));
            }
            end += 1;
        }
    }

    // 4. Read each file once; digest and zip entry come from the same bytes.
    let mut packed: Vec<PackedFile> = Vec::with_capacity(files.len());
    let mut contents: Vec<Vec<u8>> = Vec::with_capacity(files.len());
    let mut total: u64 = 0;
    for rel in &files {
        let bytes = read_regular_file(&dir.join(rel), rel, limits.max_file_bytes)?;
        total = total.saturating_add(bytes.len() as u64);
        if total > limits.max_total_bytes {
            return Err(PackError::PackageTooLarge);
        }
        packed.push(PackedFile {
            path: rel.clone(),
            integrity: sri_sha512(&bytes),
            size: bytes.len() as u64,
        });
        contents.push(bytes);
    }

    // 5. Manifest with the recomputed `files` list.
    let root = manifest_json
        .as_object_mut()
        .ok_or_else(|| PackError::ManifestInvalid("manifest must be a JSON object".into()))?;
    if packed.is_empty() {
        root.remove("files");
    } else {
        let list: Vec<Value> = packed
            .iter()
            .map(|f| serde_json::json!({"path": f.path, "integrity": f.integrity}))
            .collect();
        root.insert("files".into(), Value::Array(list));
    }
    let mut manifest =
        serde_json::to_vec_pretty(&manifest_json).map_err(|e| PackError::ManifestInvalid(e.to_string()))?;
    manifest.push(b'\n');
    if manifest.len() > MAX_MANIFEST_BYTES || manifest.len() as u64 > limits.max_file_bytes {
        return Err(PackError::ManifestTooLarge);
    }
    total = total.saturating_add(manifest.len() as u64);
    if total > limits.max_total_bytes || packed.len() + 1 > limits.max_entries {
        return Err(PackError::PackageTooLarge);
    }
    // Everything the Host checks on a signed manifest, now.
    let parsed = parse_manifest(&manifest).map_err(|e| PackError::ManifestInvalid(e.to_string()))?;
    if parsed.name.len() > MAX_NAME_BYTES || parsed.version.len() > MAX_VERSION_BYTES {
        return Err(PackError::ManifestInvalid(
            "name or version is too long to be signed".into(),
        ));
    }

    // 6. Deterministic zip.
    let options_zip = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(ZIP_DEFLATE_LEVEL))
        .last_modified_time(zip::DateTime::default())
        .unix_permissions(ZIP_FILE_MODE);
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut put = |name: &str, bytes: &[u8]| -> Result<(), PackError> {
        writer
            .start_file(name, options_zip)
            .map_err(|e| PackError::Zip(e.to_string()))?;
        writer.write_all(bytes).map_err(|e| PackError::Zip(e.to_string()))
    };
    put(EXTENSION_MANIFEST_FILE, &manifest)?;
    for (file, bytes) in packed.iter().zip(&contents) {
        put(&file.path, bytes)?;
    }
    let zip = writer.finish().map_err(|e| PackError::Zip(e.to_string()))?.into_inner();

    let diff = diff_files(&previous, &packed);
    Ok(PackOutput {
        name: parsed.name,
        version: parsed.version,
        manifest_integrity: sri_sha512(&manifest),
        manifest,
        zip,
        files: packed,
        excluded,
        diff,
    })
}

/// True when `candidate_parent` (an existing directory) is `dir` or lies inside it. Used by the
/// CLI so outputs are never written into the tree being packed.
pub fn is_inside(dir: &Path, candidate_parent: &Path) -> bool {
    match (dir.canonicalize(), candidate_parent.canonicalize()) {
        (Ok(d), Ok(c)) => c.starts_with(d),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hub::local_package_install::read_package_manifest;

    fn write(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn base_manifest() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "name": "demo.plugin", "version": "1.0.0",
            "contributes": {"panels": [{"id": "board", "title": "Board", "entryPoint": "panels/board.html"}]}
        }))
        .unwrap()
    }

    fn sample() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &base_manifest());
        write(tmp.path(), "panels/board.html", b"<html>board</html>");
        write(tmp.path(), "panels/board.css", b"p{color:red}");
        write(tmp.path(), "assets/logo.bin", &[0u8, 1, 2, 255, 254]);
        tmp
    }

    fn zip_names(zip: &[u8]) -> Vec<String> {
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(zip)).unwrap();
        (0..a.len())
            .map(|i| a.by_index(i).unwrap().name().unwrap().into_owned())
            .collect()
    }

    #[test]
    fn lists_every_file_sorted_with_sri_and_excludes_the_manifest() {
        let tmp = sample();
        let out = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        let paths: Vec<&str> = out.files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["assets/logo.bin", "panels/board.css", "panels/board.html"]);
        assert_eq!(out.files[2].integrity, sri_sha512(b"<html>board</html>"));
        assert_eq!(out.files[2].size, 18);
        assert_eq!((out.name.as_str(), out.version.as_str()), ("demo.plugin", "1.0.0"));
        let parsed = parse_manifest(&out.manifest).unwrap();
        assert_eq!(parsed.files.len(), 3);
        assert!(parsed.files.iter().all(|f| f.path != EXTENSION_MANIFEST_FILE));
        assert_eq!(out.manifest_integrity, sri_sha512(&out.manifest));
        assert_eq!(
            out.diff,
            FilesDiff {
                added: 3,
                removed: 0,
                updated: 0
            }
        );
        // Manifest first, then files in path order, files only (no directory entries).
        assert_eq!(
            zip_names(&out.zip),
            [
                EXTENSION_MANIFEST_FILE,
                "assets/logo.bin",
                "panels/board.css",
                "panels/board.html"
            ]
        );
        assert_eq!(read_package_manifest(&out.zip).unwrap(), out.manifest);
    }

    #[test]
    fn output_is_deterministic_and_idempotent_over_its_own_manifest() {
        let tmp = sample();
        let a = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        let b = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        assert_eq!(a.zip, b.zip);
        assert_eq!(a.manifest, b.manifest);
        // Feed the produced manifest back in: same bytes, empty diff.
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &a.manifest);
        let c = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        assert_eq!(c.manifest, a.manifest);
        assert_eq!(c.zip, a.zip);
        assert!(c.diff.is_empty());
        // Source untouched.
        assert_eq!(
            std::fs::read(tmp.path().join(EXTENSION_MANIFEST_FILE)).unwrap(),
            a.manifest
        );
    }

    #[test]
    fn existing_files_list_is_replaced_and_diffed() {
        let tmp = sample();
        let mut m: Value = serde_json::from_slice(&base_manifest()).unwrap();
        m["files"] = serde_json::json!([
            {"path": "panels/board.html", "integrity": sri_sha512(b"old")},
            {"path": "gone.txt", "integrity": sri_sha512(b"gone")},
        ]);
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &serde_json::to_vec(&m).unwrap());
        let out = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        assert_eq!(
            out.diff,
            FilesDiff {
                added: 2,
                removed: 1,
                updated: 1
            }
        );
        assert!(
            parse_manifest(&out.manifest)
                .unwrap()
                .files
                .iter()
                .all(|f| f.path != "gone.txt")
        );
    }

    #[test]
    fn default_and_extra_exclusions_are_reported_and_not_inspected() {
        let tmp = sample();
        write(tmp.path(), ".git/HEAD", b"ref");
        write(tmp.path(), "node_modules/x/index.js", b"x");
        write(tmp.path(), "panels/.DS_Store", b"junk");
        write(tmp.path(), "scratch/notes.txt", b"n");
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", tmp.path().join("node_modules/link")).unwrap();
        let out = pack_extension_dir(
            tmp.path(),
            &PackOptions {
                extra_excludes: vec!["scratch".into()],
                ..PackOptions::default()
            },
        )
        .unwrap();
        assert!(
            out.files
                .iter()
                .all(|f| !f.path.starts_with(".git") && !f.path.contains("node_modules"))
        );
        assert!(
            out.files
                .iter()
                .all(|f| f.path != "scratch/notes.txt" && !f.path.ends_with(".DS_Store"))
        );
        assert_eq!(out.excluded, [".git", "node_modules", "panels/.DS_Store", "scratch"]);
        // Non-canonical extra exclude spellings are refused.
        for bad in ["./scratch", "scratch/", "../x", "a//b"] {
            assert!(matches!(
                pack_extension_dir(
                    tmp.path(),
                    &PackOptions {
                        extra_excludes: vec![bad.into()],
                        ..Default::default()
                    }
                ),
                Err(PackError::NonCanonical(_))
            ));
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_and_special_files_are_refused() {
        use std::os::unix::fs::symlink;
        let tmp = sample();
        symlink("board.html", tmp.path().join("panels/alias.html")).unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(p)) if p == "panels/alias.html"
        ));
        std::fs::remove_file(tmp.path().join("panels/alias.html")).unwrap();
        // Dangling and directory symlinks too.
        symlink("nowhere", tmp.path().join("dangling")).unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(_))
        ));
        std::fs::remove_file(tmp.path().join("dangling")).unwrap();
        symlink("panels", tmp.path().join("dirlink")).unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(_))
        ));
        std::fs::remove_file(tmp.path().join("dirlink")).unwrap();
        // FIFO: refused without being opened (would block).
        let fifo = std::ffi::CString::new(tmp.path().join("pipe").to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(p)) if p == "pipe"
        ));
        std::fs::remove_file(tmp.path().join("pipe")).unwrap();
        // Manifest symlink / FIFO.
        let real = tmp.path().join("real.json");
        std::fs::rename(tmp.path().join(EXTENSION_MANIFEST_FILE), &real).unwrap();
        symlink("real.json", tmp.path().join(EXTENSION_MANIFEST_FILE)).unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(_))
        ));
        std::fs::remove_file(tmp.path().join(EXTENSION_MANIFEST_FILE)).unwrap();
        let fifo = std::ffi::CString::new(tmp.path().join(EXTENSION_MANIFEST_FILE).to_str().unwrap()).unwrap();
        // SAFETY: as above.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::SymlinkOrSpecial(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_canonical_names_are_refused() {
        for name in ["a\\b.txt", "bad\nname.txt", "tab\there"] {
            let tmp = sample();
            write(tmp.path(), name, b"x");
            assert!(
                matches!(
                    pack_extension_dir(tmp.path(), &PackOptions::default()),
                    Err(PackError::NonCanonical(_))
                ),
                "{name:?}"
            );
        }
        use std::os::unix::ffi::OsStrExt;
        let tmp = sample();
        let bad = std::ffi::OsStr::from_bytes(&[b'f', 0xff, b'.', b't']);
        std::fs::write(tmp.path().join(bad), b"x").unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::NonUtf8(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn names_that_cannot_exist_on_windows_are_refused_at_pack_time() {
        for name in [
            "docs/aux.md",
            "CON.txt",
            "panels/nul",
            "a:b.txt",
            "trailing.",
            "dir./x.txt",
            "q?.txt",
            "star*.txt",
            "lt<.txt",
            "com1.log",
        ] {
            let tmp = sample();
            write(tmp.path(), name, b"x");
            assert!(
                matches!(
                    pack_extension_dir(tmp.path(), &PackOptions::default()),
                    Err(PackError::WindowsInvalidName(p)) if p == name
                ),
                "{name:?}"
            );
        }
        // Look-alikes that are fine on Windows still pack.
        let tmp = sample();
        for name in [
            "docs/console.md",
            "auxiliary.txt",
            "com10.txt",
            "lpt.txt",
            "a b/c d.txt",
        ] {
            write(tmp.path(), name, b"x");
        }
        assert!(pack_extension_dir(tmp.path(), &PackOptions::default()).is_ok());
        // An excluded offender is not inspected, so it does not block packing.
        let tmp = sample();
        write(tmp.path(), "scratch/aux.md", b"x");
        let opts = PackOptions {
            extra_excludes: vec!["scratch".into()],
            ..PackOptions::default()
        };
        assert!(pack_extension_dir(tmp.path(), &opts).is_ok());
    }

    #[test]
    fn case_fold_collisions_are_refused_including_with_the_manifest() {
        // On a case-insensitive filesystem (Windows, default macOS) the aliasing writes below may
        // legitimately fail or overwrite; the helper must not panic there.
        fn write(root: &Path, rel: &str, bytes: &[u8]) {
            let p = root.join(rel);
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, bytes);
        }
        let tmp = sample();
        write(tmp.path(), "Panels/Board.html", b"x");
        // On a case-insensitive filesystem the write above aliases; only assert when distinct.
        if tmp.path().join("Panels").read_dir().map(|d| d.count()).unwrap_or(0) == 1
            && !tmp.path().join("panels").join("Board.html").exists()
        {
            assert!(matches!(
                pack_extension_dir(tmp.path(), &PackOptions::default()),
                Err(PackError::CaseFoldCollision(_, _))
            ));
        }
        let tmp = sample();
        write(tmp.path(), "AION-EXTENSION.JSON", b"{}");
        if tmp.path().join("AION-EXTENSION.JSON").exists()
            && std::fs::read(tmp.path().join(EXTENSION_MANIFEST_FILE)).unwrap() == base_manifest()
        {
            assert!(matches!(
                pack_extension_dir(tmp.path(), &PackOptions::default()),
                Err(PackError::CaseFoldCollision(_, _))
            ));
        }
        // File `a` vs directory `A/b`.
        let tmp = sample();
        write(tmp.path(), "a", b"x");
        write(tmp.path(), "A/b", b"y");
        if tmp.path().join("a").is_file() && tmp.path().join("A").join("b").is_file() {
            assert!(matches!(
                pack_extension_dir(tmp.path(), &PackOptions::default()),
                Err(PackError::CaseFoldCollision(_, _))
            ));
        }
    }

    #[test]
    fn install_limits_are_enforced() {
        let tmp = sample();
        let lim = |max_entries, max_file_bytes, max_total_bytes| PackOptions {
            limits: LocalPackageLimits {
                max_entries,
                max_file_bytes,
                max_total_bytes,
            },
            ..PackOptions::default()
        };
        // 3 files + manifest = 4 entries: exactly fits, one fewer does not.
        assert!(pack_extension_dir(tmp.path(), &lim(4, 1 << 20, 1 << 20)).is_ok());
        assert!(matches!(
            pack_extension_dir(tmp.path(), &lim(3, 1 << 20, 1 << 20)),
            Err(PackError::PackageTooLarge)
        ));
        // Per-file: the largest payload is 18 bytes (but the manifest is bigger than that).
        assert!(
            matches!(pack_extension_dir(tmp.path(), &lim(10, 17, 1 << 20)), Err(PackError::FileTooLarge(p)) if p == "panels/board.html")
        );
        assert!(matches!(
            pack_extension_dir(tmp.path(), &lim(10, 64, 1 << 20)),
            Err(PackError::ManifestTooLarge)
        ));
        // Total (files + manifest).
        assert!(matches!(
            pack_extension_dir(tmp.path(), &lim(10, 1 << 20, 100)),
            Err(PackError::PackageTooLarge)
        ));
        // The installer's own limits agree with what the packer accepted (boundary included).
        let out = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        let total: u64 = out.files.iter().map(|f| f.size).sum::<u64>() + out.manifest.len() as u64;
        assert!(pack_extension_dir(tmp.path(), &lim(4, 1 << 20, total)).is_ok());
        assert!(pack_extension_dir(tmp.path(), &lim(4, 1 << 20, total - 1)).is_err());
    }

    #[test]
    fn manifest_problems_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestMissing)
        ));
        write(tmp.path(), EXTENSION_MANIFEST_FILE, b"not json");
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestInvalid(_))
        ));
        write(tmp.path(), EXTENSION_MANIFEST_FILE, b"[]");
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestInvalid(_))
        ));
        write(
            tmp.path(),
            EXTENSION_MANIFEST_FILE,
            br#"{"name":"d","version":"1.0.0","description":"$file:desc.txt"}"#,
        );
        assert!(
            matches!(pack_extension_dir(tmp.path(), &PackOptions::default()), Err(PackError::FileReference(at)) if at == "/description")
        );
        // Oversize manifest.
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &vec![b' '; MAX_MANIFEST_BYTES + 1]);
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestTooLarge)
        ));
    }

    #[test]
    fn a_manifest_the_host_would_refuse_is_refused_at_pack_time() {
        let tmp = sample();
        // References a served asset that is not in the directory => not listed => parse refuses.
        let mut m: Value = serde_json::from_slice(&base_manifest()).unwrap();
        m["contributes"]["settingsTabs"] =
            serde_json::json!([{"id": "t", "name": "T", "entryPoint": "missing/tab.html"}]);
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &serde_json::to_vec(&m).unwrap());
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestInvalid(_))
        ));
        // A declared integrity that disagrees with the real bytes is refused too.
        let mut m: Value = serde_json::from_slice(&base_manifest()).unwrap();
        m["contributes"]["panels"][0]["entryPointIntegrity"] = Value::String(sri_sha512(b"other"));
        write(tmp.path(), EXTENSION_MANIFEST_FILE, &serde_json::to_vec(&m).unwrap());
        assert!(matches!(
            pack_extension_dir(tmp.path(), &PackOptions::default()),
            Err(PackError::ManifestInvalid(_))
        ));
    }

    #[test]
    fn a_directory_with_only_the_manifest_packs_without_a_files_list() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            EXTENSION_MANIFEST_FILE,
            br#"{"name":"d","version":"1.0.0","files":[]}"#,
        );
        let out = pack_extension_dir(tmp.path(), &PackOptions::default()).unwrap();
        assert!(out.files.is_empty());
        assert!(!String::from_utf8(out.manifest.clone()).unwrap().contains("files"));
        assert_eq!(zip_names(&out.zip), [EXTENSION_MANIFEST_FILE]);
        let mut a = zip::ZipArchive::new(std::io::Cursor::new(&out.zip)).unwrap();
        let mut e = a.by_index(0).unwrap();
        // The zip crate records unix mode bits differently when built for Windows; the Host
        // installer only looks at the file-type bits, so only unix pins the exact mode.
        #[cfg(unix)]
        assert_eq!(e.unix_mode().map(|m| m & 0o777), Some(0o644));
        let mut buf = Vec::new();
        e.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, out.manifest);
    }

    #[test]
    fn is_inside_detects_nested_and_equal_dirs() {
        let tmp = sample();
        assert!(is_inside(tmp.path(), tmp.path()));
        assert!(is_inside(tmp.path(), &tmp.path().join("panels")));
        let other = tempfile::tempdir().unwrap();
        assert!(!is_inside(tmp.path(), other.path()));
    }
}
