//! Where a scope's "new side" lives on disk, for server-side anchoring.

use std::path::{Path, PathBuf};

use crate::types::ReviewScopeKind;

/// Resolves a review scope to the directory whose files are the diff's new
/// side (the working tree). `None` = the side is not readable on the server;
/// anchors are then built from the client's diff context and never
/// re-anchored lazily (e.g. immutable TaskRun diffs).
pub trait ScopeContentResolver: Send + Sync {
    /// Canonical form of a scope id to store and query by.
    fn canonical_scope_id(&self, kind: ReviewScopeKind, scope_id: &str) -> Option<String>;
    /// Root directory of the scope's working tree.
    fn scope_root(&self, user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<PathBuf>;
}

/// Default resolver: an `scm` scope id is a repository directory that must
/// sit under one of the allowed roots (the same sandbox the file routes use).
/// `worktree` scopes resolve once E3 wires `task_worktrees`; `task_run` scopes
/// are immutable retained diffs.
#[derive(Debug, Clone)]
pub struct AllowedRootsResolver {
    allowed_roots: Vec<PathBuf>,
}

impl AllowedRootsResolver {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        let allowed_roots = allowed_roots
            .into_iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        Self { allowed_roots }
    }

    fn sandboxed_dir(&self, scope_id: &str) -> Option<PathBuf> {
        let path = Path::new(scope_id);
        if !path.is_absolute() {
            return None;
        }
        let canonical = std::fs::canonicalize(path).ok()?;
        (canonical.is_dir() && self.allowed_roots.iter().any(|root| canonical.starts_with(root))).then_some(canonical)
    }
}

impl ScopeContentResolver for AllowedRootsResolver {
    fn canonical_scope_id(&self, kind: ReviewScopeKind, scope_id: &str) -> Option<String> {
        match kind {
            ReviewScopeKind::Scm => self
                .sandboxed_dir(scope_id)
                .map(|path| path.to_string_lossy().into_owned()),
            ReviewScopeKind::Worktree | ReviewScopeKind::TaskRun => Some(scope_id.to_owned()),
        }
    }

    fn scope_root(&self, _user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<PathBuf> {
        match kind {
            ReviewScopeKind::Scm => self.sandboxed_dir(scope_id),
            ReviewScopeKind::Worktree | ReviewScopeKind::TaskRun => None,
        }
    }
}
