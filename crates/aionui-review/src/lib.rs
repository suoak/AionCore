//! CSBU WorkMate diff annotations ("review threads").
//!
//! - [`anchor`]: pure line anchoring and re-anchoring (design §5.2);
//! - [`ReviewService`]: CRUD over `review_threads` / `review_comments`, with
//!   lazy re-anchoring against the current file on every list;
//! - [`review_routes`]: `/api/review-threads*`, user identity only. The prefix
//!   is outside the agent-token allow-list, so agent tokens get 403
//!   (`aionui-auth::agent_scope`).
//!
//! - [`ReviewService::send_batch`]: `/api/review-batches`, one `[[AION_REVIEW]]`
//!   input per batch through the conversation input queue ([`prompt`]).
//!
//! - [`DeliveryService`]: `/api/scm-deliveries*`, PR creation through the
//!   user's logged-in `gh` / `glab`, recorded in `scm_deliveries` and as
//!   `task_evidence(kind='artifact')` (E4-5).

pub mod anchor;
mod batch;
pub mod delivery;
pub mod delivery_cli;
mod error;
pub mod prompt;
mod resolver;
pub mod routes;
mod service;
mod types;

pub use batch::{
    ListReviewBatchesQuery, MAX_BATCH_THREADS, ReviewBatch, ReviewDeliveryMode, ReviewInputAccepted, ReviewInputError,
    ReviewInputSink, SendReviewBatchRequest, SendReviewBatchResponse,
};
pub use delivery::{
    CreatePullRequestRequest, CreatePullRequestResponse, DeliveryClis, DeliveryService, DetectHostQuery, HostStatus,
    ListScmDeliveriesQuery, ScmDelivery,
};
pub use delivery_cli::{HostCli, HostProvider};
pub use error::ReviewError;
pub use prompt::{REVIEW_BLOCK_MAX_BYTES, render_review_block};
pub use resolver::{
    AllowedRootsResolver, PE_SCOPE_PREFIX, ProjectDirResolver, ScopeContentResolver, parse_pe_scope, pe_scope_id,
};
pub use routes::{DeliveryRouterState, ReviewRouterState, delivery_routes, review_routes};
pub use service::ReviewService;
pub use types::{
    AddReviewCommentRequest, CreateReviewThreadRequest, DiffContext, ListReviewThreadsQuery, ReviewComment,
    ReviewCommentAuthor, ReviewScopeKind, ReviewSide, ReviewThread, ReviewThreadStatus, UpdateReviewThreadRequest,
};
