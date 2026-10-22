//! CSBU WorkMate diff annotations ("review threads").
//!
//! - [`anchor`]: pure line anchoring and re-anchoring (design §5.2);
//! - [`ReviewService`]: CRUD over `review_threads` / `review_comments`, with
//!   lazy re-anchoring against the current file on every list;
//! - [`review_routes`]: `/api/review-threads*`, user identity only. The prefix
//!   is outside the agent-token allow-list, so agent tokens get 403
//!   (`aionui-auth::agent_scope`).
//!
//! Sending annotations to an agent (`review_batches`) is E4-3; commit / push /
//! PR delivery (`scm_deliveries`) is E4-4/E4-5.

pub mod anchor;
mod error;
mod resolver;
pub mod routes;
mod service;
mod types;

pub use error::ReviewError;
pub use resolver::{
    AllowedRootsResolver, PE_SCOPE_PREFIX, ProjectDirResolver, ScopeContentResolver, parse_pe_scope, pe_scope_id,
};
pub use routes::{ReviewRouterState, review_routes};
pub use service::ReviewService;
pub use types::{
    AddReviewCommentRequest, CreateReviewThreadRequest, DiffContext, ListReviewThreadsQuery, ReviewComment,
    ReviewCommentAuthor, ReviewScopeKind, ReviewSide, ReviewThread, ReviewThreadStatus, UpdateReviewThreadRequest,
};
