//! `aioncore delegate` — the agent-facing `@assistant` delegation CLI (3.0 C7).
//!
//! Reuses the `conversation` CLI's runtime env, envelope and error-code family
//! (`cmd_conversation.rs`). The parent conversation is always the one bound to
//! the runtime token; stdin cannot name another.

use std::io::{self, Read};
use std::process::ExitCode;

use aionui_api_types::{
    ConversationCliEnvelope, ConversationToolErrorCode, ConversationToolErrorPayload, DelegateToolName,
    delegate_tool_descriptor,
};
use serde_json::{Value, json};

use crate::cli::{DelegateArgs, DelegateCommand};
use crate::commands::cmd_conversation::{
    print_failure, print_json, print_response, runtime_env, runtime_error, unknown_command,
};
use crate::commands::delegate_capabilities;

pub(crate) async fn run_delegate(args: DelegateArgs) -> ExitCode {
    match run_delegate_inner(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(code) => code,
    }
}

async fn run_delegate_inner(args: DelegateArgs) -> Result<(), ExitCode> {
    match args.command {
        DelegateCommand::Capabilities => print_json(&ConversationCliEnvelope::success(
            delegate_capabilities::data(),
            Some("delegate capabilities".to_owned()),
        )),
        DelegateCommand::Start => start().await,
        DelegateCommand::Result => result().await,
        DelegateCommand::List => list().await,
        DelegateCommand::Unknown(path) => Err(unknown_command("delegate", path, "unknown delegate command")),
    }
}

async fn start() -> Result<(), ExitCode> {
    let command = "delegate start";
    let env = runtime_env(command)?;
    let body = read_stdin(command, DelegateToolName::DelegateStart)?;
    let url = format!("{}/api/runtime/delegations", env.base_url.trim_end_matches('/'));
    let response = reqwest::Client::new()
        .post(url)
        .headers(env.headers(command)?)
        .json(&body)
        .send()
        .await
        .map_err(|error| runtime_error(command, "DELEGATE_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

async fn result() -> Result<(), ExitCode> {
    let command = "delegate result";
    let env = runtime_env(command)?;
    let body = read_stdin(command, DelegateToolName::DelegateResult)?;
    let id = body["id"].as_str().map(str::trim).unwrap_or_default();
    if id.is_empty() || id.contains('/') {
        return Err(schema_failure(command, "id must be a non-empty handle"));
    }
    let url = format!("{}/api/runtime/delegations/{id}", env.base_url.trim_end_matches('/'));
    get(command, &env, url).await
}

async fn list() -> Result<(), ExitCode> {
    let command = "delegate list";
    let env = runtime_env(command)?;
    read_stdin(command, DelegateToolName::DelegateList)?;
    let url = format!("{}/api/runtime/delegations", env.base_url.trim_end_matches('/'));
    get(command, &env, url).await
}

async fn get(command: &str, env: &crate::commands::cmd_conversation::RuntimeEnv, url: String) -> Result<(), ExitCode> {
    let response = reqwest::Client::new()
        .get(url)
        .headers(env.headers(command)?)
        .send()
        .await
        .map_err(|error| runtime_error(command, "DELEGATE_CLI_HTTP_BRIDGE_FAILED", error.to_string()))?;
    print_response(command, response).await
}

fn schema_failure(command: &str, message: impl Into<String>) -> ExitCode {
    print_failure(
        command,
        "DELEGATE_CLI_SCHEMA_VALIDATION_FAILED",
        ConversationToolErrorPayload::new(ConversationToolErrorCode::SchemaValidationFailed, message),
    )
}

fn read_stdin(command: &str, tool: DelegateToolName) -> Result<Value, ExitCode> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(|error| schema_failure(command, error.to_string()))?;
    let value = if input.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(&input).map_err(|error| schema_failure(command, error.to_string()))?
    };
    validate(tool, value).map_err(|message| schema_failure(command, message))
}

/// Checks stdin against the registry schema before a round trip.
pub(crate) fn validate(tool: DelegateToolName, value: Value) -> Result<Value, String> {
    let descriptor = delegate_tool_descriptor(tool);
    let Some(object) = value.as_object() else {
        return Err("stdin JSON must be an object".to_owned());
    };
    let properties = descriptor.input_schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    if let Some(key) = object.keys().find(|key| !properties.contains_key(*key)) {
        return Err(format!("unknown stdin field: {key}"));
    }
    for key in descriptor.input_schema["required"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        match object.get(key) {
            Some(Value::String(text)) if !text.trim().is_empty() => {}
            _ => return Err(format!("missing required stdin field: {key}")),
        }
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_needs_both_fields_and_rejects_extras() {
        let tool = DelegateToolName::DelegateStart;
        assert!(validate(tool, json!({ "assistant_id": "writer" })).is_err());
        assert!(validate(tool, json!({ "assistant_id": "writer", "task_text": "  " })).is_err());
        assert!(
            validate(
                tool,
                json!({ "assistant_id": "w", "task_text": "t", "permission": "yolo" })
            )
            .is_err()
        );
        assert!(validate(tool, json!(["x"])).is_err());
        assert!(validate(tool, json!({ "assistant_id": "w", "task_text": "t" })).is_ok());
    }

    #[test]
    fn result_needs_an_id_and_list_takes_nothing() {
        assert!(validate(DelegateToolName::DelegateResult, json!({})).is_err());
        assert!(validate(DelegateToolName::DelegateResult, json!({ "id": "d1" })).is_ok());
        assert!(validate(DelegateToolName::DelegateList, json!({})).is_ok());
        assert!(validate(DelegateToolName::DelegateList, json!({ "id": "d1" })).is_err());
    }
}
