use std::collections::BTreeSet;

use aionui_api_types::{
    ContextAuthorization, ContextAuthorizationRequest, ContextDiscoveryRequest, ContextDocument, ContextFetchRequest,
    ContextHit, ContextProvenance, ContextProviderCapabilities, ContextProviderIdentity, ContextQuery, ContextScope,
    ContextSource,
};
use async_trait::async_trait;
use thiserror::Error;

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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use aionui_api_types::{ContextPurpose, ContextResourceRef};

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

    struct TestProvider;

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
            Ok(Vec::new())
        }

        async fn fetch(&self, _request: &ContextFetchRequest) -> Result<ContextDocument, ContextProviderError> {
            Err(ContextProviderError::UnsupportedCapability("content_fetch"))
        }

        async fn authorize(
            &self,
            _request: &ContextAuthorizationRequest,
        ) -> Result<ContextAuthorization, ContextProviderError> {
            Ok(ContextAuthorization {
                allowed: true,
                permission_scope: Some("read".to_owned()),
                rule_id: "test.read".to_owned(),
                reason: "test authorization".to_owned(),
            })
        }
    }

    #[test]
    fn provenance_rejects_a_hit_from_another_provider() {
        let provider = TestProvider;
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
