#![warn(clippy::disallowed_types)]

//! M8 E8: batch jobs.
//!
//! A batch job runs one prompt template once per target (D8-3: agent mode
//! only), with bounded concurrency (D8-4: default 4, cap 16, 30 min per
//! item), a mandatory trial (D8-2: the first `trial_size` items, default 1)
//! before the rest may be scaled out, and a pass / fail verdict taken only
//! from artifacts the item produced — never from the agent's own claims.
//!
//! - [`checks`]: `file_exists` / `glob` / `json_schema` artifact checks,
//!   confined to the item workspace.
//! - [`template`]: `{{target}}` / `{{target.<key>}}` / `{{seq}}` prompts.
//! - [`validate`]: create-request validation, defaults and caps.
//! - [`runner`]: per-job drivers — scheduling, concurrency, timeouts.
//! - [`service`]: the API surface (trial gate, scale, retry, cancel, export).
//! - [`port`]: what the runner needs from the app, implemented in `aionui-app`.

pub mod checks;
pub mod error;
pub mod port;
pub mod routes;
pub mod runner;
pub mod service;
pub mod template;
pub mod validate;

pub use checks::{CompiledChecks, all_passed};
pub use error::BatchError;
pub use port::{AttemptOutcome, BatchEvent, IBatchExecutor, ItemSpec, PreparedItem};
pub use routes::{BatchRouterState, batch_routes};
pub use runner::{BatchRunner, BatchRunnerConfig};
pub use service::{BatchService, INTERRUPTED_ERROR, item_response, job_response};
pub use template::PromptTemplate;
pub use validate::{TargetLocation, ValidatedBatchJob, target_location, validate_create};
