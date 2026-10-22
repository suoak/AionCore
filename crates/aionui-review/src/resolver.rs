//! Where a scope's "new side" lives on disk, for server-side anchoring.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;

use crate::types::ReviewScopeKind;

/// Prefix of an `scm` scope id that names a repository by its project-explorer
/// root (`pe:{pe_id}` or `pe:{pe_id}/{relative_path}`) instead of an absolute
/// path. The renderer only ever sees pe-relative identities, so this is the
/// form the UI sends.
pub const PE_SCOPE_PREFIX: &str = "pe:";

/// Resolves a review scope to the directory whose files are the diff's new
/// side (the working tree). `None` = the side is not readable on the server;
/// anchors are then built from the client's diff context and never
/// re-anchored lazily (e.g. immutable TaskRun diffs).
#[async_trait]
pub trait ScopeContentResolver: Send + Sync {
    /// Canonical form of a scope id to store and query by. `None` = the user
    /// may not use this scope.
    async fn canonical_scope_id(&self, user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<String>;
    /// Root directory of the scope's working tree.
    async fn scope_root(&self, user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<PathBuf>;
}

/// Resolves a project-explorer reference owned by `user_id` to an absolute
/// directory (implemented in `aionui-app` over `ProjectService`).
#[async_trait]
pub trait ProjectDirResolver: Send + Sync {
    async fn resolve_dir(&self, user_id: &str, pe_id: &str, relative_path: &str) -> Option<PathBuf>;
}

/// Split and normalize a `pe:` scope id into `(pe_id, relative_path)`.
pub fn parse_pe_scope(scope_id: &str) -> Option<(String, String)> {
    let rest = scope_id.strip_prefix(PE_SCOPE_PREFIX)?;
    let (pe_id, relative) = rest.split_once('/').unwrap_or((rest, ""));
    if pe_id.is_empty() || pe_id.contains(['\\', '\0']) {
        return None;
    }
    let relative = relative.replace('\\', "/");
    let segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    if segments.iter().any(|s| *s == "." || *s == ".." || s.contains('\0')) {
        return None;
    }
    if relative.as_bytes().get(1) == Some(&b':') {
        return None;
    }
    Some((pe_id.to_owned(), segments.join("/")))
}

/// Canonical `pe:` scope id for a pe root ref.
pub fn pe_scope_id(pe_id: &str, relative_path: &str) -> String {
    if relative_path.is_empty() {
        format!("{PE_SCOPE_PREFIX}{pe_id}")
    } else {
        format!("{PE_SCOPE_PREFIX}{pe_id}/{relative_path}")
    }
}

/// Default resolver.
///
/// An `scm` scope id is either
/// - `pe:{pe_id}[/{relative_path}]`, resolved through the user's own project
///   entries (ownership enforced by the project service), or
/// - an absolute repository directory under one of the allowed roots (the
///   same sandbox the file routes use).
///
/// `worktree` scopes resolve once E3 wires `task_worktrees`; `task_run` scopes
/// are immutable retained diffs.
#[derive(Clone)]
pub struct AllowedRootsResolver {
    allowed_roots: Vec<PathBuf>,
    projects: Option<Arc<dyn ProjectDirResolver>>,
}

impl AllowedRootsResolver {
    pub fn new(allowed_roots: Vec<PathBuf>) -> Self {
        let allowed_roots = allowed_roots
            .into_iter()
            .filter_map(|root| std::fs::canonicalize(root).ok())
            .collect();
        Self {
            allowed_roots,
            projects: None,
        }
    }

    /// Also accept `pe:` scope ids, resolved through `projects`.
    pub fn with_projects(mut self, projects: Arc<dyn ProjectDirResolver>) -> Self {
        self.projects = Some(projects);
        self
    }

    fn sandboxed_dir(&self, scope_id: &str) -> Option<PathBuf> {
        let path = Path::new(scope_id);
        if !path.is_absolute() {
            return None;
        }
        let canonical = std::fs::canonicalize(path).ok()?;
        (canonical.is_dir() && self.allowed_roots.iter().any(|root| canonical.starts_with(root))).then_some(canonical)
    }

    async fn pe_dir(&self, user_id: &str, scope_id: &str) -> Option<PathBuf> {
        let (pe_id, relative) = parse_pe_scope(scope_id)?;
        let dir = self.projects.as_ref()?.resolve_dir(user_id, &pe_id, &relative).await?;
        let canonical = tokio::fs::canonicalize(dir).await.ok()?;
        canonical.is_dir().then_some(canonical)
    }
}

#[async_trait]
impl ScopeContentResolver for AllowedRootsResolver {
    async fn canonical_scope_id(&self, user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<String> {
        match kind {
            ReviewScopeKind::Scm if scope_id.starts_with(PE_SCOPE_PREFIX) => {
                // The pe must exist and belong to the user; the stored id stays
                // in pe form so it is stable across machines and path casing.
                self.pe_dir(user_id, scope_id).await?;
                let (pe_id, relative) = parse_pe_scope(scope_id)?;
                Some(pe_scope_id(&pe_id, &relative))
            }
            ReviewScopeKind::Scm => self
                .sandboxed_dir(scope_id)
                .map(|path| path.to_string_lossy().into_owned()),
            ReviewScopeKind::Worktree | ReviewScopeKind::TaskRun => Some(scope_id.to_owned()),
        }
    }

    async fn scope_root(&self, user_id: &str, kind: ReviewScopeKind, scope_id: &str) -> Option<PathBuf> {
        match kind {
            ReviewScopeKind::Scm if scope_id.starts_with(PE_SCOPE_PREFIX) => self.pe_dir(user_id, scope_id).await,
            ReviewScopeKind::Scm => self.sandboxed_dir(scope_id),
            ReviewScopeKind::Worktree | ReviewScopeKind::TaskRun => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pe_scope_parsing_normalizes_and_rejects_traversal() {
        assert_eq!(parse_pe_scope("pe:abc"), Some(("abc".into(), String::new())));
        assert_eq!(parse_pe_scope("pe:abc/"), Some(("abc".into(), String::new())));
        assert_eq!(
            parse_pe_scope("pe:abc/sub/repo"),
            Some(("abc".into(), "sub/repo".into()))
        );
        assert_eq!(
            parse_pe_scope("pe:abc/sub\\repo/"),
            Some(("abc".into(), "sub/repo".into()))
        );
        assert_eq!(parse_pe_scope("pe:abc/../x"), None);
        assert_eq!(parse_pe_scope("pe:/x"), None);
        assert_eq!(parse_pe_scope("pe:"), None);
        assert_eq!(parse_pe_scope("/abs"), None);
        assert_eq!(pe_scope_id("abc", ""), "pe:abc");
        assert_eq!(pe_scope_id("abc", "sub/repo"), "pe:abc/sub/repo");
    }
}
