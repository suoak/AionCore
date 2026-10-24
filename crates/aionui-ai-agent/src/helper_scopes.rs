//! Skill → helper-token scope mapping (B-15, M8 E12-1).
//!
//! Every conversation runtime gets a `ConversationHelper` token so the
//! in-conversation helper CLI can authenticate. The base scope keeps the
//! read-only / own-conversation surface. The write-capable helper surfaces are
//! each owned by one auto-inject skill and are granted only when that skill is
//! actually installed in the session snapshot (`conversation.extra.skills`):
//!
//! | skill                 | scope                       |
//! |-----------------------|-----------------------------|
//! | `aionui-config`       | `HelperConfig`              |
//! | `cron`                | `HelperCron`                |
//! | `conversation-create` | `HelperConversationCreate`  |
//! | `session-message`     | `HelperSessionMessage`      |
//!
//! A session without a skill snapshot (legacy rows predating
//! `extra.skills`) keeps every helper scope, so its behavior is unchanged.

use crate::runtime_token::RuntimeTokenScope;

/// `(skill name, scope)` pairs. The skill names are the auto-inject builtin
/// skill directory names under `assets/builtin-skills/auto-inject/`.
pub const HELPER_SKILL_SCOPE_MAP: [(&str, RuntimeTokenScope); 4] = [
    ("aionui-config", RuntimeTokenScope::HelperConfig),
    ("cron", RuntimeTokenScope::HelperCron),
    ("conversation-create", RuntimeTokenScope::HelperConversationCreate),
    ("session-message", RuntimeTokenScope::HelperSessionMessage),
];

/// Skill that grants `scope`, for 403 messages.
pub fn skill_for_helper_scope(scope: RuntimeTokenScope) -> Option<&'static str> {
    HELPER_SKILL_SCOPE_MAP
        .iter()
        .find(|(_, s)| *s == scope)
        .map(|(name, _)| *name)
}

/// Conversation-helper token scopes for a session.
///
/// `skills` is the session's skill snapshot; `None` means the conversation has
/// no snapshot (legacy row) and keeps every helper scope.
pub fn conversation_helper_scopes(skills: Option<&[String]>) -> Vec<RuntimeTokenScope> {
    let mut scopes = vec![RuntimeTokenScope::ConversationHelper];
    match skills {
        None => scopes.extend(RuntimeTokenScope::HELPER_SKILL_SCOPES),
        Some(skills) => scopes.extend(
            HELPER_SKILL_SCOPE_MAP
                .iter()
                .filter(|(name, _)| skills.iter().any(|skill| skill == name))
                .map(|(_, scope)| *scope),
        ),
    }
    scopes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn legacy_session_without_snapshot_keeps_every_helper_scope() {
        let scopes = conversation_helper_scopes(None);
        assert!(scopes.contains(&RuntimeTokenScope::ConversationHelper));
        for scope in RuntimeTokenScope::HELPER_SKILL_SCOPES {
            assert!(scopes.contains(&scope), "missing {scope:?}");
        }
    }

    #[test]
    fn full_auto_inject_snapshot_keeps_every_helper_scope() {
        let skills = names(&[
            "aionui-config",
            "conversation-create",
            "cron",
            "officecli",
            "review-notes",
            "session-message",
            "skill-creator",
        ]);
        let scopes = conversation_helper_scopes(Some(&skills));
        assert_eq!(scopes.len(), 1 + RuntimeTokenScope::HELPER_SKILL_SCOPES.len());
    }

    #[test]
    fn excluded_skill_drops_only_its_scope() {
        let skills = names(&["aionui-config", "conversation-create", "session-message"]);
        let scopes = conversation_helper_scopes(Some(&skills));
        assert!(!scopes.contains(&RuntimeTokenScope::HelperCron));
        assert!(scopes.contains(&RuntimeTokenScope::HelperConfig));
        assert!(scopes.contains(&RuntimeTokenScope::HelperConversationCreate));
        assert!(scopes.contains(&RuntimeTokenScope::HelperSessionMessage));
    }

    #[test]
    fn empty_snapshot_keeps_only_base_scope() {
        let scopes = conversation_helper_scopes(Some(&[]));
        assert_eq!(scopes, vec![RuntimeTokenScope::ConversationHelper]);
    }

    #[test]
    fn every_scope_maps_back_to_a_skill() {
        for scope in RuntimeTokenScope::HELPER_SKILL_SCOPES {
            assert!(skill_for_helper_scope(scope).is_some());
        }
        assert_eq!(skill_for_helper_scope(RuntimeTokenScope::ConversationHelper), None);
    }
}
