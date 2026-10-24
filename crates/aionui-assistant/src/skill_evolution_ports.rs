//! Ports for Skill Evolution Phase 2 (trajectory / LLM / apply / pin).
//!
//! Implementations live in `aionui-app` so this crate stays free of conversation
//! and provider wiring. Experience articles MUST NOT be injected into normal
//! Inference Agent chat prompts — these ports are evolve/apply only.

use async_trait::async_trait;

use crate::error::AssistantError;

#[derive(Debug, Clone, Default)]
pub struct TrajectoryDigest {
    pub turns: u64,
    pub steps: u64,
    pub tools: u64,
    pub errors: u64,
    pub record_count: usize,
    pub digest_md: String,
    pub conversation_name: Option<String>,
    pub workspace: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SkillWriteOutcome {
    pub skills_hub_path: Option<String>,
    pub workspace_skill_path: Option<String>,
    pub skill_key: String,
}

#[async_trait]
pub trait SkillEvolutionTrajectoryPort: Send + Sync {
    async fn load_digest(&self, user_id: &str, conversation_id: &str) -> Result<TrajectoryDigest, AssistantError>;
}

#[async_trait]
pub trait SkillEvolutionLlmPort: Send + Sync {
    /// Returns `(completion_text, model_id_used)`.
    async fn complete(
        &self,
        user_id: &str,
        system: &str,
        user: &str,
        model_hint: Option<&str>,
    ) -> Result<(String, String), AssistantError>;
}

#[async_trait]
pub trait SkillEvolutionApplyPort: Send + Sync {
    async fn write_skill(
        &self,
        user_id: &str,
        skill_key: &str,
        skill_md: &str,
        workspace_root: Option<&str>,
        write_to_skills_hub: bool,
    ) -> Result<SkillWriteOutcome, AssistantError>;

    /// Whether the existing skill `skill_key` ships scripts (M8 E10, D10-7).
    /// Evolution never auto-applies to such a skill. The default answers
    /// `true` so an adapter that cannot tell keeps auto-apply off.
    async fn target_has_scripts(&self, _user_id: &str, _skill_key: &str) -> bool {
        true
    }
}

#[async_trait]
pub trait SkillEvolutionPinPort: Send + Sync {
    async fn pin_skill(
        &self,
        user_id: &str,
        assistant_id: &str,
        skill_key: &str,
        version: &str,
    ) -> Result<(), AssistantError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct WriteOnly;

    #[async_trait]
    impl SkillEvolutionApplyPort for WriteOnly {
        async fn write_skill(
            &self,
            _user_id: &str,
            skill_key: &str,
            _skill_md: &str,
            _workspace_root: Option<&str>,
            _write_to_skills_hub: bool,
        ) -> Result<SkillWriteOutcome, AssistantError> {
            Ok(SkillWriteOutcome {
                skills_hub_path: None,
                workspace_skill_path: None,
                skill_key: skill_key.into(),
            })
        }
    }

    #[tokio::test]
    async fn unknown_script_state_keeps_auto_apply_off() {
        // D10-7: an adapter that cannot inspect the skill must not unlock auto-apply.
        assert!(WriteOnly.target_has_scripts("u", "k").await);
    }
}
