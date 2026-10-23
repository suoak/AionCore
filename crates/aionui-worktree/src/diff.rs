//! Read-only diff of a managed worktree against its `base_commit` (git2).
//!
//! Per design §5 reads stay on git2: libgit2 never runs external diff,
//! textconv or filter drivers, so a checked-out branch cannot make a diff
//! request execute repository-controlled commands.

use std::path::{Component, Path};

use git2::{Delta, DiffFindOptions, DiffOptions, Oid, Patch, Repository};

use crate::error::WorktreeError;
use crate::types::DiffStatFile;

/// `diff-stat` lists at most this many files (totals still count all).
pub const MAX_DIFF_STAT_FILES: usize = 2000;
/// A single-file patch is cut at this size.
pub const MAX_PATCH_BYTES: usize = 1024 * 1024;

pub(crate) struct DiffStat {
    pub files: Vec<DiffStatFile>,
    pub files_changed: u32,
    pub additions: u64,
    pub deletions: u64,
    pub truncated: bool,
}

pub(crate) struct FileDiff {
    pub status: String,
    pub binary: bool,
    pub patch: String,
    pub truncated: bool,
}

fn git_err(context: &str, err: git2::Error) -> WorktreeError {
    WorktreeError::Git(format!("{context}: {}", err.message()))
}

fn status_name(delta: Delta) -> &'static str {
    match delta {
        Delta::Added => "added",
        Delta::Deleted => "deleted",
        Delta::Renamed => "renamed",
        Delta::Copied => "added",
        Delta::Typechange => "typechange",
        Delta::Untracked => "untracked",
        _ => "modified",
    }
}

fn path_text(path: Option<&Path>) -> Option<String> {
    path.map(|p| p.to_string_lossy().replace('\\', "/"))
}

/// Reject anything that is not a plain path inside the worktree.
pub(crate) fn validate_relative_path(raw: &str) -> Result<String, WorktreeError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(WorktreeError::BadRequest("path is required".into()));
    }
    let normalized = trimmed.replace('\\', "/");
    let path = Path::new(&normalized);
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => {
                return Err(WorktreeError::PathEscape(normalized.clone()));
            }
        }
    }
    if parts.is_empty() {
        return Err(WorktreeError::BadRequest("path is required".into()));
    }
    if parts[0].eq_ignore_ascii_case(".git") {
        return Err(WorktreeError::PathEscape(normalized));
    }
    Ok(parts.join("/"))
}

fn base_options(pathspec: Option<&str>) -> DiffOptions {
    let mut opts = DiffOptions::new();
    opts.include_untracked(true)
        .recurse_untracked_dirs(true)
        .show_untracked_content(true)
        .include_typechange(true)
        .ignore_submodules(true);
    if let Some(spec) = pathspec {
        opts.pathspec(spec).disable_pathspec_match(true);
    }
    opts
}

fn open_diff<'r>(
    repo: &'r Repository,
    base_commit: &str,
    pathspec: Option<&str>,
) -> Result<git2::Diff<'r>, WorktreeError> {
    let oid = Oid::from_str(base_commit).map_err(|e| git_err("base commit", e))?;
    let tree = repo
        .find_commit(oid)
        .and_then(|c| c.tree())
        .map_err(|e| git_err("base commit", e))?;
    let mut opts = base_options(pathspec);
    let mut diff = repo
        .diff_tree_to_workdir_with_index(Some(&tree), Some(&mut opts))
        .map_err(|e| git_err("diff", e))?;
    if pathspec.is_none() {
        let mut find = DiffFindOptions::new();
        find.renames(true);
        diff.find_similar(Some(&mut find))
            .map_err(|e| git_err("rename detection", e))?;
    }
    Ok(diff)
}

pub(crate) fn diff_stat(worktree: &Path, base_commit: &str) -> Result<DiffStat, WorktreeError> {
    let repo = Repository::open(worktree).map_err(|e| git_err("open worktree", e))?;
    let diff = open_diff(&repo, base_commit, None)?;
    let mut out = DiffStat {
        files: Vec::new(),
        files_changed: 0,
        additions: 0,
        deletions: 0,
        truncated: false,
    };
    for idx in 0..diff.deltas().len() {
        let patch = Patch::from_diff(&diff, idx).map_err(|e| git_err("diff", e))?;
        let Some(patch) = patch else { continue };
        let delta = patch.delta();
        let binary = delta.flags().is_binary();
        let (additions, deletions) = if binary {
            (0, 0)
        } else {
            let (_, add, del) = patch.line_stats().map_err(|e| git_err("diff", e))?;
            (add, del)
        };
        let new_path = path_text(delta.new_file().path());
        let old_path = path_text(delta.old_file().path());
        let status = status_name(delta.status());
        out.files_changed += 1;
        out.additions += additions as u64;
        out.deletions += deletions as u64;
        if out.files.len() >= MAX_DIFF_STAT_FILES {
            out.truncated = true;
            continue;
        }
        let path = new_path.clone().or_else(|| old_path.clone()).unwrap_or_default();
        out.files.push(DiffStatFile {
            old_path: if status == "renamed" && old_path != new_path {
                old_path
            } else {
                None
            },
            path,
            status: status.to_owned(),
            additions: u32::try_from(additions).unwrap_or(u32::MAX),
            deletions: u32::try_from(deletions).unwrap_or(u32::MAX),
            binary,
        });
    }
    Ok(out)
}

pub(crate) fn file_diff(worktree: &Path, base_commit: &str, rel_path: &str) -> Result<FileDiff, WorktreeError> {
    let repo = Repository::open(worktree).map_err(|e| git_err("open worktree", e))?;
    let diff = open_diff(&repo, base_commit, Some(rel_path))?;
    for idx in 0..diff.deltas().len() {
        let Some(mut patch) = Patch::from_diff(&diff, idx).map_err(|e| git_err("diff", e))? else {
            continue;
        };
        let delta = patch.delta();
        let status = status_name(delta.status()).to_owned();
        if delta.flags().is_binary() {
            return Ok(FileDiff {
                status,
                binary: true,
                patch: String::new(),
                truncated: false,
            });
        }
        let buf = patch.to_buf().map_err(|e| git_err("diff", e))?;
        let bytes: &[u8] = &buf;
        let truncated = bytes.len() > MAX_PATCH_BYTES;
        let mut text = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_PATCH_BYTES)]).into_owned();
        if truncated && let Some(cut) = text.rfind('\n') {
            text.truncate(cut + 1);
        }
        return Ok(FileDiff {
            status,
            binary: false,
            patch: text,
            truncated,
        });
    }
    Ok(FileDiff {
        status: "unchanged".into(),
        binary: false,
        patch: String::new(),
        truncated: false,
    })
}

#[cfg(test)]
mod tests {
    use super::validate_relative_path;

    #[test]
    fn relative_path_validation() {
        assert_eq!(validate_relative_path("src/a.rs").unwrap(), "src/a.rs");
        assert_eq!(validate_relative_path("./src\\a.rs").unwrap(), "src/a.rs");
        for bad in [
            "",
            "  ",
            "../x",
            "a/../../x",
            "/etc/passwd",
            ".git/config",
            ".GIT/HEAD",
            "a\0b",
        ] {
            assert!(validate_relative_path(bad).is_err(), "{bad:?} must be rejected");
        }
    }
}
