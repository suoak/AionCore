//! M8 E7-4: Squad-specific prompt sections appended to the role prompt.

/// Appended to a `route_only` Squad Leader's role prompt. The rules are also
/// enforced at runtime (Team MCP gate + backend native tool exclusion); the
/// prompt only keeps the model from wasting turns on refused calls.
pub const SQUAD_ROUTER_PROMPT: &str = r#"## Squad Router Mode

You lead a fixed Squad and act as a **router only**:
- Break the user's request into tasks and route each to the best-fit member with `team_send_message` / `team_task_update`; use `team_task_list` to track progress and `team_describe_assistant` to check a member's skills.
- Do NOT do the work yourself: you cannot write files, run commands, or start sub-agents, and you cannot add, remove, or spawn members. Such calls are refused.
- Reading for context is fine. When members report back, verify, consolidate, and answer the user."#;

/// Appended to every Squad member's role prompt.
pub const SQUAD_MEMBER_PROMPT: &str = r#"## Squad Member Rules

You are a member of a fixed Squad. Do the routed work yourself: starting sub-agents or spawning teammates is forbidden and will be refused. Report results to the Leader with `team_send_message`."#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn router_prompt_names_only_route_tools() {
        for tool in [
            "team_send_message",
            "team_task_update",
            "team_task_list",
            "team_describe_assistant",
        ] {
            assert!(SQUAD_ROUTER_PROMPT.contains(tool), "{tool}");
        }
        assert!(!SQUAD_ROUTER_PROMPT.contains("team_spawn_agent"));
        assert!(SQUAD_MEMBER_PROMPT.contains("forbidden"));
    }
}
