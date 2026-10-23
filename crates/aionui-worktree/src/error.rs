use crate::git_cli::GitCliError;

/// Crate-owned errors. Mapped to HTTP only in `routes.rs`.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WorktreeError {
    #[error("git is not available: {0}")]
    GitNotAvailable(String),
    #[error("git {found} is too old; {required} or newer is required")]
    GitTooOld { found: String, required: String },
    #[error("not a git repository: {0}")]
    NotARepository(String),
    #[error("ref not found: {0}")]
    RefNotFound(String),
    #[error("invalid ref or branch name: {0}")]
    InvalidRef(String),
    #[error("branch already exists: {0}")]
    BranchExists(String),
    #[error("path is too long for this platform: {0}")]
    PathTooLong(String),
    #[error("worktree has uncommitted changes: {0}")]
    Dirty(String),
    #[error("worktree not found: {0}")]
    NotFound(String),
    #[error("worktree is {status}; cannot {action}")]
    InvalidState { status: String, action: &'static str },
    #[error("worktree path is outside the managed root: {0}")]
    PathEscape(String),
    #[error("worktree path already in use: {0}")]
    PathConflict(String),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error("git command failed: {0}")]
    Git(String),
    #[error("git command timed out: {0}")]
    Timeout(String),
    #[error("cancelled")]
    Cancelled,
    #[error("filesystem error: {0}")]
    Io(String),
    #[error("database error: {0}")]
    Database(String),
    // ---- E3-4 fan-out ----
    #[error("fan-out limit: {0}")]
    FanoutLimit(String),
    #[error("fan-out group not found: {0}")]
    FanoutNotFound(String),
    #[error("fan-out group is {status}; cannot {action}")]
    FanoutInvalidState { status: String, action: &'static str },
    #[error("fan-out is not available: {0}")]
    FanoutUnavailable(String),
    #[error("slot {0} is still running; stop it before adopting")]
    SlotBusy(String),
    #[error("adopt conflict: {0}")]
    AdoptConflict(String),
    #[error("target branch is not clean: {0}")]
    TargetNotClean(String),
}

impl WorktreeError {
    /// Stable machine-readable code (also persisted in `error_code`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::GitNotAvailable(_) => "GIT_NOT_AVAILABLE",
            Self::GitTooOld { .. } => "GIT_TOO_OLD",
            Self::NotARepository(_) => "NOT_A_REPOSITORY",
            Self::RefNotFound(_) => "REF_NOT_FOUND",
            Self::InvalidRef(_) => "INVALID_REF",
            Self::BranchExists(_) => "BRANCH_EXISTS",
            Self::PathTooLong(_) => "PATH_TOO_LONG",
            Self::Dirty(_) => "WORKTREE_DIRTY",
            Self::NotFound(_) => "WORKTREE_NOT_FOUND",
            Self::InvalidState { .. } => "WORKTREE_INVALID_STATE",
            Self::PathEscape(_) => "WORKTREE_PATH_ESCAPE",
            Self::PathConflict(_) => "WORKTREE_PATH_CONFLICT",
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::Git(_) => "GIT_FAILED",
            Self::Timeout(_) => "GIT_TIMEOUT",
            Self::Cancelled => "CANCELLED",
            Self::Io(_) => "IO_ERROR",
            Self::Database(_) => "DATABASE_ERROR",
            Self::FanoutLimit(_) => "FANOUT_LIMIT",
            Self::FanoutNotFound(_) => "FANOUT_NOT_FOUND",
            Self::FanoutInvalidState { .. } => "FANOUT_INVALID_STATE",
            Self::FanoutUnavailable(_) => "FANOUT_UNAVAILABLE",
            Self::SlotBusy(_) => "SLOT_BUSY",
            Self::AdoptConflict(_) => "ADOPT_CONFLICT",
            Self::TargetNotClean(_) => "TARGET_NOT_CLEAN",
        }
    }
}

impl From<GitCliError> for WorktreeError {
    fn from(err: GitCliError) -> Self {
        match err {
            GitCliError::NotAvailable(msg) => Self::GitNotAvailable(msg),
            GitCliError::TooOld { found, required } => Self::GitTooOld {
                found: found.to_string(),
                required: required.to_string(),
            },
            GitCliError::UnknownVersion(msg) => Self::GitNotAvailable(msg),
            GitCliError::Timeout { .. } => Self::Timeout(err.to_string()),
            GitCliError::Cancelled { .. } => Self::Cancelled,
            GitCliError::InvalidRef(r) => Self::InvalidRef(r),
            GitCliError::Failed { .. } => Self::Git(err.to_string()),
            GitCliError::Io(msg) => Self::Io(msg),
        }
    }
}
