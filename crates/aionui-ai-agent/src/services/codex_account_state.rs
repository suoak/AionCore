//! Pure state machine for Codex managed ChatGPT authentication.
//!
//! Protocol evidence: Codex CLI 0.160.1 machine-generated schemas at
//! `~/aion/protocols/samples/codex-cli/0.160.1/schema-full/v2/`.

use aionui_api_types::{CodexAccountSnapshot, CodexAuthState, CodexLoginAttempt, CodexLoginAttemptState};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub(crate) struct CodexAccountStateMachine {
    snapshot: CodexAccountSnapshot,
    identity: Option<String>,
}

impl CodexAccountStateMachine {
    pub(crate) fn snapshot(&self) -> CodexAccountSnapshot {
        self.snapshot.clone()
    }

    pub(crate) fn begin_login(&mut self, login_id: String, now_ms: i64) {
        self.snapshot.auth_state = CodexAuthState::Authenticating;
        self.snapshot.active_login = Some(CodexLoginAttempt {
            login_id,
            started_at: now_ms,
            state: CodexLoginAttemptState::Waiting,
        });
        self.touch(now_ms);
    }

    pub(crate) fn begin_cancel(&mut self, login_id: &str, now_ms: i64) -> bool {
        let Some(attempt) = self.snapshot.active_login.as_mut() else {
            return false;
        };
        if attempt.login_id != login_id {
            return false;
        }
        attempt.state = CodexLoginAttemptState::Cancelling;
        self.touch(now_ms);
        true
    }

    /// Returns true only for the currently active login. A stale completion is
    /// deliberately ignored and cannot mutate a newer attempt.
    pub(crate) fn complete_login(&mut self, login_id: Option<&str>, success: bool, now_ms: i64) -> bool {
        let Some(attempt) = self.snapshot.active_login.as_mut() else {
            return false;
        };
        if login_id != Some(attempt.login_id.as_str()) {
            return false;
        }
        if !success {
            attempt.state = CodexLoginAttemptState::Failed;
            self.snapshot.auth_state = CodexAuthState::Error;
        }
        self.touch(now_ms);
        true
    }

    /// Apply the authoritative `account/read` response. Notification payloads
    /// never enter this method directly.
    pub(crate) fn apply_account_read(&mut self, response: &Value, now_ms: i64) {
        let account = response.get("account").filter(|value| !value.is_null());
        let next_identity = account.map(account_identity);
        if next_identity != self.identity {
            self.snapshot.generation = self.snapshot.generation.saturating_add(1);
            self.identity = next_identity;
        }

        self.snapshot.requires_openai_auth = response
            .get("requiresOpenaiAuth")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        self.snapshot.workspace_routing = response
            .get("workspaceRouting")
            .filter(|value| !value.is_null())
            .cloned();
        self.snapshot.active_login = None;

        match account {
            Some(account) => {
                self.snapshot.auth_state = CodexAuthState::SignedIn;
                self.snapshot.account_type = string_field(account, "type");
                self.snapshot.auth_mode = self.snapshot.account_type.clone();
                self.snapshot.email = string_field(account, "email");
                self.snapshot.plan_type = string_field(account, "planType");
            }
            None => {
                self.snapshot.auth_state = CodexAuthState::SignedOut;
                self.snapshot.auth_mode = None;
                self.snapshot.account_type = None;
                self.snapshot.email = None;
                self.snapshot.plan_type = None;
                self.snapshot.workspace_routing = None;
            }
        }
        self.touch(now_ms);
    }

    pub(crate) fn account_read_failed(&mut self, now_ms: i64) {
        self.snapshot.auth_state = CodexAuthState::Error;
        self.touch(now_ms);
    }

    fn touch(&mut self, now_ms: i64) {
        self.snapshot.updated_at = now_ms;
    }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn account_identity(account: &Value) -> String {
    // Current account/read has no public stable account-id field. Type + email
    // is the narrowest available identity; workspace routing remains optional.
    format!(
        "{}\u{1f}{}",
        account.get("type").and_then(Value::as_str).unwrap_or("unknown"),
        account.get("email").and_then(Value::as_str).unwrap_or("")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn account_read_projects_signed_out_and_signed_in() {
        let mut state = CodexAccountStateMachine::default();
        state.apply_account_read(&json!({"account": null, "requiresOpenaiAuth": true}), 1);
        assert_eq!(state.snapshot().auth_state, CodexAuthState::SignedOut);

        state.apply_account_read(
            &json!({
                "account": {"type": "chatgpt", "email": "person@example.com", "planType": "plus"},
                "requiresOpenaiAuth": true,
                "workspaceRouting": null
            }),
            2,
        );
        let snapshot = state.snapshot();
        assert_eq!(snapshot.auth_state, CodexAuthState::SignedIn);
        assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshot.generation, 1);
    }

    #[test]
    fn stale_login_completion_cannot_finish_new_attempt() {
        let mut state = CodexAccountStateMachine::default();
        state.begin_login("A".into(), 1);
        state.begin_login("B".into(), 2);
        assert!(!state.complete_login(Some("A"), true, 3));
        assert_eq!(state.snapshot().active_login.unwrap().login_id, "B");
        assert!(state.complete_login(Some("B"), true, 4));
    }

    #[test]
    fn failed_active_login_is_error_until_authoritative_refresh() {
        let mut state = CodexAccountStateMachine::default();
        state.begin_login("A".into(), 1);
        assert!(state.complete_login(Some("A"), false, 2));
        assert_eq!(state.snapshot().auth_state, CodexAuthState::Error);
        state.apply_account_read(&json!({"account": null, "requiresOpenaiAuth": true}), 3);
        assert_eq!(state.snapshot().auth_state, CodexAuthState::SignedOut);
        assert!(state.snapshot().active_login.is_none());
    }

    #[test]
    fn cancel_is_correlated_to_login_id() {
        let mut state = CodexAccountStateMachine::default();
        state.begin_login("B".into(), 1);
        assert!(!state.begin_cancel("A", 2));
        assert!(state.begin_cancel("B", 3));
        assert_eq!(
            state.snapshot().active_login.unwrap().state,
            CodexLoginAttemptState::Cancelling
        );
    }

    #[test]
    fn repeated_read_is_monotonic_and_identity_change_advances_generation() {
        let mut state = CodexAccountStateMachine::default();
        let first = json!({"account": {"type":"chatgpt", "email":"a@example.com", "planType":"plus"}});
        state.apply_account_read(&first, 10);
        state.apply_account_read(&first, 11);
        assert_eq!(state.snapshot().generation, 1);
        state.apply_account_read(
            &json!({"account": {"type":"chatgpt", "email":"b@example.com", "planType":"team"}}),
            12,
        );
        assert_eq!(state.snapshot().generation, 2);
    }

    #[test]
    fn workspace_routing_is_diagnostic_not_sign_in_gate() {
        let mut state = CodexAccountStateMachine::default();
        state.apply_account_read(
            &json!({"account": {"type":"chatgpt", "email":null, "planType":"unknown"}}),
            1,
        );
        assert_eq!(state.snapshot().auth_state, CodexAuthState::SignedIn);
        assert!(state.snapshot().workspace_routing.is_none());
    }
}
