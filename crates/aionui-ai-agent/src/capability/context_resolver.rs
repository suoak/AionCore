use std::collections::{BTreeMap, BTreeSet};

use aionui_api_types::{
    ContextDiscoveryRequest, ContextResolutionOutcome, ContextResolutionRequest, ContextResolutionWarning,
    ContextSourceRecommendation,
};

use super::context_provider::{ContextProvider, ContextProviderError};

pub const MAX_CONTEXT_RECOMMENDATIONS: usize = 100;

pub async fn resolve_context_sources(
    providers: &[&dyn ContextProvider],
    request: &ContextResolutionRequest,
) -> Result<ContextResolutionOutcome, ContextProviderError> {
    validate_request(request)?;

    let mut registry = BTreeMap::new();
    for provider in providers {
        let identity = provider.identity();
        if identity.provider.trim().is_empty() || identity.instance_id.trim().is_empty() {
            return Err(ContextProviderError::Provider(
                "provider identity and instance id are required".to_owned(),
            ));
        }
        if registry
            .insert(identity.instance_id.clone(), (*provider, identity))
            .is_some()
        {
            return Err(ContextProviderError::Provider(
                "provider instance ids must be unique".to_owned(),
            ));
        }
    }

    let discovery = ContextDiscoveryRequest {
        task_id: request.task_id.clone(),
        project_id: request.project_id.clone(),
        agent_id: request.agent_id.clone(),
        workspace: request.workspace.clone(),
        hints: request.hints.clone(),
    };
    let mut recommendations = Vec::new();
    let mut warnings = Vec::new();
    let mut seen_sources = BTreeSet::new();

    for selection in &request.providers {
        let Some((provider, identity)) = registry.get(&selection.instance_id) else {
            if selection.required {
                return Err(ContextProviderError::Unavailable(format!(
                    "required context provider '{}' is not registered",
                    selection.instance_id
                )));
            }
            warnings.push(warning(&selection.instance_id, "provider_not_registered"));
            continue;
        };
        let capabilities = provider.capabilities();
        if !capabilities.permission_aware || !capabilities.provenance {
            if selection.required {
                return Err(ContextProviderError::UnsupportedCapability(
                    "permission_aware_and_provenance",
                ));
            }
            warnings.push(warning(&selection.instance_id, "provider_contract_incomplete"));
            continue;
        }

        let sources = match provider.discover(&discovery).await {
            Ok(sources) => sources,
            Err(_) if !selection.required => {
                warnings.push(warning(&selection.instance_id, "context_unavailable"));
                continue;
            }
            Err(error) => return Err(error),
        };
        for source in sources {
            if source.provider != identity.provider || source.source_id.trim().is_empty() {
                if selection.required {
                    return Err(ContextProviderError::Provider(format!(
                        "provider '{}' returned an invalid discovery source",
                        identity.instance_id
                    )));
                }
                warnings.push(warning(&selection.instance_id, "invalid_discovery_source"));
                continue;
            }
            if !seen_sources.insert((identity.instance_id.clone(), source.source_id.clone())) {
                continue;
            }
            if recommendations.len() == MAX_CONTEXT_RECOMMENDATIONS {
                warnings.push(warning("resolver", "recommendation_budget_exhausted"));
                return Ok(ContextResolutionOutcome {
                    recommendations,
                    warnings,
                });
            }
            recommendations.push(ContextSourceRecommendation {
                provider: identity.provider.clone(),
                provider_instance_id: identity.instance_id.clone(),
                source,
                requires_user_confirmation: true,
            });
        }
    }

    Ok(ContextResolutionOutcome {
        recommendations,
        warnings,
    })
}

fn validate_request(request: &ContextResolutionRequest) -> Result<(), ContextProviderError> {
    if request.task_id.trim().is_empty() {
        return Err(ContextProviderError::InvalidQuery("task_id is required".to_owned()));
    }
    if request.providers.is_empty() {
        return Err(ContextProviderError::InvalidQuery(
            "at least one context provider selection is required".to_owned(),
        ));
    }
    let mut selected = BTreeSet::new();
    for provider in &request.providers {
        if provider.instance_id.trim().is_empty() {
            return Err(ContextProviderError::InvalidQuery(
                "provider instance_id is required".to_owned(),
            ));
        }
        if !selected.insert(provider.instance_id.as_str()) {
            return Err(ContextProviderError::InvalidQuery(
                "provider selections contain duplicate instance ids".to_owned(),
            ));
        }
    }
    Ok(())
}

fn warning(provider_instance_id: &str, code: &str) -> ContextResolutionWarning {
    ContextResolutionWarning {
        provider_instance_id: provider_instance_id.to_owned(),
        code: code.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use aionui_api_types::{
        ContextAuthorization, ContextAuthorizationRequest, ContextDocument, ContextFetchRequest, ContextHit,
        ContextProvenance, ContextProviderCapabilities, ContextProviderIdentity, ContextProviderSelection,
        ContextQuery, ContextSource,
    };
    use async_trait::async_trait;

    use super::*;

    struct TestProvider {
        identity: ContextProviderIdentity,
        capabilities: ContextProviderCapabilities,
        sources: Result<Vec<ContextSource>, &'static str>,
    }

    #[async_trait]
    impl ContextProvider for TestProvider {
        fn identity(&self) -> ContextProviderIdentity {
            self.identity.clone()
        }

        fn capabilities(&self) -> ContextProviderCapabilities {
            self.capabilities
        }

        async fn discover(
            &self,
            request: &ContextDiscoveryRequest,
        ) -> Result<Vec<ContextSource>, ContextProviderError> {
            assert_eq!(request.task_id, "task-1");
            assert_eq!(request.project_id.as_deref(), Some("project-1"));
            assert_eq!(request.workspace.as_deref(), Some("/workspace"));
            assert_eq!(request.agent_id.as_deref(), Some("agent-1"));
            self.sources
                .clone()
                .map_err(|message| ContextProviderError::Unavailable(message.to_owned()))
        }

        async fn search(&self, _query: &ContextQuery) -> Result<Vec<ContextHit>, ContextProviderError> {
            unreachable!("resolver discovery must not search")
        }

        async fn fetch(&self, _request: &ContextFetchRequest) -> Result<ContextDocument, ContextProviderError> {
            unreachable!("resolver discovery must not fetch")
        }

        async fn authorize(
            &self,
            _request: &ContextAuthorizationRequest,
        ) -> Result<ContextAuthorization, ContextProviderError> {
            unreachable!("source authorization happens when a query is selected")
        }

        fn provenance(&self, _hit: &ContextHit) -> Result<ContextProvenance, ContextProviderError> {
            unreachable!("resolver discovery must not inspect hit provenance")
        }
    }

    fn provider(instance_id: &str, source_ids: &[&str]) -> TestProvider {
        TestProvider {
            identity: ContextProviderIdentity {
                provider: "test".to_owned(),
                instance_id: instance_id.to_owned(),
                display_name: instance_id.to_owned(),
            },
            capabilities: ContextProviderCapabilities {
                single_scope: true,
                multi_scope: true,
                all_accessible: false,
                provenance: true,
                permission_aware: true,
                freshness: false,
                content_fetch: false,
            },
            sources: Ok(source_ids
                .iter()
                .map(|source_id| ContextSource {
                    provider: "test".to_owned(),
                    source_id: (*source_id).to_owned(),
                    kind: "space".to_owned(),
                    display_name: (*source_id).to_owned(),
                    parent_source_id: None,
                    metadata: BTreeMap::new(),
                })
                .collect()),
        }
    }

    fn request(selections: Vec<ContextProviderSelection>) -> ContextResolutionRequest {
        ContextResolutionRequest {
            task_id: "task-1".to_owned(),
            project_id: Some("project-1".to_owned()),
            workspace: Some("/workspace".to_owned()),
            agent_id: Some("agent-1".to_owned()),
            providers: selections,
            hints: BTreeMap::new(),
        }
    }

    #[tokio::test]
    async fn resolves_explicit_providers_in_selection_order_and_deduplicates_sources() {
        let alpha = provider("alpha", &["space-1", "space-1"]);
        let beta = provider("beta", &["space-2"]);
        let outcome = resolve_context_sources(
            &[&beta, &alpha],
            &request(vec![
                ContextProviderSelection {
                    instance_id: "alpha".to_owned(),
                    required: true,
                },
                ContextProviderSelection {
                    instance_id: "beta".to_owned(),
                    required: false,
                },
            ]),
        )
        .await
        .unwrap();

        assert_eq!(
            outcome
                .recommendations
                .iter()
                .map(|item| item.source.source_id.as_str())
                .collect::<Vec<_>>(),
            ["space-1", "space-2"]
        );
        assert!(
            outcome
                .recommendations
                .iter()
                .all(|item| item.requires_user_confirmation)
        );
        assert!(outcome.warnings.is_empty());
    }

    #[tokio::test]
    async fn optional_provider_failure_warns_without_hiding_required_results() {
        let required = provider("required", &["space-1"]);
        let optional = TestProvider {
            sources: Err("offline"),
            ..provider("optional", &[])
        };
        let outcome = resolve_context_sources(
            &[&required, &optional],
            &request(vec![
                ContextProviderSelection {
                    instance_id: "required".to_owned(),
                    required: true,
                },
                ContextProviderSelection {
                    instance_id: "optional".to_owned(),
                    required: false,
                },
            ]),
        )
        .await
        .unwrap();

        assert_eq!(outcome.recommendations.len(), 1);
        assert_eq!(outcome.warnings[0].code, "context_unavailable");
    }

    #[tokio::test]
    async fn missing_required_provider_fails_closed() {
        let error = resolve_context_sources(
            &[],
            &request(vec![ContextProviderSelection {
                instance_id: "required".to_owned(),
                required: true,
            }]),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ContextProviderError::Unavailable(message) if message.contains("required")));
    }
}
