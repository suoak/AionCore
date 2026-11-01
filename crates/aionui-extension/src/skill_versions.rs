//! Append-only on-disk skill version snapshots (M8 E10-1).
//!
//! Layout: `{skills_root}/.versions/{skill_id}/{version_no}/` holds a full
//! copy of the skill directory as it was when the version was recorded
//! (D10-1: copy, never symlink). A version directory is written once and never
//! overwritten or removed (D10-4: keep all versions).
//!
//! [`snapshot_skill_dir`] computes the `content_hash` (SHA-256 over SKILL.md and
//! the whole file tree), a manifest, and `has_scripts`. A version with
//! `has_scripts = true` must go through human review before the skill is live;
//! the DB layer enforces that by setting `skills.review_status = 'pending'`.

use std::io;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Hidden directory under the skills root that holds version snapshots.
/// Skill scans skip it so snapshots never show up as live skills.
pub const SKILL_VERSIONS_DIR: &str = ".versions";

/// Directories that never belong to a snapshot.
const SKIPPED_DIRS: &[&str] = &[SKILL_VERSIONS_DIR, ".git"];

/// File extensions treated as executable scripts.
const SCRIPT_EXTENSIONS: &[&str] = &[
    "sh", "bash", "zsh", "fish", "py", "js", "mjs", "cjs", "ts", "rb", "pl", "php", "lua", "ps1", "psm1", "bat", "cmd",
    "exe", "com", "vbs",
];

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SnapshotFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub script: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SkillSnapshot {
    pub content_hash: String,
    pub has_scripts: bool,
    pub files: Vec<SnapshotFile>,
}

impl SkillSnapshot {
    /// JSON stored in `skill_versions.manifest`.
    pub fn manifest_json(&self) -> String {
        let scripts: Vec<&str> = self
            .files
            .iter()
            .filter(|f| f.script)
            .map(|f| f.path.as_str())
            .collect();
        serde_json::json!({ "files": self.files, "scripts": scripts }).to_string()
    }
}

/// Whether a file counts as a script: known script extension, anything under a
/// `scripts/` or `bin/` directory, a `#!` shebang, or (unix) an executable bit.
pub fn is_script(rel_path: &str, head: &[u8], executable: bool) -> bool {
    let lower = rel_path.to_ascii_lowercase();
    let in_script_dir = lower
        .split('/')
        .rev()
        .skip(1)
        .any(|seg| seg == "scripts" || seg == "bin");
    let by_ext = Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| SCRIPT_EXTENSIONS.contains(&e));
    in_script_dir || by_ext || head.starts_with(b"#!") || executable
}

/// Hash and inventory a skill directory. Symlinks are rejected; `.versions`
/// and `.git` are skipped. Paths are `/`-separated and sorted, so the hash is
/// stable across platforms.
pub fn snapshot_skill_dir(dir: &Path) -> io::Result<SkillSnapshot> {
    let mut paths = Vec::new();
    collect_files(dir, dir, &mut paths)?;
    paths.sort();

    let mut tree = Sha256::new();
    let mut files = Vec::with_capacity(paths.len());
    for rel in paths {
        let full = dir.join(&rel);
        let bytes = std::fs::read(&full)?;
        let executable = is_executable(&full)?;
        let file_hash = hex::encode(Sha256::digest(&bytes));
        tree.update(rel.as_bytes());
        tree.update([0u8]);
        tree.update((bytes.len() as u64).to_le_bytes());
        tree.update(&bytes);
        let script = is_script(&rel, &bytes[..bytes.len().min(2)], executable);
        files.push(SnapshotFile {
            path: rel,
            size: bytes.len() as u64,
            sha256: file_hash,
            script,
        });
    }
    if !files.iter().any(|f| f.path == crate::constants::SKILL_MANIFEST_FILE) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "skill directory has no SKILL.md",
        ));
    }
    Ok(SkillSnapshot {
        content_hash: hex::encode(tree.finalize()),
        has_scripts: files.iter().any(|f| f.script),
        files,
    })
}

/// Directory of one version: `{skills_root}/.versions/{skill_id}/{version_no}`.
pub fn version_dir(skills_root: &Path, skill_id: &str, version_no: i64) -> io::Result<PathBuf> {
    let mut comps = Path::new(skill_id).components();
    let safe = matches!((comps.next(), comps.next()), (Some(Component::Normal(_)), None)) && !skill_id.starts_with('.');
    if !safe || version_no < 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid skill id or version number",
        ));
    }
    Ok(skills_root
        .join(SKILL_VERSIONS_DIR)
        .join(skill_id)
        .join(version_no.to_string()))
}

/// Copy `src_dir` into a new version directory. Fails with `AlreadyExists` if
/// that version was already stored: versions are append-only. The copy goes to
/// a temporary sibling first and is renamed into place, so a crash never leaves
/// a half-written version behind.
pub fn store_version(skills_root: &Path, skill_id: &str, version_no: i64, src_dir: &Path) -> io::Result<PathBuf> {
    version_dir(skills_root, skill_id, version_no)?;
    let staging = stage_version_files(
        skills_root,
        skill_id,
        &format!("{version_no}-{}", std::process::id()),
        src_dir,
    )?;
    match commit_staged_version(skills_root, skill_id, version_no, &staging) {
        Ok(target) => Ok(target),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&staging);
            Err(e)
        }
    }
}

/// Copy `src_dir` into a hidden staging directory next to the version dirs of
/// `skill_id` (`.versions/<skill_id>/.staging-<tag>`). Used to put the files on
/// disk BEFORE the version row is inserted, so a disk failure never leaves a
/// version row without files.
pub fn stage_version_files(skills_root: &Path, skill_id: &str, tag: &str, src_dir: &Path) -> io::Result<PathBuf> {
    let probe = version_dir(skills_root, skill_id, 1)?;
    let parent = probe.parent().expect("version dir has a parent");
    if tag.is_empty() || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid staging tag"));
    }
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".staging-{tag}"));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let result = (|| {
        let mut paths = Vec::new();
        collect_files(src_dir, src_dir, &mut paths)?;
        for rel in &paths {
            let dst = staging.join(rel);
            if let Some(p) = dst.parent() {
                std::fs::create_dir_all(p)?;
            }
            std::fs::copy(src_dir.join(rel), &dst)?;
        }
        std::fs::create_dir_all(&staging)
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    Ok(staging)
}

/// Atomically move a staged copy into `.versions/<skill_id>/<version_no>`.
/// Refuses to overwrite: versions are append-only.
pub fn commit_staged_version(
    skills_root: &Path,
    skill_id: &str,
    version_no: i64,
    staging: &Path,
) -> io::Result<PathBuf> {
    let target = version_dir(skills_root, skill_id, version_no)?;
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("skill version {skill_id}/{version_no} already stored"),
        ));
    }
    std::fs::rename(staging, &target)?;
    Ok(target)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if meta.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("symlink not allowed in skill snapshot: {}", path.display()),
            ));
        }
        if meta.is_dir() {
            if SKIPPED_DIRS.contains(&name.as_ref()) {
                continue;
            }
            collect_files(root, &path, out)?;
        } else if meta.is_file() {
            let rel = path
                .strip_prefix(root)
                .map_err(io::Error::other)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push(rel);
        }
    }
    Ok(())
}

/// Text files larger than this are listed as changed but not diffed.
pub const DIFF_MAX_BYTES: usize = 512 * 1024;

struct DiffSide {
    bytes: Vec<u8>,
    script: bool,
}

fn read_side(root: &Path, rel: &str) -> io::Result<Option<DiffSide>> {
    let full = root.join(rel);
    match std::fs::read(&full) {
        Ok(bytes) => {
            let executable = is_executable(&full)?;
            let script = is_script(rel, &bytes[..bytes.len().min(2)], executable);
            Ok(Some(DiffSide { bytes, script }))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

fn is_binary(bytes: &[u8]) -> bool {
    bytes[..bytes.len().min(8192)].contains(&0) || std::str::from_utf8(bytes).is_err()
}

/// E10-3: per-file diff from version dir `from` to version dir `to`.
/// Unchanged files are omitted; text files get a unified diff (3 lines of
/// context), binary or oversized files are only listed. Sorted by path.
pub fn diff_version_dirs(from: &Path, to: &Path) -> io::Result<Vec<aionui_api_types::SkillFileDiff>> {
    let mut paths = Vec::new();
    collect_files(from, from, &mut paths)?;
    collect_files(to, to, &mut paths)?;
    paths.sort();
    paths.dedup();
    let mut out = Vec::new();
    for rel in paths {
        let old = read_side(from, &rel)?;
        let new = read_side(to, &rel)?;
        let status = match (&old, &new) {
            (Some(a), Some(b)) if a.bytes == b.bytes && a.script == b.script => continue,
            (Some(_), Some(_)) => "modified",
            (None, Some(_)) => "added",
            (Some(_), None) => "removed",
            (None, None) => continue,
        };
        let empty: &[u8] = &[];
        let old_bytes = old.as_ref().map_or(empty, |s| s.bytes.as_slice());
        let new_bytes = new.as_ref().map_or(empty, |s| s.bytes.as_slice());
        let script = old.as_ref().is_some_and(|s| s.script) || new.as_ref().is_some_and(|s| s.script);
        let binary = is_binary(old_bytes) || is_binary(new_bytes);
        let too_large = !binary && (old_bytes.len() > DIFF_MAX_BYTES || new_bytes.len() > DIFF_MAX_BYTES);
        let diff = if binary || too_large {
            None
        } else {
            // Both sides are valid UTF-8 (checked by `is_binary`).
            let a = std::str::from_utf8(old_bytes).unwrap_or_default();
            let b = std::str::from_utf8(new_bytes).unwrap_or_default();
            Some(
                similar::TextDiff::from_lines(a, b)
                    .unified_diff()
                    .context_radius(3)
                    .header(&format!("a/{rel}"), &format!("b/{rel}"))
                    .to_string(),
            )
        };
        out.push(aionui_api_types::SkillFileDiff {
            path: rel,
            status: status.to_string(),
            binary,
            script,
            diff,
            too_large,
        });
    }
    Ok(out)
}

/// E10-3: replace the files of the live skill dir with a copy of version
/// `version_no`. The version itself is never touched (append-only). The new
/// content is staged next to the versions first, then swapped in with two
/// renames; on failure the old live dir is put back. A `.git` dir in the live
/// skill (never part of a snapshot) is carried over.
pub fn restore_version_into(skills_root: &Path, skill_id: &str, version_no: i64, live_dir: &Path) -> io::Result<()> {
    let src = version_dir(skills_root, skill_id, version_no)?;
    if !src.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("skill version {skill_id}/{version_no} has no files"),
        ));
    }
    if !live_dir.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "live skill directory is missing",
        ));
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let tag = format!("restore-{version_no}-{}-{nanos}", std::process::id());
    let staging = stage_version_files(skills_root, skill_id, &tag, &src)?;
    let parked = staging.with_file_name(format!(".old-{tag}"));
    if let Err(e) = std::fs::rename(live_dir, &parked) {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    if let Err(e) = std::fs::rename(&staging, live_dir) {
        let _ = std::fs::rename(&parked, live_dir);
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    let git = parked.join(".git");
    if git.is_dir() {
        let _ = std::fs::rename(&git, live_dir.join(".git"));
    }
    // The previous content was recorded as a version before the restore, so
    // the parked copy holds nothing that is not kept elsewhere.
    let _ = std::fs::remove_dir_all(&parked);
    Ok(())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> io::Result<bool> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> io::Result<bool> {
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn snapshot_hash_is_stable_and_tracks_every_file() {
        let a = tempfile::tempdir().unwrap();
        write(a.path(), "SKILL.md", "# demo");
        write(a.path(), "ref/notes.md", "notes");
        let first = snapshot_skill_dir(a.path()).unwrap();
        assert!(!first.has_scripts);
        assert_eq!(first.files.len(), 2);
        assert_eq!(snapshot_skill_dir(a.path()).unwrap().content_hash, first.content_hash);

        write(a.path(), "ref/notes.md", "changed");
        assert_ne!(snapshot_skill_dir(a.path()).unwrap().content_hash, first.content_hash);

        // .versions is never part of the snapshot.
        write(a.path(), "ref/notes.md", "notes");
        write(a.path(), ".versions/x/1/SKILL.md", "old");
        assert_eq!(snapshot_skill_dir(a.path()).unwrap().content_hash, first.content_hash);
    }

    #[test]
    fn scripts_are_detected() {
        assert!(is_script("run.py", b"", false));
        assert!(is_script("scripts/setup", b"", false));
        assert!(is_script("tool", b"#!", false));
        assert!(is_script("tool", b"", true));
        assert!(!is_script("SKILL.md", b"# ", false));
        assert!(!is_script("docs/scripts.md", b"", false));

        let d = tempfile::tempdir().unwrap();
        write(d.path(), "SKILL.md", "# demo");
        write(d.path(), "helpers/fetch.sh", "echo hi");
        let snap = snapshot_skill_dir(d.path()).unwrap();
        assert!(snap.has_scripts);
        assert!(snap.manifest_json().contains("helpers/fetch.sh"));
    }

    #[test]
    fn missing_skill_md_is_rejected() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "README.md", "x");
        assert!(snapshot_skill_dir(d.path()).is_err());
    }

    #[test]
    fn versions_are_append_only_copies() {
        let src = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        write(src.path(), "SKILL.md", "# v1");
        write(src.path(), "a/b.txt", "b");

        let v1 = store_version(root.path(), "skl_1", 1, src.path()).unwrap();
        assert_eq!(v1, root.path().join(".versions/skl_1/1"));
        assert_eq!(std::fs::read_to_string(v1.join("a/b.txt")).unwrap(), "b");
        assert!(
            !std::fs::symlink_metadata(v1.join("SKILL.md"))
                .unwrap()
                .file_type()
                .is_symlink()
        );

        write(src.path(), "SKILL.md", "# v2");
        let again = store_version(root.path(), "skl_1", 1, src.path()).unwrap_err();
        assert_eq!(again.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read_to_string(v1.join("SKILL.md")).unwrap(), "# v1");

        store_version(root.path(), "skl_1", 2, src.path()).unwrap();
        assert!(store_version(root.path(), "../evil", 1, src.path()).is_err());
        assert!(store_version(root.path(), "skl_1", 0, src.path()).is_err());
    }

    #[test]
    fn diff_lists_changes_and_skips_binary_bodies() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write(a.path(), "SKILL.md", "line1\nline2\n");
        write(b.path(), "SKILL.md", "line1\nline2 changed\n");
        write(a.path(), "same.md", "x");
        write(b.path(), "same.md", "x");
        write(a.path(), "gone.md", "bye\n");
        write(b.path(), "scripts/run.sh", "echo hi\n");
        std::fs::write(a.path().join("img.bin"), [0u8, 1, 2]).unwrap();
        std::fs::write(b.path().join("img.bin"), [0u8, 1, 3]).unwrap();

        let d = diff_version_dirs(a.path(), b.path()).unwrap();
        let paths: Vec<_> = d.iter().map(|f| (f.path.as_str(), f.status.as_str())).collect();
        assert_eq!(
            paths,
            vec![
                ("SKILL.md", "modified"),
                ("gone.md", "removed"),
                ("img.bin", "modified"),
                ("scripts/run.sh", "added"),
            ]
        );
        let skill = &d[0];
        let text = skill.diff.as_deref().unwrap();
        assert!(
            text.contains("--- a/SKILL.md") && text.contains("+++ b/SKILL.md"),
            "{text}"
        );
        assert!(text.contains("-line2\n") && text.contains("+line2 changed\n"), "{text}");
        assert!(d[2].binary && d[2].diff.is_none());
        assert!(d[3].script && d[3].diff.as_deref().unwrap().contains("+echo hi"));
        assert!(diff_version_dirs(a.path(), a.path()).unwrap().is_empty());
    }

    #[test]
    fn restore_swaps_live_dir_and_keeps_the_version() {
        let root = tempfile::tempdir().unwrap();
        let live = root.path().join("demo");
        write(&live, "SKILL.md", "# v1");
        write(&live, "old.md", "o");
        store_version(root.path(), "skl_r", 1, &live).unwrap();
        std::fs::remove_file(live.join("old.md")).unwrap();
        write(&live, "SKILL.md", "# v2");
        write(&live, "new.md", "n");
        write(&live, ".git/HEAD", "ref");

        restore_version_into(root.path(), "skl_r", 1, &live).unwrap();
        assert_eq!(std::fs::read_to_string(live.join("SKILL.md")).unwrap(), "# v1");
        assert!(live.join("old.md").is_file());
        assert!(!live.join("new.md").exists());
        assert!(live.join(".git/HEAD").is_file(), ".git is carried over");
        let v1 = version_dir(root.path(), "skl_r", 1).unwrap();
        assert_eq!(std::fs::read_to_string(v1.join("SKILL.md")).unwrap(), "# v1");
        let leftovers: Vec<_> = std::fs::read_dir(v1.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with('.'))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert_eq!(
            restore_version_into(root.path(), "skl_r", 9, &live).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }
}
