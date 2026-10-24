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
    let target = version_dir(skills_root, skill_id, version_no)?;
    if target.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("skill version {skill_id}/{version_no} already stored"),
        ));
    }
    let parent = target.parent().expect("version dir has a parent");
    std::fs::create_dir_all(parent)?;
    let staging = parent.join(format!(".{version_no}.staging-{}", std::process::id()));
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    let mut paths = Vec::new();
    collect_files(src_dir, src_dir, &mut paths)?;
    for rel in &paths {
        let dst = staging.join(rel);
        if let Some(p) = dst.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::copy(src_dir.join(rel), &dst)?;
    }
    std::fs::create_dir_all(&staging)?;
    std::fs::rename(&staging, &target)?;
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
}
