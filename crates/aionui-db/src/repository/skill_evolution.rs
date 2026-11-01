//! Repository traits for Skill Evolution tables.

use crate::error::DbError;
use crate::models::{
    CreateExperienceArticleParams, CreateSkillEvolutionProposalParams, ExperienceArticleRow, SkillEvolutionProposalRow,
    SkillEvolutionSettingsRow, UpdateSkillEvolutionProposalParams, UpsertSkillEvolutionSettingsParams,
};

#[async_trait::async_trait]
pub trait IExperienceArticleRepository: Send + Sync {
    async fn create(&self, params: &CreateExperienceArticleParams<'_>) -> Result<ExperienceArticleRow, DbError>;
    async fn get(&self, id: &str) -> Result<Option<ExperienceArticleRow>, DbError>;
    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError>;

    /// List articles visible to a user: owned OR team-visible for given team ids.
    async fn list_visible(
        &self,
        owner_user_id: &str,
        team_ids: &[String],
        assistant_id: Option<&str>,
        visibility: Option<&str>,
        limit: i64,
    ) -> Result<Vec<ExperienceArticleRow>, DbError>;
}

#[async_trait::async_trait]
pub trait ISkillEvolutionProposalRepository: Send + Sync {
    async fn create(
        &self,
        params: &CreateSkillEvolutionProposalParams<'_>,
    ) -> Result<SkillEvolutionProposalRow, DbError>;
    async fn get(&self, id: &str) -> Result<Option<SkillEvolutionProposalRow>, DbError>;
    async fn list_for_owner(
        &self,
        owner_user_id: &str,
        status: Option<&str>,
        assistant_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<SkillEvolutionProposalRow>, DbError>;
    async fn update(
        &self,
        id: &str,
        params: &UpdateSkillEvolutionProposalParams<'_>,
    ) -> Result<Option<SkillEvolutionProposalRow>, DbError>;
}

#[async_trait::async_trait]
pub trait ISkillEvolutionSettingsRepository: Send + Sync {
    async fn get(&self, user_id: &str) -> Result<Option<SkillEvolutionSettingsRow>, DbError>;
    async fn upsert(
        &self,
        params: &UpsertSkillEvolutionSettingsParams<'_>,
    ) -> Result<SkillEvolutionSettingsRow, DbError>;
}

/// Tag on every experience article written by an E10-6 evidence signal.
pub const EVOLUTION_EVIDENCE_TAG: &str = "evidence";

/// Body cap for one evidence article; the signal is a pointer, not a transcript.
const EVOLUTION_EVIDENCE_BODY_MAX_CHARS: usize = 4000;

/// M8 E10-6: human review signals that become skill-evolution evidence.
///
/// Evidence only feeds the Maintainer/Proposer side of skill evolution. It is
/// never injected into daily chat, and a proposal that cites it still waits
/// for a human approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvolutionSignal {
    /// E4: a diff annotation that went to the agent was resolved by the user.
    AnnotationAccepted,
    /// E4: the user reopened an annotation after it went to the agent.
    AnnotationRejected,
    /// E6: a work item in review was sent back (`review_decision` reject).
    WorkItemReturned,
}

impl EvolutionSignal {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AnnotationAccepted => "annotation_accepted",
            Self::AnnotationRejected => "annotation_rejected",
            Self::WorkItemReturned => "work_item_returned",
        }
    }

    /// Positive signals are patterns; negative ones are rejected notes, which
    /// the Proposer already reads so it does not repeat a failed approach.
    pub fn article_kind(self) -> &'static str {
        match self {
            Self::AnnotationAccepted => "pattern",
            Self::AnnotationRejected | Self::WorkItemReturned => "rejected_note",
        }
    }
}

/// One evidence record. `ref_id` is the review thread or work item id.
#[derive(Debug, Clone)]
pub struct EvolutionEvidence<'a> {
    pub owner_user_id: &'a str,
    pub signal: EvolutionSignal,
    pub ref_id: &'a str,
    pub conversation_id: Option<&'a str>,
    pub title: &'a str,
    pub body_md: &'a str,
}

/// Tags of an evidence article: `["skill-evolution","evidence","signal:…","ref:…"]`.
pub fn evolution_evidence_tags(signal: EvolutionSignal, ref_id: &str) -> Vec<String> {
    vec![
        "skill-evolution".to_owned(),
        EVOLUTION_EVIDENCE_TAG.to_owned(),
        format!("signal:{}", signal.as_str()),
        format!("ref:{ref_id}"),
    ]
}

/// Write one evidence article (private to its owner).
pub async fn record_evolution_evidence(
    repo: &dyn IExperienceArticleRepository,
    evidence: &EvolutionEvidence<'_>,
) -> Result<ExperienceArticleRow, DbError> {
    let id = aionui_common::generate_prefixed_id("ea");
    let body: String = evidence
        .body_md
        .chars()
        .take(EVOLUTION_EVIDENCE_BODY_MAX_CHARS)
        .collect();
    let sources: Vec<&str> = evidence.conversation_id.into_iter().collect();
    let sources = serde_json::to_string(&sources).map_err(|e| DbError::Init(e.to_string()))?;
    let tags = serde_json::to_string(&evolution_evidence_tags(evidence.signal, evidence.ref_id))
        .map_err(|e| DbError::Init(e.to_string()))?;
    repo.create(&CreateExperienceArticleParams {
        id: &id,
        owner_user_id: evidence.owner_user_id,
        assistant_id: None,
        team_id: None,
        kind: evidence.signal.article_kind(),
        title: evidence.title,
        body_md: &body,
        source_conversation_ids: &sources,
        tags: &tags,
        status: "active",
        visibility: "private",
    })
    .await
}
