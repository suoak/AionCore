use aionui_api_types::{DELEGATE_TOOLS_SCHEMA_VERSION, MAX_CONCURRENT_DELEGATIONS, delegate_tool_descriptors};
use serde_json::{Value, json};

pub(crate) fn data() -> Value {
    let tools = delegate_tool_descriptors()
        .into_iter()
        .map(|tool| {
            json!({
                "name": tool.name,
                "cli_command": tool.cli_command,
                "description": tool.description,
                "when": tool.when,
                "input_summary": tool.input_summary,
                "stdin_json_schema": tool.input_schema,
            })
        })
        .collect::<Vec<_>>();
    let runtime = [
        "AIONUI_BASE_URL",
        "AIONUI_USER_ID",
        "AIONUI_CONVERSATION_ID",
        "AIONUI_RUNTIME_TOKEN",
    ];
    json!({
        "schema_version": DELEGATE_TOOLS_SCHEMA_VERSION,
        "contract": "agent-facing-delegate-cli",
        "commands": {
            "capabilities": { "runtime_env_required": [] },
            "start": { "runtime_env_required": runtime },
            "result": { "runtime_env_required": runtime },
            "list": { "runtime_env_required": runtime }
        },
        "output_envelope": {
            "success": "boolean",
            "data": "object when success=true: { id, parent_conversation_id, child_conversation_id?, assistant_id, task_text, status, error?, result_text?, created_at, updated_at } (an array for list)",
            "error": "object when success=false: { code, message, details? }",
            "meta": { "schema_version": DELEGATE_TOOLS_SCHEMA_VERSION }
        },
        "semantics": {
            "non_blocking": "start returns as soon as the assistant's conversation exists; read the result later with `delegate result`",
            "only_user_picked": "only assistants the user picked with @ in THIS conversation can be delegated to",
            "cold_start": "the assistant sees only task_text, not this conversation",
            "depth": "a delegated conversation cannot delegate further",
            "concurrency": format!("at most {MAX_CONCURRENT_DELEGATIONS} unfinished delegations per conversation"),
            "permissions": "the assistant runs with its own permission mode; nothing here can raise it"
        },
        "tools": tools,
        "errors": [
            "delegation_not_granted",
            "delegation_depth_exceeded",
            "delegation_limit_reached",
            "launch_denied",
            "not_found",
            "invalid_request",
            "internal",
            "runtime_auth_failed",
            "schema_validation_failed",
            "transport_unavailable"
        ]
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_lists_every_registry_tool_and_command() {
        let data = data();
        assert_eq!(
            data["tools"].as_array().unwrap().len(),
            delegate_tool_descriptors().len()
        );
        for descriptor in delegate_tool_descriptors() {
            assert!(
                data["commands"].get(&descriptor.cli_command[0]).is_some(),
                "{} has no commands entry",
                descriptor.name
            );
        }
    }
}
