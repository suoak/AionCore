use aionui_common::ApiError;
use axum::http::StatusCode;

/// File crate application errors.
#[derive(Debug, thiserror::Error)]
pub enum FileError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{message}")]
    PathOutsideSandbox {
        message: String,
        field: Option<&'static str>,
        operation: Option<&'static str>,
    },

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Internal(String),

    /// Revealing an item in the OS file manager failed (the shell reveal command
    /// errored). Distinct from `NotFound` (missing path) so the frontend can tell
    /// "couldn't open the file manager" from "the item is gone". Maps to the
    /// stable API code `REVEAL_FAILED`.
    ///
    /// The payload is the underlying cause, kept **for logs only**. Boundary
    /// mappers must not copy it into the response: it originates in the shell
    /// layer and can quote a subprocess's stderr or an absolute path.
    #[error("failed to reveal item: {0}")]
    RevealFailed(String),

    /// A backend-resolved target does not exist on disk.
    ///
    /// Deliberately payload-free, unlike [`Self::NotFound`]. It serves routes
    /// addressed by identity (`{pe_id, relative_path}` or `ChatFileRef`) rather
    /// than by client-supplied path, where the absolute path is resolved on the
    /// server and the client has never seen it. Having no field to carry a path
    /// means no boundary mapper can forward one, which is what went wrong before:
    /// the shell error's path was threaded through `NotFound(String)` all the way
    /// into the response body. Maps to the stable API code `FILE_NOT_FOUND`.
    #[error("target not found")]
    TargetNotFound,

    /// The file exists but is not valid UTF-8 or UTF-16 text.
    ///
    /// Preview and other utf8 reads used to collapse this into a generic
    /// `Internal("cannot read file")`, which the client could only show as
    /// "preview failed". Windows PowerShell's default `Out-File` encoding is
    /// UTF-16 LE with BOM — a JSON file written that way is text, just not
    /// UTF-8. Maps to the stable API code `INVALID_TEXT_ENCODING`.
    #[error("file is not valid UTF-8 or UTF-16 text")]
    InvalidTextEncoding,

    /// The file exists but is temporarily locked (Windows sharing/lock
    /// violation after retries). Distinct from `Internal` so the client can
    /// ask the user to retry instead of treating it as a permanent failure.
    /// Maps to the stable API code `FILE_BUSY`.
    #[error("file is in use")]
    Busy,
}

impl FileError {
    /// HTTP mapping shared by the file crate and office path-validation.
    ///
    /// New variants belong here so both boundaries stay in lockstep. Causes that
    /// may quote a path or OS error stay in logs; public messages are fixed.
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::BadRequest(message) => ApiError::BadRequest(message),
            Self::Forbidden(message) => ApiError::Forbidden(message),
            Self::PathOutsideSandbox {
                message,
                field,
                operation,
            } => ApiError::PathOutsideSandbox {
                message,
                field,
                operation,
            },
            Self::NotFound(message) => ApiError::NotFound(message),
            Self::Internal(message) => ApiError::Internal(message),
            Self::RevealFailed(_) => ApiError::coded(
                StatusCode::INTERNAL_SERVER_ERROR,
                "REVEAL_FAILED",
                "Could not open the system file manager.",
                None::<serde_json::Value>,
            ),
            Self::TargetNotFound => ApiError::coded(
                StatusCode::NOT_FOUND,
                "FILE_NOT_FOUND",
                "The requested file no longer exists.",
                None::<serde_json::Value>,
            ),
            Self::InvalidTextEncoding => ApiError::coded(
                StatusCode::UNPROCESSABLE_ENTITY,
                "INVALID_TEXT_ENCODING",
                "The file is not valid UTF-8 or UTF-16 text.",
                None::<serde_json::Value>,
            ),
            Self::Busy => ApiError::coded(
                StatusCode::CONFLICT,
                "FILE_BUSY",
                "The file is in use and could not be read. Retry.",
                None::<serde_json::Value>,
            ),
        }
    }
}

impl From<FileError> for ApiError {
    fn from(error: FileError) -> Self {
        error.into_api_error()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_text_encoding_maps_to_stable_code() {
        let api_err = ApiError::from(FileError::InvalidTextEncoding);
        assert_eq!(api_err.error_code(), "INVALID_TEXT_ENCODING");
        assert_eq!(api_err.status_code(), StatusCode::UNPROCESSABLE_ENTITY);
        assert!(!api_err.public_message().is_empty());
    }

    #[test]
    fn busy_maps_to_stable_code() {
        let api_err = ApiError::from(FileError::Busy);
        assert_eq!(api_err.error_code(), "FILE_BUSY");
        assert_eq!(api_err.status_code(), StatusCode::CONFLICT);
    }
}
