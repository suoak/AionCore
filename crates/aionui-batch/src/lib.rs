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

pub mod checks;
pub mod error;
pub mod template;
pub mod validate;

pub use checks::{CompiledChecks, all_passed};
pub use error::BatchError;
pub use template::PromptTemplate;
pub use validate::{TargetLocation, ValidatedBatchJob, target_location, validate_create};
