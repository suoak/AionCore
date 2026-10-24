use std::sync::Weak;
use std::time::Instant;

use aionui_api_types::{
    TeamToolCall, TeamToolDescriptor, TeamToolErrorCode, TeamToolErrorPayload, TeamToolName, TeamToolRole,
    TeamToolTransport,
};
use serde_json::Value;
use tracing::{info, warn};

use crate::mcp::server::ToolCallError;
use crate::scheduler::TeammateManager;
use crate::service::TeamSessionService;
use crate::types::TeammateRole;

#[derive(Debug, Clone)]
pub struct TeamToolContext {
    pub team_id: String,
    pub caller_slot_id: String,
    pub caller_role: TeammateRole,
    pub user_id: Option<String>,
    pub conversation_id: Option<String>,
    pub transport: TeamToolTransport,
}

pub struct TeamToolExecutor<'a> {
    scheduler: &'a TeammateManager,
    service: &'a Weak<TeamSessionService>,
}

impl<'a> TeamToolExecutor<'a> {
    pub fn new(scheduler: &'a TeammateManager, service: &'a Weak<TeamSessionService>) -> Self {
        Self { scheduler, service }
    }

    pub fn list_tools(&self, context: &TeamToolContext) -> Vec<TeamToolDescriptor> {
        aionui_api_types::team_tool_descriptors_for_role(team_tool_role(context.caller_role))
    }

    /// Runtime tool list: like `list_tools`, but trims a route_only Squad
    /// Leader down to the routing tools (needs a live service).
    pub async fn list_tools_runtime(&self, context: &TeamToolContext) -> Vec<TeamToolDescriptor> {
        let tools = self.list_tools(context);
        if context.caller_role == TeammateRole::Lead
            && let Some(service) = self.service.upgrade()
            && service.squad_leader_route_only(&context.team_id).await
        {
            return filter_squad_route_only(tools);
        }
        tools
    }

    pub async fn execute(&self, context: &TeamToolContext, call: TeamToolCall) -> Result<Value, TeamToolErrorPayload> {
        let started = Instant::now();
        let tool = call.tool.as_str();
        if context.caller_role == TeammateRole::Lead
            && !squad_route_only_allows(tool)
            && let Some(service) = self.service.upgrade()
            && service.squad_leader_route_only(&context.team_id).await
        {
            warn!(team_id = %context.team_id, caller_slot_id = %context.caller_slot_id, tool, "Squad route_only Leader tool call rejected");
            return Err(squad_route_only_error(&context.team_id, tool));
        }
        let result = crate::mcp::server::dispatch_tool(
            tool,
            &call.arguments,
            self.scheduler,
            self.service,
            &context.team_id,
            &context.caller_slot_id,
            context.caller_role,
        )
        .await
        .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)))
        .map_err(map_tool_error);

        let duration_ms = started.elapsed().as_millis();
        match &result {
            Ok(_) => info!(
                transport = ?context.transport,
                team_id = %context.team_id,
                caller_slot_id = %context.caller_slot_id,
                conversation_id = context.conversation_id.as_deref().unwrap_or(""),
                tool,
                result = "success",
                duration_ms,
                "Team tool call completed"
            ),
            Err(error) => warn!(
                transport = ?context.transport,
                team_id = %context.team_id,
                caller_slot_id = %context.caller_slot_id,
                conversation_id = context.conversation_id.as_deref().unwrap_or(""),
                tool,
                error_code = ?error.code,
                classification = error.details.as_ref().and_then(|details| details.get("classification")).and_then(|value| value.as_str()).unwrap_or("business_error"),
                duration_ms,
                "Team tool call failed"
            ),
        }
        result
    }
}

pub fn team_tool_call_from_name(tool_name: &str, arguments: Value) -> Result<TeamToolCall, TeamToolErrorPayload> {
    let tool = TeamToolName::parse(tool_name).ok_or_else(|| {
        TeamToolErrorPayload::new(TeamToolErrorCode::UnknownTool, format!("Unknown tool: {tool_name}"))
    })?;
    Ok(TeamToolCall { tool, arguments })
}

/// M8 E7-3: whether a route_only Squad Leader may call `tool`.
pub fn squad_route_only_allows(tool: &str) -> bool {
    crate::squad::SQUAD_ROUTE_ONLY_LEADER_TOOLS.contains(&tool)
}

/// Drop tools a route_only Squad Leader may not use from a tool list.
pub fn filter_squad_route_only(tools: Vec<TeamToolDescriptor>) -> Vec<TeamToolDescriptor> {
    tools
        .into_iter()
        .filter(|descriptor| squad_route_only_allows(descriptor.name.as_str()))
        .collect()
}

fn squad_route_only_error(team_id: &str, tool: &str) -> TeamToolErrorPayload {
    TeamToolErrorPayload::new(
        TeamToolErrorCode::PermissionDenied,
        format!("Squad Leader is route_only; tool {tool} is not allowed"),
    )
    .with_details(serde_json::json!({
        "code": "squad_route_only_tool_forbidden",
        "team_id": team_id,
        "tool": tool,
        "allowed": crate::squad::SQUAD_ROUTE_ONLY_LEADER_TOOLS,
    }))
}

fn team_tool_role(role: TeammateRole) -> TeamToolRole {
    match role {
        TeammateRole::Lead => TeamToolRole::Lead,
        TeammateRole::Teammate => TeamToolRole::Teammate,
    }
}

fn map_tool_error(error: ToolCallError) -> TeamToolErrorPayload {
    let code = if error.message.starts_with("Unknown tool:") {
        TeamToolErrorCode::UnknownTool
    } else if error.message.starts_with("Only Lead") {
        TeamToolErrorCode::PermissionDenied
    } else if error.message.starts_with("Invalid params")
        || error.message.starts_with("Missing required field")
        || error.message.contains("does not accept arguments")
        || error.message.contains("is no longer accepted")
    {
        TeamToolErrorCode::SchemaValidationFailed
    } else if error.message.contains("Invalid agent target") {
        TeamToolErrorCode::AgentNotFound
    } else if error.message.contains("No active session") {
        TeamToolErrorCode::TeamNotFound
    } else if error.message.contains("Team service not available") {
        TeamToolErrorCode::TransportUnavailable
    } else {
        TeamToolErrorCode::RuntimeContextMissing
    };

    let mut payload = TeamToolErrorPayload::new(code, error.message);
    if let Some(details) = error.details {
        payload = payload.with_details(details);
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_tool_maps_to_shared_error_payload() {
        let err = team_tool_call_from_name("missing_tool", json!({})).unwrap_err();
        assert_eq!(err.code, TeamToolErrorCode::UnknownTool);
        assert!(err.message.contains("Unknown tool"));
    }

    #[test]
    fn squad_route_only_allows_only_routing_tools() {
        for tool in crate::squad::SQUAD_ROUTE_ONLY_LEADER_TOOLS {
            assert!(squad_route_only_allows(tool));
        }
        assert!(!squad_route_only_allows("team_spawn_agent"));
        let lead = aionui_api_types::team_tool_descriptors_for_role(TeamToolRole::Lead);
        let filtered = filter_squad_route_only(lead.clone());
        assert!(filtered.len() < lead.len());
        assert!(filtered.iter().all(|d| squad_route_only_allows(d.name.as_str())));
        let err = squad_route_only_error("t1", "team_spawn_agent");
        assert_eq!(err.code, TeamToolErrorCode::PermissionDenied);
        assert_eq!(err.details.unwrap()["code"], "squad_route_only_tool_forbidden");
    }
}
