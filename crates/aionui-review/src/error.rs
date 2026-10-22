/// Errors of the review domain. Routes map them to `ApiError`; storage
/// detail never reaches the client.
#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("{0}")]
    BadRequest(String),
    #[error("review thread not found")]
    NotFound,
    /// A stable machine code plus a human message (e.g. `ANCHOR_STALE`).
    #[error("{message}")]
    Conflict { code: &'static str, message: String },
    #[error("internal error: {0}")]
    Internal(String),
}

impl From<sqlx::Error> for ReviewError {
    fn from(error: sqlx::Error) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<serde_json::Error> for ReviewError {
    fn from(error: serde_json::Error) -> Self {
        Self::Internal(error.to_string())
    }
}
