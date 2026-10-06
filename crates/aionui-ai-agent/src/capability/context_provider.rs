use std::collections::BTreeSet;

use aionui_api_types::{
    ContextAuthorization, ContextAuthorizationRequest, ContextBudget, ContextDiscoveryRequest, ContextDocument,
    ContextFetchRequest, ContextHit, ContextProvenance, ContextProviderCapabilities, ContextProviderIdentity,
    ContextQuery, ContextScope, ContextSource, PolicyDecisionKind,
};
use async_trait::async_trait;
use thiserror::Error;

use super::planning_policy::{PlanningPolicy, PolicyContext, ToolPolicyRequest, classify_context_capability};

pub const MAX_CONTEXT_QUERY_HITS: usize = 100;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContextProviderError {
    #[error("invalid context query: {0}")]
    InvalidQuery(String),
    #[error("context provider does not support {0}")]
    UnsupportedCapability(&'static str),
    #[error("context access was denied: {0}")]
    Unauthorized(String),
    #[error("context provider is unavailable: {0}")]
    Unavailable(String),
    #[error("context provider failed: {0}")]
    Provider(String),
}

#[async_trait]
pub trait ContextProvider: Send + Sync {
    fn identity(&self) -> ContextProviderIdentity;

    fn capabilities(&self) -> ContextProviderCapabilities;

    async fn discover(&self, request: &ContextDiscoveryRequest) -> Result<Vec<ContextSource>, ContextProviderError>;

    async fn search(&self, query: &ContextQuery) -> Result<Vec<ContextHit>, ContextProviderError>;

    async fn fetch(&self, request: &ContextFetchRequest) -> Result<ContextDocument, ContextProviderError>;

    async fn authorize(
        &self,
        request: &ContextAuthorizationRequest,
    ) -> Result<ContextAuthorization, ContextProviderError>;

    fn provenance(&self, hit: &ContextHit) -> Result<ContextProvenance, ContextProviderError> {
        let identity = self.identity();
        if hit.provider != identity.provider || hit.provenance.provider != identity.provider {
            return Err(ContextProviderError::Provider(
                "hit provenance does not belong to this provider".to_owned(),
            ));
        }
        if hit.source_id != hit.provenance.source_id {
            return Err(ContextProviderError::Provider(
                "hit source id does not match its provenance".to_owned(),
            ));
        }
        Ok(hit.provenance.clone())
    }
}

pub fn validate_context_query(
    capabilities: ContextProviderCapabilities,
    query: &ContextQuery,
) -> Result<(), ContextProviderError> {
    if query.task_id.trim().is_empty() {
        return Err(ContextProviderError::InvalidQuery("task_id is required".to_owned()));
    }
    if query.query.trim().is_empty() {
        return Err(ContextProviderError::InvalidQuery("query is required".to_owned()));
    }
    if query.limit == 0 || query.limit > MAX_CONTEXT_QUERY_HITS {
        return Err(ContextProviderError::InvalidQuery(format!(
            "limit must be between 1 and {MAX_CONTEXT_QUERY_HITS}"
        )));
    }

    match &query.scope {
        ContextScope::SingleScope { scope_id } => {
            if !capabilities.single_scope {
                return Err(ContextProviderError::UnsupportedCapability("single_scope"));
            }
            if scope_id.trim().is_empty() {
                return Err(ContextProviderError::InvalidQuery("scope_id is required".to_owned()));
            }
        }
        ContextScope::MultiScope { scope_ids } => {
            if !capabilities.multi_scope {
                return Err(ContextProviderError::UnsupportedCapability("multi_scope"));
            }
            if scope_ids.is_empty() || scope_ids.iter().any(|scope_id| scope_id.trim().is_empty()) {
                return Err(ContextProviderError::InvalidQuery(
                    "multi_scope requires non-empty scope ids".to_owned(),
                ));
            }
            let unique: BTreeSet<_> = scope_ids.iter().collect();
            if unique.len() != scope_ids.len() {
                return Err(ContextProviderError::InvalidQuery(
                    "multi_scope contains duplicate scope ids".to_owned(),
                ));
            }
        }
        ContextScope::AllAccessible => {
            if !capabilities.all_accessible {
                return Err(ContextProviderError::UnsupportedCapability("all_accessible"));
            }
        }
    }

    if !capabilities.permission_aware {
        return Err(ContextProviderError::UnsupportedCapability("permission_aware"));
    }
    if !capabilities.provenance {
        return Err(ContextProviderError::UnsupportedCapability("provenance"));
    }
    Ok(())
}

pub async fn search_context(
    provider: &dyn ContextProvider,
    query: &ContextQuery,
    budget: ContextBudget,
) -> Result<Vec<ContextHit>, ContextProviderError> {
    let capabilities = provider.capabilities();
    validate_context_query(capabilities, query)?;
    validate_context_budget(budget)?;

    let identity = provider.identity();
    if identity.provider.trim().is_empty() || identity.instance_id.trim().is_empty() {
        return Err(ContextProviderError::Provider(
            "provider identity and instance id are required".to_owned(),
        ));
    }

    let mut accepted = Vec::new();
    let mut seen = BTreeSet::new();
    let mut remaining_chars = budget.max_total_snippet_chars;
    let max_hits = budget.max_hits.min(query.limit);

    for mut hit in provider.search(query).await? {
        validate_context_hit(provider, &identity, &hit)?;
        let authorization = provider
            .authorize(&ContextAuthorizationRequest {
                task_id: query.task_id.clone(),
                project_id: query.project_id.clone(),
                source_id: hit.source_id.clone(),
                scope: query.scope.clone(),
                purpose: query.purpose,
            })
            .await?;
        if !authorization.allowed {
            return Err(ContextProviderError::Unauthorized(format!(
                "provider returned source '{}' without authorization ({})",
                hit.source_id, authorization.rule_id
            )));
        }
        if authorization.permission_scope.as_deref() != Some(hit.permission_scope.as_str()) {
            return Err(ContextProviderError::Unauthorized(format!(
                "authorization scope for source '{}' does not match the result",
                hit.source_id
            )));
        }

        let dedup_key = context_hit_dedup_key(&hit);
        if !seen.insert(dedup_key) {
            continue;
        }
        if accepted.len() == max_hits || remaining_chars == 0 {
            break;
        }
        let snippet_limit = budget.max_snippet_chars.min(remaining_chars);
        hit.snippet = hit.snippet.chars().take(snippet_limit).collect();
        remaining_chars = remaining_chars.saturating_sub(hit.snippet.chars().count());
        accepted.push(hit);
    }
    Ok(accepted)
}

pub async fn search_context_with_policy(
    policy: &PlanningPolicy,
    policy_context: &PolicyContext,
    provider: &dyn ContextProvider,
    trusted_provider: bool,
    query: &ContextQuery,
    budget: ContextBudget,
) -> Result<Vec<ContextHit>, ContextProviderError> {
    let identity = provider.identity();
    let capability = classify_context_capability(trusted_provider, provider.capabilities());
    let decision = policy.evaluate(
        policy_context,
        &ToolPolicyRequest {
            capability,
            resource: Some(identity.instance_id),
            metadata: [("tool".to_owned(), format!("{}.search", identity.provider))]
                .into_iter()
                .collect(),
        },
    );
    if decision.decision != PolicyDecisionKind::Allow {
        return Err(ContextProviderError::Unauthorized(format!(
            "context query denied by policy rule {}: {}",
            decision.rule_id, decision.reason
        )));
    }
    search_context(provider, query, budget).await
}

fn validate_context_budget(budget: ContextBudget) -> Result<(), ContextProviderError> {
    if budget.max_hits == 0 || budget.max_hits > MAX_CONTEXT_QUERY_HITS {
        return Err(ContextProviderError::InvalidQuery(format!(
            "context budget max_hits must be between 1 and {MAX_CONTEXT_QUERY_HITS}"
        )));
    }
    if budget.max_snippet_chars == 0 || budget.max_total_snippet_chars == 0 {
        return Err(ContextProviderError::InvalidQuery(
            "context snippet budgets must be greater than zero".to_owned(),
        ));
    }
    Ok(())
}

fn validate_context_hit(
    provider: &dyn ContextProvider,
    identity: &ContextProviderIdentity,
    hit: &ContextHit,
) -> Result<(), ContextProviderError> {
    if hit.source_id.trim().is_empty() || hit.title.trim().is_empty() || hit.permission_scope.trim().is_empty() {
        return Err(ContextProviderError::Provider(
            "context hits require source_id, title, and permission_scope".to_owned(),
        ));
    }
    if hit.provider != identity.provider {
        return Err(ContextProviderError::Provider(format!(
            "source '{}' belongs to unexpected provider '{}'",
            hit.source_id, hit.provider
        )));
    }
    provider.provenance(hit)?;
    Ok(())
}

fn context_hit_dedup_key(hit: &ContextHit) -> String {
    if let Some(document) = &hit.provenance.document {
        return format!("{}:document:{}", hit.provider, document.id);
    }
    if let Some(content_hash) = &hit.content_hash {
        return format!("{}:content_hash:{content_hash}", hit.provider);
    }
    format!("{}:source:{}", hit.provider, hit.source_id)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aionui_api_types::{AgentIntegrationMode, ContextPurpose, ContextResourceRef, TaskSessionMode};

    use super::*;

    fn query(scope: ContextScope) -> ContextQuery {
        ContextQuery {
            task_id: "task-1".to_owned(),
            project_id: Some("project-1".to_owned()),
            query: "release requirements".to_owned(),
            scope,
            limit: 10,
            filters: BTreeMap::new(),
            purpose: ContextPurpose::Planning,
        }
    }

    fn capabilities() -> ContextProviderCapabilities {
        ContextProviderCapabilities {
            single_scope: true,
            multi_scope: true,
            all_accessible: false,
            provenance: true,
            permission_aware: true,
            freshness: false,
            content_fetch: false,
        }
    }

    fn policy_context() -> PolicyContext {
        PolicyContext {
            task_id: "task-1".to_owned(),
            project_id: Some("project-1".to_owned()),
            task_mode: TaskSessionMode::Plan,
            agent_id: "agent-1".to_owned(),
            integration_mode: AgentIntegrationMode::InProcessToolRegistry,
        }
    }

    #[test]
    fn rejects_scope_the_provider_does_not_support() {
        let error = validate_context_query(capabilities(), &query(ContextScope::AllAccessible)).unwrap_err();
        assert_eq!(error, ContextProviderError::UnsupportedCapability("all_accessible"));
    }

    #[test]
    fn rejects_duplicate_multi_scope_ids() {
        let error = validate_context_query(
            capabilities(),
            &query(ContextScope::MultiScope {
                scope_ids: vec!["space-1".to_owned(), "space-1".to_owned()],
            }),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ContextProviderError::InvalidQuery("multi_scope contains duplicate scope ids".to_owned())
        );
    }

    #[test]
    fn accepts_a_permission_aware_provenance_query() {
        validate_context_query(
            capabilities(),
            &query(ContextScope::MultiScope {
                scope_ids: vec!["space-1".to_owned(), "space-2".to_owned()],
            }),
        )
        .unwrap();
    }

    #[derive(Default)]
    struct TestProvider {
        hits: Vec<ContextHit>,
        allowed: bool,
    }

    #[async_trait]
    impl ContextProvider for TestProvider {
        fn identity(&self) -> ContextProviderIdentity {
            ContextProviderIdentity {
                provider: "test".to_owned(),
                instance_id: "test-instance".to_owned(),
                display_name: "Test".to_owned(),
            }
        }

        fn capabilities(&self) -> ContextProviderCapabilities {
            capabilities()
        }

        async fn discover(
            &self,
            _request: &ContextDiscoveryRequest,
        ) -> Result<Vec<ContextSource>, ContextProviderError> {
            Ok(Vec::new())
        }

        async fn search(&self, _query: &ContextQuery) -> Result<Vec<ContextHit>, ContextProviderError> {
            Ok(self.hits.clone())
        }

        async fn fetch(&self, _request: &ContextFetchRequest) -> Result<ContextDocument, ContextProviderError> {
            Err(ContextProviderError::UnsupportedCapability("content_fetch"))
        }

        async fn authorize(
            &self,
            _request: &ContextAuthorizationRequest,
        ) -> Result<ContextAuthorization, ContextProviderError> {
            Ok(ContextAuthorization {
                allowed: self.allowed,
                permission_scope: self.allowed.then(|| "read".to_owned()),
                rule_id: "test.read".to_owned(),
                reason: "test authorization".to_owned(),
            })
        }
    }

    fn hit(source_id: &str, document_id: &str, snippet: &str) -> ContextHit {
        ContextHit {
            provider: "test".to_owned(),
            source_id: source_id.to_owned(),
            title: format!("Document {document_id}"),
            snippet: snippet.to_owned(),
            score: Some(1.0),
            permission_scope: "read".to_owned(),
            version_or_updated_at: None,
            retrieved_at: 1,
            content_hash: None,
            provenance: ContextProvenance {
                provider: "test".to_owned(),
                source_id: source_id.to_owned(),
                tenant: None,
                space: Some(ContextResourceRef {
                    id: "space-1".to_owned(),
                    name: None,
                }),
                knowledge_base: None,
                document: Some(ContextResourceRef {
                    id: document_id.to_owned(),
                    name: None,
                }),
                provider_data: BTreeMap::new(),
            },
        }
    }

    #[tokio::test]
    async fn authorized_search_deduplicates_canonical_documents_and_applies_budget() {
        let provider = TestProvider {
            hits: vec![
                hit("space-1/doc-1", "doc-1", "123456"),
                hit("space-2/doc-1", "doc-1", "duplicate"),
                hit("space-1/doc-2", "doc-2", "abcdef"),
            ],
            allowed: true,
        };

        let hits = search_context(
            &provider,
            &query(ContextScope::MultiScope {
                scope_ids: vec!["space-1".to_owned(), "space-2".to_owned()],
            }),
            ContextBudget {
                max_hits: 3,
                max_snippet_chars: 4,
                max_total_snippet_chars: 6,
            },
        )
        .await
        .unwrap();

        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].source_id, "space-1/doc-1");
        assert_eq!(hits[0].snippet, "1234");
        assert_eq!(hits[1].snippet, "ab");
    }

    #[tokio::test]
    async fn search_fails_closed_when_provider_returns_an_unauthorized_hit() {
        let provider = TestProvider {
            hits: vec![hit("space-3/doc-1", "doc-1", "secret")],
            allowed: false,
        };
        let error = search_context(
            &provider,
            &query(ContextScope::SingleScope {
                scope_id: "space-3".to_owned(),
            }),
            ContextBudget::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ContextProviderError::Unauthorized(_)));
    }

    #[tokio::test]
    async fn planning_search_requires_the_trusted_knowledge_read_policy_path() {
        let provider = TestProvider {
            hits: vec![hit("space-1/doc-1", "doc-1", "allowed")],
            allowed: true,
        };
        let query = query(ContextScope::SingleScope {
            scope_id: "space-1".to_owned(),
        });
        assert_eq!(
            search_context_with_policy(
                &PlanningPolicy::default(),
                &policy_context(),
                &provider,
                true,
                &query,
                ContextBudget::default(),
            )
            .await
            .unwrap()
            .len(),
            1
        );
        assert!(matches!(
            search_context_with_policy(
                &PlanningPolicy::default(),
                &policy_context(),
                &provider,
                false,
                &query,
                ContextBudget::default(),
            )
            .await,
            Err(ContextProviderError::Unauthorized(message)) if message.contains("planning.unknown.deny")
        ));
    }

    #[test]
    fn provenance_rejects_a_hit_from_another_provider() {
        let provider = TestProvider::default();
        let provenance = ContextProvenance {
            provider: "other".to_owned(),
            source_id: "doc-1".to_owned(),
            tenant: None,
            space: Some(ContextResourceRef {
                id: "space-1".to_owned(),
                name: None,
            }),
            knowledge_base: None,
            document: None,
            provider_data: BTreeMap::new(),
        };
        let hit = ContextHit {
            provider: "other".to_owned(),
            source_id: "doc-1".to_owned(),
            title: "Document".to_owned(),
            snippet: "Snippet".to_owned(),
            score: Some(1.0),
            permission_scope: "read".to_owned(),
            version_or_updated_at: None,
            retrieved_at: 1,
            content_hash: None,
            provenance,
        };

        assert!(matches!(
            provider.provenance(&hit),
            Err(ContextProviderError::Provider(message)) if message.contains("does not belong")
        ));
    }
}
