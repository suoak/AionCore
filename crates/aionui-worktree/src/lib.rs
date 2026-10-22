#![warn(clippy::disallowed_types)]

//! M7 E3: WorkMate-managed git worktrees.
//!
//! - [`git_cli`]: system `git` runner (version probe, scrubbed env, timeout /
//!   cancellation with process-tree kill, Windows options).
//! - [`service`]: worktree lifecycle and state machine.
//! - [`routes`]: `/api/worktrees*` (user identity only; agent tokens denied).
//! - [`events`]: `worktree.progress` / `worktree.changed`.
//!
//! Read-only SCM (status/diff via git2) stays in `aionui-project`; this crate
//! never changes the `scm/*` protocol.

pub mod error;
pub mod events;
pub mod git_cli;
pub mod routes;
pub mod service;
pub mod store;
pub mod types;

pub use error::WorktreeError;
pub use events::WorktreeEvents;
pub use git_cli::{GitCli, GitCliError, GitVersion, MIN_GIT_VERSION};
pub use routes::{WorktreeRouterState, worktree_routes};
pub use service::{WorktreeService, WorktreeServiceConfig};
pub use store::WorktreeStore;
