//! 3.0 C9: remote approval cards in IM.
//!
//! An approval card carries two buttons whose payload is the request id, a
//! digest prefix and the decision (`apr.d` with `r`, `d`, `v`). Only a button
//! press counts — typed text, forwarded messages and the bot's own messages
//! never approve anything — and Core checks that the presser's platform
//! identity is bound to the owner as an approver and still paired.

use std::collections::HashMap;

use async_trait::async_trait;

use crate::types::{ActionBehavior, ActionButton, ActionResponse};

/// Button action of an approval card.
pub const APPROVAL_ACTION: &str = "apr.d";
/// How much of the digest an IM button carries (Telegram allows 64 bytes).
pub const CARD_DIGEST_PREFIX: usize = 24;

/// Implemented by the app over the conversation service.
#[async_trait]
pub trait ChannelApprovalPort: Send + Sync {
    /// `Ok(message)` once decided (and executed); `Err(message)` when refused.
    async fn decide(
        &self,
        owner_user_id: &str,
        platform: &str,
        identity: &str,
        request_id: &str,
        digest_prefix: &str,
        approve: bool,
    ) -> Result<String, String>;
}

/// The two buttons of an approval card.
pub fn approval_buttons(request_id: &str, digest: &str) -> Vec<Vec<ActionButton>> {
    let prefix: String = digest.chars().take(CARD_DIGEST_PREFIX).collect();
    let button = |label: &str, value: &str| ActionButton {
        label: label.to_owned(),
        action: APPROVAL_ACTION.to_owned(),
        params: Some(HashMap::from([
            ("r".to_owned(), request_id.to_owned()),
            ("d".to_owned(), prefix.clone()),
            ("v".to_owned(), value.to_owned()),
        ])),
    };
    vec![vec![button("Approve", "a"), button("Reject", "r")]]
}

pub(crate) fn answer(text: String) -> ActionResponse {
    ActionResponse {
        text: Some(text),
        parse_mode: None,
        buttons: None,
        keyboard: None,
        behavior: ActionBehavior::Send,
        toast: None,
        edit_message_id: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_buttons_fit_a_telegram_payload() {
        let buttons = approval_buttons("0123456789abcdef", &"f".repeat(64));
        for button in &buttons[0] {
            let params = button.params.as_ref().unwrap();
            assert_eq!(params["d"].len(), CARD_DIGEST_PREFIX);
            let encoded: Vec<String> = params.iter().map(|(k, v)| format!("{k}={v}")).collect();
            let payload = format!("system:{}:{}", button.action, encoded.join(","));
            assert!(payload.len() <= 64, "{payload} is {} bytes", payload.len());
        }
        assert_eq!(buttons[0][0].params.as_ref().unwrap()["v"], "a");
        assert_eq!(buttons[0][1].params.as_ref().unwrap()["v"], "r");
    }
}
