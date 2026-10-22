use crate::shared_kernel::PersistedSessionState;
use agent_client_protocol::schema::v1::{EnvVariable, McpServer, McpServerStdio, NewSessionRequest};
use aionui_api_types::AgentMetadata;
use aionui_api_types::{AcpBuildExtra, TEAM_MCP_SERVER_NAME, TeamMcpStdioConfig};
use aionui_common::CommandSpec;
use std::path::PathBuf;

/// Pre-computed workspace information.
#[derive(Debug, Clone)]
pub struct WorkspaceInfo {
    pub path: String,
    pub is_custom: bool,
}

/// All pre-computed parameters needed to create and drive an ACP session.
///
/// Assembled once by `assemble_acp_params` in the factory layer; the
/// `AcpAgentManager` reads from this but never mutates it. By front-loading
/// the decision logic (which MCP servers to inject, what preset context to
/// compose) we keep the manager focused on execution + state.
#[derive(Debug, Clone)]
pub struct AcpSessionParams {
    pub conversation_id: String,
    pub user_id: String,
    pub workspace: WorkspaceInfo,
    pub metadata: AgentMetadata,
    pub command_spec: CommandSpec,
    pub config: AcpBuildExtra,
    pub mcp_servers: Vec<McpServer>,
    pub preset_context: Option<String>,
    pub session_snapshot: Option<PersistedSessionState>,
    /// Backend data directory (`AppConfig.data_dir`) used for process
    /// registration and optional prompt diagnostics.
    pub data_dir: PathBuf,
    /// Whether prompt diagnostics should be dumped under `data_dir/prompt-dumps`.
    pub dump_prompts: bool,
}

impl AcpSessionParams {
    /// Build a `NewSessionRequest` using the pre-computed MCP servers.
    pub fn new_session_request(&self) -> NewSessionRequest {
        let req = NewSessionRequest::new(&self.workspace.path);
        if self.mcp_servers.is_empty() {
            req
        } else {
            req.mcp_servers(self.mcp_servers.clone())
        }
    }
}

/// Assemble fully-resolved ACP session params from factory inputs.
///
/// This front-loads all decision logic that was previously scattered across
/// `build_new_session_request`, preset context normalization,
/// and the factory's ACP match arm.
///
/// `user_mcp_servers` are operator-configured MCP servers loaded from the DB
/// by the factory layer; they are appended after the team injection so
/// the agent gets *all* the user's tools on `session/new` (ELECTRON-1JG fix).
#[allow(clippy::too_many_arguments)]
pub async fn assemble_acp_params(
    conversation_id: String,
    user_id: String,
    workspace: WorkspaceInfo,
    metadata: AgentMetadata,
    mut command_spec: CommandSpec,
    config: AcpBuildExtra,
    user_mcp_servers: Vec<McpServer>,
    session_snapshot: Option<PersistedSessionState>,
    data_dir: PathBuf,
    dump_prompts: bool,
) -> AcpSessionParams {
    let mcp_servers = resolve_mcp_servers(&config, user_mcp_servers);
    apply_gemini_mcp_isolation_args(&mut command_spec, &metadata, &config, &mcp_servers);
    let preset_context = compose_preset_context(config.preset_context.as_deref());

    AcpSessionParams {
        conversation_id,
        user_id,
        workspace,
        metadata,
        command_spec,
        config,
        mcp_servers,
        preset_context,
        session_snapshot,
        data_dir,
        dump_prompts,
    }
}

/// Determine which MCP servers to inject into `session/new`.
///
/// Layout: `[team?, ...user_mcp_servers]`. The user's
/// own enabled MCP servers are always appended on top so a team
/// session still gets the operator's tools.
fn resolve_mcp_servers(config: &AcpBuildExtra, user_mcp_servers: Vec<McpServer>) -> Vec<McpServer> {
    let mut servers: Vec<McpServer> = Vec::new();
    if let Some(cfg) = config.team_mcp_stdio_config.as_ref() {
        servers.push(team_mcp_server(cfg));
    }
    servers.extend(user_mcp_servers);
    servers
}

/// Allowlist entry that names no real server. gemini-cli treats an
/// `--allowed-mcp-server-names` list as "only these", so a managed session
/// with an EMPTY WorkMate allowlist passes this to mean "none at all".
pub(crate) const GEMINI_NO_MCP_SENTINEL: &str = "__csbu_workmate_no_mcp__";

/// The gemini-cli backend (`gemini --acp`). Its forks (qwen, …) are NOT
/// assumed to honor the same flags until verified.
fn is_gemini_cli(metadata: &AgentMetadata) -> bool {
    metadata.backend.as_deref() == Some("gemini")
}

/// M7 E1: whether this ACP launch is isolated from ambient MCP config. Only a
/// managed (`extra.mcp_strict`) gemini session is: gemini-cli otherwise merges
/// user, system and WORKSPACE `.gemini/settings.json` servers plus extension
/// servers into every ACP session (verified against gemini-cli 0.65.0, see
/// `docs/workmate-agent-v1/41-backend-mcp-matrix.md`).
pub(crate) fn acp_launch_is_mcp_strict(metadata: &AgentMetadata, config: &AcpBuildExtra) -> bool {
    config.mcp_strict && is_gemini_cli(metadata)
}

/// Name of an ACP MCP server entry; `None` for transports this build does not
/// model (never allowlisted, so they fail closed under a strict launch).
pub(crate) fn acp_mcp_server_name(server: &McpServer) -> Option<&str> {
    match server {
        McpServer::Stdio(s) => Some(s.name.as_str()),
        McpServer::Http(s) => Some(s.name.as_str()),
        McpServer::Sse(s) => Some(s.name.as_str()),
        _ => None,
    }
}

/// M7 E1 (B-09 for gemini): a managed gemini session may load ONLY the MCP
/// servers WorkMate injects through `session/new`, and no extensions (which
/// can carry their own MCP servers). Legacy / unmanaged sessions are untouched.
fn apply_gemini_mcp_isolation_args(
    command_spec: &mut CommandSpec,
    metadata: &AgentMetadata,
    config: &AcpBuildExtra,
    mcp_servers: &[McpServer],
) {
    if !acp_launch_is_mcp_strict(metadata, config) {
        return;
    }
    let names: Vec<&str> = mcp_servers.iter().filter_map(acp_mcp_server_name).collect();
    let allowlist = if names.is_empty() {
        GEMINI_NO_MCP_SENTINEL.to_owned()
    } else {
        names.join(",")
    };
    command_spec.args.extend([
        "--allowed-mcp-server-names".to_owned(),
        allowlist,
        "--extensions".to_owned(),
        "none".to_owned(),
    ]);
}

/// M7 E1: launch-config tool surface of a generic ACP session (see
/// `AcpAgentManager::tool_surface`).
pub(crate) fn tool_surface_from_params(params: &AcpSessionParams) -> aionui_api_types::ConversationToolSurface {
    let isolation = if acp_launch_is_mcp_strict(&params.metadata, &params.config) {
        aionui_api_types::McpIsolation::Strict
    } else {
        aionui_api_types::McpIsolation::Ambient
    };
    let injected = params
        .mcp_servers
        .iter()
        .filter_map(acp_mcp_server_name)
        .map(str::to_owned)
        .collect();
    aionui_api_types::ConversationToolSurface::launch_config(injected, isolation)
}

/// Compose first-message preset context.
fn compose_preset_context(base_preset_context: Option<&str>) -> Option<String> {
    base_preset_context
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn team_mcp_server(cfg: &TeamMcpStdioConfig) -> McpServer {
    let env = vec![
        EnvVariable::new(TeamMcpStdioConfig::ENV_PORT.to_owned(), cfg.port.to_string()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_TOKEN.to_owned(), cfg.token.clone()),
        EnvVariable::new(TeamMcpStdioConfig::ENV_SLOT_ID.to_owned(), cfg.slot_id.clone()),
    ];
    let stdio = McpServerStdio::new(TEAM_MCP_SERVER_NAME, &cfg.binary_path)
        .args(vec!["mcp-team-stdio".to_owned()])
        .env(env);
    McpServer::Stdio(stdio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compose_preset_context_returns_trimmed_base_only() {
        assert_eq!(
            compose_preset_context(Some("  frozen rules  ")),
            Some("frozen rules".to_owned())
        );
        let result = compose_preset_context(Some("  "));
        assert_eq!(result, None);
        assert_eq!(compose_preset_context(None), None);
    }

    fn user_stdio(name: &str) -> McpServer {
        McpServer::Stdio(McpServerStdio::new(name, "/bin/sh"))
    }

    fn team_cfg() -> TeamMcpStdioConfig {
        TeamMcpStdioConfig {
            team_id: "team-1".into(),
            port: 9999,
            token: "tok".into(),
            slot_id: "slot-lead".into(),
            binary_path: "/bin/backend".into(),
        }
    }

    fn test_metadata() -> AgentMetadata {
        AgentMetadata {
            id: "agent-1".into(),
            icon: None,
            name: "Test ACP".into(),
            name_i18n: None,
            description: None,
            description_i18n: None,
            backend: Some("claude".into()),
            agent_type: aionui_common::AgentType::Acp,
            agent_source: aionui_api_types::AgentSource::Builtin,
            agent_source_info: aionui_api_types::AgentSourceInfo::default(),
            enabled: true,
            available: true,
            command: Some("claude".into()),
            resolved_command: None,
            args: vec![],
            env: vec![],
            native_skills_dirs: None,
            skill_delivery: None,
            behavior_policy: aionui_api_types::BehaviorPolicy::default(),
            yolo_id: None,
            sort_order: 0,
            team_capable: true,
            last_check_status: None,
            last_check_kind: None,
            last_check_error_code: None,
            last_check_error_message: None,
            last_check_error_details: None,
            last_check_guidance: None,
            last_check_latency_ms: None,
            last_check_at: None,
            last_success_at: None,
            last_failure_at: None,
            handshake: aionui_api_types::AgentHandshake::default(),
            has_command_override: false,
            env_override_key_count: 0,
        }
    }

    #[tokio::test]
    async fn assemble_acp_params_uses_frozen_preset_context_and_snapshot_seeds() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            preset_context: Some("frozen rules".into()),
            skills: vec!["pdf".into()],
            mcp_server_ids: Some(vec!["mcp-docs".into()]),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };

        let params = assemble_acp_params(
            "conv-1".into(),
            "user-1".into(),
            WorkspaceInfo {
                path: "/tmp/workspace".into(),
                is_custom: false,
            },
            test_metadata(),
            CommandSpec::default(),
            config,
            vec![user_stdio("mcp-docs")],
            None,
            PathBuf::from("/tmp/data"),
            true,
        )
        .await;

        assert!(params.dump_prompts);
        assert_eq!(params.preset_context.as_deref(), Some("frozen rules"));
        assert_eq!(params.config.skills, vec!["pdf"]);
        assert_eq!(
            params.config.mcp_server_ids.as_deref(),
            Some(&["mcp-docs".to_owned()][..])
        );
        assert_eq!(params.mcp_servers.len(), 2);
    }

    #[test]
    fn resolve_mcp_servers_solo_only_gets_user_mcp_servers() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, vec![user_stdio("mcp-docs")]);
        assert_eq!(servers.len(), 1);
        match &servers[0] {
            McpServer::Stdio(s) => assert_eq!(s.name, "mcp-docs"),
            _ => panic!("expected user stdio server"),
        }
    }

    #[test]
    fn resolve_mcp_servers_team_session_keeps_team_mcp_before_user_mcp() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, vec![user_stdio("mcp-docs")]);
        assert_eq!(servers.len(), 2);
        match &servers[0] {
            McpServer::Stdio(s) => {
                assert_eq!(s.name, TEAM_MCP_SERVER_NAME);
                assert_eq!(s.args, vec!["mcp-team-stdio".to_owned()]);
                assert_eq!(s.command.to_string_lossy(), "/bin/backend");
                // The stdio server carries no port/token/slot arguments; the whole
                // connection triple travels in env, so the teammate identity cannot
                // be forged from the command line.
                let env: std::collections::HashMap<_, _> = s
                    .env
                    .iter()
                    .map(|variable| (variable.name.as_str(), variable.value.as_str()))
                    .collect();
                assert_eq!(env[TeamMcpStdioConfig::ENV_PORT], "9999");
                assert_eq!(env[TeamMcpStdioConfig::ENV_TOKEN], "tok");
                assert_eq!(env[TeamMcpStdioConfig::ENV_SLOT_ID], "slot-lead");
            }
            _ => panic!("expected stdio"),
        }
        match &servers[1] {
            McpServer::Stdio(s) => assert_eq!(s.name, "mcp-docs"),
            _ => panic!("expected stdio"),
        }
    }

    #[tokio::test]
    async fn session_new_payload_contains_team_nonbuiltin_and_builtin_mcp_servers() {
        let config = AcpBuildExtra {
            backend: Some("custom-acp".into()),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };
        let params = assemble_acp_params(
            "conv-1".into(),
            "user-1".into(),
            WorkspaceInfo {
                path: "/tmp/workspace".into(),
                is_custom: false,
            },
            test_metadata(),
            CommandSpec::default(),
            config,
            vec![user_stdio("mcp-docs"), user_stdio("chrome-devtools")],
            None,
            PathBuf::from("/tmp/data"),
            false,
        )
        .await;

        let request = params.new_session_request();
        let names = request
            .mcp_servers
            .iter()
            .map(|server| match server {
                McpServer::Stdio(server) => server.name.as_str(),
                McpServer::Http(server) => server.name.as_str(),
                McpServer::Sse(server) => server.name.as_str(),
                _ => panic!("unexpected MCP transport"),
            })
            .collect::<Vec<_>>();

        assert_eq!(names, [TEAM_MCP_SERVER_NAME, "mcp-docs", "chrome-devtools"]);
    }

    /// The pre-fix bug: with no team configured and an empty
    /// user-server list, the payload is empty. This is the *no-fix*
    /// scenario and remains valid (no MCP configured anywhere).
    #[test]
    fn resolve_mcp_servers_empty_when_nothing_configured() {
        let config = AcpBuildExtra {
            backend: Some("claude".into()),
            ..Default::default()
        };
        let servers = resolve_mcp_servers(&config, Vec::new());
        assert!(servers.is_empty());
    }

    // ── M7 E1: gemini managed-MCP launch isolation ──────────────────────

    fn gemini_metadata() -> AgentMetadata {
        AgentMetadata {
            backend: Some("gemini".into()),
            command: Some("gemini".into()),
            args: vec!["--acp".into()],
            ..test_metadata()
        }
    }

    fn gemini_spec() -> CommandSpec {
        CommandSpec {
            command: "gemini".into(),
            args: vec!["--acp".into()],
            ..Default::default()
        }
    }

    async fn assemble_with(
        metadata: AgentMetadata,
        config: AcpBuildExtra,
        user_mcp_servers: Vec<McpServer>,
    ) -> AcpSessionParams {
        assemble_acp_params(
            "conv-1".into(),
            "user-1".into(),
            WorkspaceInfo {
                path: "/tmp/workspace".into(),
                is_custom: false,
            },
            metadata,
            gemini_spec(),
            config,
            user_mcp_servers,
            None,
            PathBuf::from("/tmp/data"),
            false,
        )
        .await
    }

    #[tokio::test]
    async fn managed_gemini_with_empty_allowlist_launches_with_no_mcp_at_all() {
        let config = AcpBuildExtra {
            backend: Some("gemini".into()),
            mcp_strict: true,
            mcp_server_ids: Some(vec![]),
            ..Default::default()
        };
        let params = assemble_with(gemini_metadata(), config, Vec::new()).await;
        assert!(params.mcp_servers.is_empty());
        assert_eq!(
            params.command_spec.args,
            vec![
                "--acp",
                "--allowed-mcp-server-names",
                GEMINI_NO_MCP_SENTINEL,
                "--extensions",
                "none"
            ]
        );
        assert!(acp_launch_is_mcp_strict(&params.metadata, &params.config));
    }

    #[tokio::test]
    async fn managed_gemini_allowlists_exactly_the_injected_servers_including_team() {
        let config = AcpBuildExtra {
            backend: Some("gemini".into()),
            mcp_strict: true,
            mcp_server_ids: Some(vec!["mcp-docs".into()]),
            team_mcp_stdio_config: Some(team_cfg()),
            ..Default::default()
        };
        let params = assemble_with(gemini_metadata(), config, vec![user_stdio("docs")]).await;
        let names: Vec<&str> = params.mcp_servers.iter().filter_map(acp_mcp_server_name).collect();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&"docs"));
        assert!(names.contains(&TEAM_MCP_SERVER_NAME));
        let args = &params.command_spec.args;
        assert_eq!(args[0], "--acp");
        assert_eq!(args[1], "--allowed-mcp-server-names");
        assert_eq!(args[2], names.join(","));
        assert!(!args[2].contains(GEMINI_NO_MCP_SENTINEL));
        assert_eq!(&args[3..], ["--extensions", "none"]);
    }

    #[tokio::test]
    async fn unmanaged_gemini_launch_args_are_unchanged() {
        // Legacy / unmanaged session: no mcp_strict stamp → historical launch.
        let config = AcpBuildExtra {
            backend: Some("gemini".into()),
            ..Default::default()
        };
        let params = assemble_with(gemini_metadata(), config, vec![user_stdio("docs")]).await;
        assert_eq!(params.command_spec.args, vec!["--acp"]);
        assert!(!acp_launch_is_mcp_strict(&params.metadata, &params.config));
    }

    #[tokio::test]
    async fn non_gemini_acp_backends_are_not_given_gemini_flags_even_when_strict() {
        for backend in ["qwen", "codebuddy", "claude"] {
            let metadata = AgentMetadata {
                backend: Some(backend.into()),
                ..test_metadata()
            };
            let config = AcpBuildExtra {
                backend: Some(backend.into()),
                mcp_strict: true,
                mcp_server_ids: Some(vec![]),
                ..Default::default()
            };
            let params = assemble_with(metadata, config, Vec::new()).await;
            assert_eq!(params.command_spec.args, vec!["--acp"], "{backend}");
            assert!(!acp_launch_is_mcp_strict(&params.metadata, &params.config), "{backend}");
        }
    }

    #[tokio::test]
    async fn acp_tool_surface_is_strict_only_for_managed_gemini() {
        use aionui_api_types::{McpDiscovery, McpIsolation, ToolSurfaceSource};
        let strict = AcpBuildExtra {
            backend: Some("gemini".into()),
            mcp_strict: true,
            mcp_server_ids: Some(vec!["mcp-docs".into()]),
            ..Default::default()
        };
        let params = assemble_with(gemini_metadata(), strict, vec![user_stdio("docs")]).await;
        let surface = tool_surface_from_params(&params);
        assert_eq!(surface.source, ToolSurfaceSource::LaunchConfig);
        assert_eq!(surface.mcp_isolation, McpIsolation::Strict);
        assert_eq!(surface.mcp_discovery, McpDiscovery::Unavailable);
        assert_eq!(surface.mcp_servers.len(), 1);
        assert_eq!(surface.mcp_servers[0].name, "docs");
        assert!(!surface.mcp_servers[0].external);
        assert!(surface.external_mcp_servers.is_empty());
        assert!(surface.tools.is_empty());

        // Unmanaged gemini and every other ACP backend: honest "may load local MCP".
        let unmanaged = AcpBuildExtra {
            backend: Some("gemini".into()),
            ..Default::default()
        };
        let params = assemble_with(gemini_metadata(), unmanaged, vec![user_stdio("docs")]).await;
        let surface = tool_surface_from_params(&params);
        assert_eq!(surface.mcp_isolation, McpIsolation::Ambient);
        assert_eq!(surface.mcp_discovery, McpDiscovery::Unavailable);

        let qwen = AgentMetadata {
            backend: Some("qwen".into()),
            ..test_metadata()
        };
        let cfg = AcpBuildExtra {
            backend: Some("qwen".into()),
            mcp_strict: true,
            mcp_server_ids: Some(vec![]),
            ..Default::default()
        };
        let params = assemble_with(qwen, cfg, Vec::new()).await;
        let surface = tool_surface_from_params(&params);
        assert_eq!(surface.mcp_isolation, McpIsolation::Ambient);
        assert!(surface.mcp_servers.is_empty());
    }
}
