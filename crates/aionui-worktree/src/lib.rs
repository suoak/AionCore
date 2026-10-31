#![warn(clippy::disallowed_types)]

//! M7 E3: WorkMate-managed git worktrees.
//!
//! - [`git_cli`]: system `git` runner (version probe, scrubbed env, timeout /
//!   cancellation with process-tree kill, Windows options).
//! - [`service`]: worktree lifecycle and state machine.
//! - [`routes`]: `/api/worktrees*` (user identity only; agent tokens denied).
//! - [`events`]: `worktree.progress` / `worktree.changed`.
//! - [`diff`]: read-only diff-stat / per-file diff against `base_commit`
//!   (git2; never runs repository-configured drivers).
//!
//! Conversation binding (E3-2): a conversation created with `worktree_id`
//! holds the worktree (`conversation_id`); deleting the conversation marks
//! it `unbound` and keeps the directory.
//!
//! Work item sharing (M8 Q12 / D7-3): [`work_item`] gives every Squad
//! instance of one E6 work item the same worktree (`task_session_id` =
//! work item id); closing the item only marks it, never removes it.
//!
//! Fan-out (E3-4): [`fanout`] runs one prompt through 2..=5 assistants, each
//! in its own worktree slot, then compares and adopts one
//! (`/api/fanout-groups*`, [`fanout_routes`]).

pub mod diff;
pub mod error;
pub mod events;
pub mod fanout;
pub mod fanout_port;
pub mod fanout_routes;
pub mod fanout_store;
pub mod fanout_types;
pub mod git_cli;
pub mod routes;
pub mod service;
pub mod store;
pub mod types;
pub mod work_item;

pub use error::WorktreeError;
pub use events::WorktreeEvents;
pub use fanout::FanoutService;
pub use fanout_port::{FanoutPrompt, IFanoutConversations, SlotConversationSpec, SlotRuntime};
pub use fanout_routes::{FanoutRouterState, fanout_routes};
pub use fanout_store::FanoutStore;
pub use git_cli::{GitCli, GitCliError, GitVersion, MIN_GIT_VERSION};
pub use routes::{WorktreeRouterState, worktree_routes};
pub use service::{WorktreeService, WorktreeServiceConfig};
pub use store::WorktreeStore;
pub use work_item::{WorkItemWorktree, WorkItemWorktreeSource};
