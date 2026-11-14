//! Which connections may see a bus event (live delivery and E14 resume replay).
//!
//! Brand: CSBU WorkMate. Live delivery (`aionui-app` bus → WebSocket bridge) and
//! resume replay must apply the same rule, or a reconnecting client could
//! receive another user's events from the shared resume ring.

use aionui_api_types::WebSocketMessage;
use serde_json::Value;

/// Events without a `user_id` that every authenticated connection receives.
pub fn is_global_websocket_event(event_name: &str) -> bool {
    matches!(
        event_name,
        "runtime.statusChanged" | "extensions.lifecycle" | "hub.state-changed"
    )
}

/// The `user_id` an event is addressed to, if any.
pub fn event_user_id(event: &WebSocketMessage<Value>) -> Option<&str> {
    event.data.get("user_id").and_then(Value::as_str)
}

/// True when a connection authenticated as `user_id` may receive `event`:
/// the event names that user, or it carries no `user_id` and is an explicit
/// global event. Everything else is dropped, as in live delivery.
pub fn event_visible_to_user(event: &WebSocketMessage<Value>, user_id: &str) -> bool {
    match event_user_id(event) {
        Some(owner) => owner == user_id,
        None => is_global_websocket_event(&event.name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn own_events_and_globals_are_visible() {
        let own = WebSocketMessage::new("conversation.listChanged", json!({"user_id": "alice"}));
        let other = WebSocketMessage::new("conversation.listChanged", json!({"user_id": "bob"}));
        let global = WebSocketMessage::new("extensions.lifecycle", json!({}));
        let unscoped = WebSocketMessage::new("message.stream", json!({"text": "secret"}));
        assert!(event_visible_to_user(&own, "alice"));
        assert!(!event_visible_to_user(&other, "alice"));
        assert!(event_visible_to_user(&global, "alice"));
        assert!(!event_visible_to_user(&unscoped, "alice"));
    }

    #[test]
    fn non_string_user_id_is_not_global() {
        let odd = WebSocketMessage::new("extensions.lifecycle", json!({"user_id": 7}));
        assert!(event_user_id(&odd).is_none());
        // Falls back to the global rule by name only.
        assert!(event_visible_to_user(&odd, "alice"));
        assert!(!is_global_websocket_event("extensions.state-changed"));
    }
}
