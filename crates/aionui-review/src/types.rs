//! Wire and domain types for review threads.

use serde::{Deserialize, Serialize};

use crate::anchor::Anchor;
use crate::error::ReviewError;

macro_rules! string_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(rename_all = "snake_case")]
        pub enum $name { $($variant),+ }

        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $text),+ }
            }

            pub fn parse(value: &str) -> Result<Self, ReviewError> {
                match value {
                    $($text => Ok(Self::$variant),)+
                    other => Err(ReviewError::BadRequest(format!(
                        concat!("invalid ", stringify!($name), " '{}'"),
                        other
                    ))),
                }
            }
        }
    };
}

string_enum!(
    /// What a thread's `scope_id` identifies.
    ReviewScopeKind { Scm => "scm", Worktree => "worktree", TaskRun => "task_run" }
);
string_enum!(
    /// Which side of the diff the lines refer to.
    ReviewSide { New => "new", Old => "old" }
);
string_enum!(
    ReviewThreadStatus { Open => "open", Sent => "sent", Resolved => "resolved", Outdated => "outdated" }
);
string_enum!(
    ReviewCommentAuthor { User => "user", Agent => "agent" }
);

/// The diff lines the user selected, as the UI rendered them. The server
/// re-derives the anchor from the file when it can read it; this is the
/// fallback and the staleness check, never trusted for `content_sha`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DiffContext {
    #[serde(default)]
    pub hunk_header: Option<String>,
    /// `WORKTREE`, `INDEX` or a commit. Defaults to `WORKTREE` for the new
    /// side of an SCM / worktree diff.
    #[serde(default)]
    pub base: Option<String>,
    #[serde(default)]
    pub before: Vec<String>,
    #[serde(default)]
    pub lines: Vec<String>,
    #[serde(default)]
    pub after: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreateReviewThreadRequest {
    pub scope_kind: String,
    pub scope_id: String,
    #[serde(default)]
    pub conversation_id: Option<String>,
    #[serde(default)]
    pub task_session_id: Option<String>,
    #[serde(default)]
    pub run_id: Option<String>,
    pub file_path: String,
    pub side: String,
    pub start_line: u32,
    pub end_line: u32,
    pub body: String,
    #[serde(default)]
    pub diff_context: DiffContext,
}

/// `PATCH /api/review-threads/{id}`: edit the first comment and/or move the
/// selection. A new range needs a fresh `diff_context` when the server cannot
/// read the side.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateReviewThreadRequest {
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub start_line: Option<u32>,
    #[serde(default)]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub diff_context: Option<DiffContext>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddReviewCommentRequest {
    pub body: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ListReviewThreadsQuery {
    pub scope_kind: Option<String>,
    pub scope_id: Option<String>,
    pub file_path: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReviewComment {
    pub id: String,
    pub thread_id: String,
    pub author: ReviewCommentAuthor,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReviewThread {
    pub id: String,
    pub scope_kind: ReviewScopeKind,
    pub scope_id: String,
    pub conversation_id: Option<String>,
    pub task_session_id: Option<String>,
    pub run_id: Option<String>,
    pub file_path: String,
    pub side: ReviewSide,
    pub start_line: u32,
    pub end_line: u32,
    pub status: ReviewThreadStatus,
    /// The selected lines were rewritten; the thread follows the region that
    /// replaced them (design §5.2 step 4).
    pub moved_fuzzy: bool,
    pub anchor: Anchor,
    pub last_batch_id: Option<String>,
    pub comments: Vec<ReviewComment>,
    pub created_at: i64,
    pub updated_at: i64,
    pub resolved_at: Option<i64>,
}
