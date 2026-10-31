use aionui_db::DbError;

/// Crate-owned batch error; mapped to `ApiError` only in routes.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("{0}")]
    BadRequest(String),
    #[error("batch not found: {0}")]
    NotFound(String),
    /// The job / item is not in a state that allows the operation
    /// (e.g. scale before the trial finished).
    #[error("{0}")]
    InvalidState(String),
    /// D8 disk precheck: not enough free space for `concurrency` worktrees.
    #[error("{0}")]
    InsufficientDisk(String),
    /// A runtime dependency (assistant, worktree, conversation) is unavailable.
    #[error("{0}")]
    Unavailable(String),
    #[error("database error: {0}")]
    Database(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl BatchError {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadRequest(_) => "BAD_REQUEST",
            Self::NotFound(_) => "BATCH_NOT_FOUND",
            Self::InvalidState(_) => "BATCH_INVALID_STATE",
            Self::InsufficientDisk(_) => "BATCH_INSUFFICIENT_DISK",
            Self::Unavailable(_) => "BATCH_UNAVAILABLE",
            Self::Database(_) => "DATABASE_ERROR",
            Self::Internal(_) => "INTERNAL_ERROR",
        }
    }

    pub(crate) fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
}

impl From<DbError> for BatchError {
    fn from(err: DbError) -> Self {
        Self::Database(err.to_string())
    }
}
