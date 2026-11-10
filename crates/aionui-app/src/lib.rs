#![warn(clippy::disallowed_types)]

//! Application crate: assembles all domain crates into an Axum server with DI and middleware.
//!
//! This file is a public façade — it only re-exports symbols defined in
//! submodules. All logic lives in the modules below.

mod config;
/// Host composition: signed backgroundService binding → checked entry-point source (no service-start auth).
pub mod extension_js_background_service_source;
pub mod extension_js_checked_source;
/// Experimental internal fixed-executable JS client; not connected to extension routes.
pub mod extension_js_client;
/// Host composition: signed tool binding → checked handler source (no exec auth).
pub mod extension_js_handler_source;
/// Host invoke prep: checked load → non-executing ack or refuse (no fake route wiring).
pub mod extension_js_invoke_checked;
/// App adapter implementing extension CheckedAgentToolPrepare (crate-boundary bridge).
pub mod extension_js_invoke_prepare_adapter;
/// Host composition: signed panel binding → checked entry-point source (no panel-load auth).
pub mod extension_js_panel_source;
pub mod extension_js_source;
/// Host composition: signed workPanelView binding → checked entry-point source (no view-load auth).
pub mod extension_js_work_panel_view_source;
mod github_issue_source;
mod router;
mod services;
mod skill_evolution_adapters;
mod squad_board_relay;

pub use config::{AppConfig, ENCRYPTION_STDIN_ENV, EncryptionRoot, IdentityMode, derive_encryption_key};
pub use router::{
    ChannelOrchestratorComponents, ModuleStates, RouterBuildError, RouterRuntime, build_assistant_state,
    build_conversation_state, build_extension_states, build_module_states, build_ws_state, create_router,
    create_router_with_all_state, create_router_with_runtime, create_router_with_states,
};
pub use services::AppServices;
