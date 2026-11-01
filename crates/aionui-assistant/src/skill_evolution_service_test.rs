//! M8 E10-6: E4/E6 review evidence feeds evolution, and a proposal built on
//! it always waits for a human, even under `auto_apply_on_pass`.

use std::sync::{Arc, Mutex};

use aionui_api_types::{EvolveSkillEvolutionRequest, SkillEvolutionStatus};
use aionui_db::{
    EvolutionEvidence, EvolutionSignal, SqliteConversationRepository, SqliteExperienceArticleRepository,
    SqliteSkillEvolutionProposalRepository, SqliteSkillEvolutionSettingsRepository, UpsertSkillEvolutionSettingsParams,
    init_database_memory, record_evolution_evidence,
};
use async_trait::async_trait;

use super::*;
use crate::skill_evolution_ports::{
    SkillEvolutionApplyPort, SkillEvolutionLlmPort, SkillWriteOutcome, TrajectoryDigest,
};
use crate::skill_evolution_prompts::MAINTAINER_SYSTEM;

const GOOD_DRAFT: &str = "---\nname: evolved-demo\ndescription: demo skill\nversion: 0.1.0\n---\n\n# Demo\n\n## 使用指引\n\n1. 确认适用范围。\n2. 在智能体中心试跑。\n3. 发布时 pin。\n";

/// Maintainer returns a pattern; Proposer returns a gate-passing draft and
/// the prompt it saw is kept for assertions.
#[derive(Default)]
struct FakeLlm {
    proposer_prompts: Mutex<Vec<String>>,
}

#[async_trait]
impl SkillEvolutionLlmPort for FakeLlm {
    async fn complete(
        &self,
        _user_id: &str,
        system: &str,
        user: &str,
        _model_hint: Option<&str>,
    ) -> Result<(String, String), AssistantError> {
        if system == MAINTAINER_SYSTEM {
            return Ok(("# 演示模式\n\n先写测试再改代码。".into(), "fake".into()));
        }
        self.proposer_prompts.lock().unwrap().push(user.to_owned());
        let json = serde_json::json!({
            "title": "Demo",
            "target_skill_key": "evolved-demo",
            "action": "create",
            "experience_summary": "summary",
            "draft_skill_md": GOOD_DRAFT,
        });
        Ok((json.to_string(), "fake".into()))
    }
}

/// A script-free target, so only the evidence rule can stop auto-apply.
struct NoScripts;

#[async_trait]
impl SkillEvolutionApplyPort for NoScripts {
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

    async fn target_has_scripts(&self, _user_id: &str, _skill_key: &str) -> bool {
        false
    }
}

fn auto_apply_request() -> EvolveSkillEvolutionRequest {
    EvolveSkillEvolutionRequest {
        gate_mode: Some("auto_apply_on_pass".into()),
        try_run_ok: Some(true),
        ..Default::default()
    }
}

async fn service(db: &aionui_db::Database, llm: Arc<FakeLlm>) -> SkillEvolutionService {
    let pool = db.pool().clone();
    // The user explicitly opted into auto-apply with a reachable threshold.
    let settings = SqliteSkillEvolutionSettingsRepository::new(pool.clone());
    settings
        .upsert(&UpsertSkillEvolutionSettingsParams {
            user_id: "u1",
            gate_mode: "auto_apply_on_pass",
            assist_threshold: 70,
            auto_threshold: 80,
            default_experience_visibility: "private",
        })
        .await
        .unwrap();
    SkillEvolutionService::new(
        Arc::new(SqliteSkillEvolutionProposalRepository::new(pool.clone())),
        Arc::new(SqliteExperienceArticleRepository::new(pool.clone())),
        Arc::new(SqliteConversationRepository::new(pool)),
    )
    .with_settings_and_teams(Some(Arc::new(settings)), None)
    .with_ports(None, Some(llm), Some(Arc::new(NoScripts)), None)
}

#[tokio::test]
async fn review_evidence_is_linked_and_keeps_the_proposal_in_human_review() {
    let db = init_database_memory().await.unwrap();
    let llm = Arc::new(FakeLlm::default());
    let svc = service(&db, llm.clone()).await;
    let repo = SqliteExperienceArticleRepository::new(db.pool().clone());
    let returned = record_evolution_evidence(
        &repo,
        &EvolutionEvidence {
            owner_user_id: "u1",
            signal: EvolutionSignal::WorkItemReturned,
            ref_id: "ts-1",
            conversation_id: Some("conv-a"),
            title: "工作项被退回 · Fix login",
            body_md: "退回意见: missing tests",
        },
    )
    .await
    .unwrap();
    // Evidence from another conversation is not pulled in.
    record_evolution_evidence(
        &repo,
        &EvolutionEvidence {
            owner_user_id: "u1",
            signal: EvolutionSignal::AnnotationRejected,
            ref_id: "rt-9",
            conversation_id: Some("conv-b"),
            title: "批注被拒绝 · other",
            body_md: "unrelated",
        },
    )
    .await
    .unwrap();

    let out = svc
        .run_evolve(
            "u1",
            Some("conv-a"),
            None,
            TrajectoryDigest::default(),
            auto_apply_request(),
        )
        .await
        .unwrap();

    assert_eq!(
        out.proposal.status,
        SkillEvolutionStatus::PendingReview,
        "{:?}",
        out.gate_note
    );
    assert!(out.gate_note.as_deref().unwrap_or("").contains("人工审阅证据"));
    assert!(out.proposal.experience_article_ids.contains(&returned.id));
    assert!(out.experience_articles.iter().any(|a| a.id == returned.id));
    let prompt = llm.proposer_prompts.lock().unwrap().pop().unwrap();
    assert!(prompt.contains("missing tests"));
    assert!(!prompt.contains("unrelated"));
}

#[tokio::test]
async fn without_review_evidence_auto_apply_still_runs() {
    // Control for the test above: same gate, no evidence → auto-applied.
    let db = init_database_memory().await.unwrap();
    let svc = service(&db, Arc::new(FakeLlm::default())).await;
    let out = svc
        .run_evolve(
            "u1",
            Some("conv-c"),
            None,
            TrajectoryDigest::default(),
            auto_apply_request(),
        )
        .await
        .unwrap();
    assert_eq!(
        out.proposal.status,
        SkillEvolutionStatus::Applied,
        "{:?}",
        out.gate_note
    );
}
