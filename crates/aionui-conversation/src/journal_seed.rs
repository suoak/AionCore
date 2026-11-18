//! Seed an empty canonical journal from a parsed external transcript.
//!
//! The desktop already converted Claude Code / Codex sessions into model-visible
//! text items. This module writes those items as Host journal events so WebUI
//! and remote clients can hydrate from `/transcript`. Content is opaque text;
//! this path never opens a filesystem path or constructs a Host `File`.

use aionui_api_types::{SeedJournalItem, SeedJournalRequest};
use serde_json::{Value, json};

use crate::ConversationError;
use crate::stream_persistence::canonical_event_id;

pub(crate) const MAX_SEED_ITEMS: usize = 2_000;
pub(crate) const MAX_ITEM_BYTES: usize = 256 * 1024;
pub(crate) const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PreparedSeedEvent {
    UserPrompt { msg_id: String, content: String },
    AssistantText { event_id: String, payload: Value },
    ToolCall { event_id: String, payload: Value },
}

pub(crate) fn prepare_seed_events(
    conversation_id: &str,
    request: &SeedJournalRequest,
) -> Result<Vec<PreparedSeedEvent>, ConversationError> {
    if request.items.is_empty() {
        return Err(ConversationError::bad_request("seed items are required"));
    }
    if request.items.len() > MAX_SEED_ITEMS {
        return Err(ConversationError::bad_request(format!(
            "seed exceeds {MAX_SEED_ITEMS} items"
        )));
    }

    let mut prepared = Vec::new();
    let mut total_bytes = 0usize;
    for (index, item) in request.items.iter().enumerate() {
        let content = item.content.trim();
        if content.is_empty() {
            continue;
        }
        if content.len() > MAX_ITEM_BYTES {
            return Err(ConversationError::bad_request(format!(
                "seed item {index} exceeds {MAX_ITEM_BYTES} bytes"
            )));
        }
        total_bytes = total_bytes.saturating_add(content.len());
        if total_bytes > MAX_TOTAL_BYTES {
            return Err(ConversationError::bad_request(format!(
                "seed exceeds {MAX_TOTAL_BYTES} bytes"
            )));
        }
        if let Some(event) = map_item(conversation_id, index, item, content)? {
            prepared.push(event);
        }
    }
    if prepared.is_empty() {
        return Err(ConversationError::bad_request("no seedable model-visible items"));
    }
    Ok(prepared)
}

fn map_item(
    conversation_id: &str,
    index: usize,
    item: &SeedJournalItem,
    content: &str,
) -> Result<Option<PreparedSeedEvent>, ConversationError> {
    match item.transcript_kind.as_str() {
        "user/message" => {
            let msg_id = format!("import-user-{index}");
            Ok(Some(PreparedSeedEvent::UserPrompt {
                msg_id,
                content: content.to_owned(),
            }))
        }
        "assistant/message" => {
            let payload = json!({ "type": "content", "data": { "content": content } });
            let seed = format!("import_text:{conversation_id}:{index}");
            Ok(Some(PreparedSeedEvent::AssistantText {
                event_id: canonical_event_id(&seed, &payload),
                payload,
            }))
        }
        "tool/call" | "tool/result" => {
            let call_id = item
                .call_id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("import-tool-{index}"));
            let payload = json!({
                "type": "tool_call",
                "data": {
                    "call_id": call_id,
                    "status": "completed",
                    "content": content,
                    "output": content,
                }
            });
            let seed = format!("import_tool:{conversation_id}:{index}");
            Ok(Some(PreparedSeedEvent::ToolCall {
                event_id: canonical_event_id(&seed, &payload),
                payload,
            }))
        }
        // Unknown kinds are ignored so a desktop transcript can be posted as-is.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, content: &str) -> SeedJournalItem {
        SeedJournalItem {
            transcript_kind: kind.to_owned(),
            content: content.to_owned(),
            call_id: None,
        }
    }

    #[test]
    fn prepares_user_assistant_and_terminal_tool_events() {
        let request = SeedJournalRequest {
            items: vec![
                item("user/message", "hello"),
                item("assistant/message", "hi"),
                SeedJournalItem {
                    transcript_kind: "tool/call".into(),
                    content: r#"{"name":"Bash","input":{"cmd":"ls"}}"#.into(),
                    call_id: Some("call-1".into()),
                },
                item("tool/result", "ok"),
                item("host/notice", "ignored"),
                item("user/message", "   "),
            ],
            source: Some("claude-code".into()),
            source_session_id: Some("abc".into()),
        };
        let events = prepare_seed_events("conv", &request).unwrap();
        assert_eq!(events.len(), 4);
        match &events[0] {
            PreparedSeedEvent::UserPrompt { content, .. } => assert_eq!(content, "hello"),
            other => panic!("expected user prompt, got {other:?}"),
        }
        match &events[1] {
            PreparedSeedEvent::AssistantText { payload, .. } => {
                assert_eq!(payload["data"]["content"], "hi");
            }
            other => panic!("expected assistant text, got {other:?}"),
        }
        match &events[2] {
            PreparedSeedEvent::ToolCall { payload, .. } => {
                assert_eq!(payload["data"]["call_id"], "call-1");
                assert_eq!(payload["data"]["status"], "completed");
                assert_eq!(payload["data"]["output"], r#"{"name":"Bash","input":{"cmd":"ls"}}"#);
            }
            other => panic!("expected tool call, got {other:?}"),
        }
    }

    #[test]
    fn rejects_empty_and_oversized_requests() {
        let empty = SeedJournalRequest {
            items: vec![],
            source: None,
            source_session_id: None,
        };
        assert!(matches!(
            prepare_seed_events("conv", &empty),
            Err(ConversationError::BadRequest { .. })
        ));

        let skipped = SeedJournalRequest {
            items: vec![item("unknown", "x")],
            source: None,
            source_session_id: None,
        };
        assert!(matches!(
            prepare_seed_events("conv", &skipped),
            Err(ConversationError::BadRequest { reason }) if reason.contains("no seedable")
        ));

        let too_many = SeedJournalRequest {
            items: (0..=MAX_SEED_ITEMS)
                .map(|i| item("user/message", &format!("x{i}")))
                .collect(),
            source: None,
            source_session_id: None,
        };
        assert!(matches!(
            prepare_seed_events("conv", &too_many),
            Err(ConversationError::BadRequest { reason }) if reason.contains("items")
        ));
    }

    #[test]
    fn request_type_ignores_path_fields() {
        let parsed: SeedJournalRequest = serde_json::from_value(json!({
            "items": [{ "transcript_kind": "user/message", "content": "hi", "path": "/tmp/secret.jsonl" }],
            "source": "codex",
            "source_path": "/tmp/secret.jsonl"
        }))
        .unwrap();
        assert_eq!(parsed.items[0].content, "hi");
        assert_eq!(parsed.source.as_deref(), Some("codex"));
        let events = prepare_seed_events("conv", &parsed).unwrap();
        assert_eq!(events.len(), 1);
    }
}
