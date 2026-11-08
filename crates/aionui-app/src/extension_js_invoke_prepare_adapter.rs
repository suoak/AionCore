//! App-side [`CheckedAgentToolPrepare`] adapter for HostRuntime.
//!
//! Bridges extension routes (no aionui-app dependency) to
//! [`prepare_checked_agent_tool_invoke`]. On Ok → non-executing ack
//! (`handler_integrity_checked`, `dispatched=false`); source stays Host-private
//! and is dropped here (first knife: no supervisor hand-off). On Err → refuse
//! with the fixed category reason. Not execution authorization / E13 complete.

use crate::extension_js_invoke_checked::prepare_checked_agent_tool_invoke;
use aionui_extension::host_runtime::{AgentToolInvokeResult, CheckedAgentToolPrepare};
use aionui_extension::hub::signed_manifest::SignatureCheckedManifest;
use aionui_extension::types::ResolvedAgentTool;
use std::fs::File;

/// Default Host prepare adapter wired into [`aionui_extension::HostRuntime`].
#[derive(Debug, Default, Clone, Copy)]
pub struct AppCheckedAgentToolPrepare;

impl CheckedAgentToolPrepare for AppCheckedAgentToolPrepare {
    fn prepare(
        &self,
        tool: &ResolvedAgentTool,
        local_tool_id: &str,
        checked: &SignatureCheckedManifest,
        root: &File,
    ) -> AgentToolInvokeResult {
        match prepare_checked_agent_tool_invoke(checked, local_tool_id, root) {
            Ok(prepared) => {
                // Non-executing first knife: drop verified source rather than
                // handing it to the experimental supervisor without new gates.
                let _host_private = prepared.into_checked_source();
                AgentToolInvokeResult {
                    status: "accepted",
                    tool: tool.clone(),
                    dispatched: false,
                    reason: Some("handler_integrity_checked"),
                }
            }
            Err(err) => AgentToolInvokeResult {
                status: "refused",
                tool: tool.clone(),
                dispatched: false,
                reason: Some(err.reason()),
            },
        }
    }
}

#[cfg(all(test, feature = "experimental-extension-js", target_os = "linux"))]
mod enabled_tests {
    use super::*;
    use aionui_extension::host_runtime::{HostRuntime, resolve_agent_tool_invoke};
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::sync::Arc;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[23; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), signing_key().verifying_key())]),
        )
    }

    fn checked(raw: &[u8]) -> SignatureCheckedManifest {
        let integrity = sri_sha512(raw);
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        check_signed_manifest(
            &policy(),
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw,
        )
        .unwrap()
    }

    #[test]
    fn adapter_ok_is_non_executing_checked_ack() {
        let temp = tempfile::tempdir().unwrap();
        let body = "export const run = () => 1;\n";
        fs::write(temp.path().join("h.js"), body.as_bytes()).unwrap();
        let sri = sri_sha512(body.as_bytes());
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]}}}}"#
        );
        let root = File::open(temp.path()).unwrap();
        let host = HostRuntime::new().with_checked_prepare(Arc::new(AppCheckedAgentToolPrepare));
        host.signed_roots
            .install("demo.plugin", checked(raw.as_bytes()), root)
            .unwrap();
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: Some("h.js".into()),
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "accepted");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some("handler_integrity_checked"));
        assert_ne!(result.reason, Some("handler_stub"));
    }

    #[test]
    fn adapter_missing_integrity_refuses_not_stub() {
        let temp = tempfile::tempdir().unwrap();
        let raw = r#"{"name":"demo.plugin","version":"1.0.0","contributes":{"agentTools":[{"id":"search","name":"Search","handler":"h.js"}]}}"#;
        let root = File::open(temp.path()).unwrap();
        let host = HostRuntime::new().with_checked_prepare(Arc::new(AppCheckedAgentToolPrepare));
        host.signed_roots
            .install("demo.plugin", checked(raw.as_bytes()), root)
            .unwrap();
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: Some("h.js".into()),
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "refused");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some("handler_integrity_not_present"));
        assert_ne!(result.reason, Some("handler_stub"));
    }
}

#[cfg(all(test, not(all(feature = "experimental-extension-js", target_os = "linux"))))]
mod disabled_tests {
    use super::*;
    use aionui_extension::host_runtime::{HostRuntime, resolve_agent_tool_invoke};
    use aionui_extension::hub::curated::{CuratedInstallPolicy, signature_payload, sri_sha512};
    use aionui_extension::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::sync::Arc;

    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[23; 32])
    }

    #[test]
    fn adapter_with_binding_refuses_source_check_when_feature_disabled() {
        let verifying = signing_key().verifying_key();
        let policy = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::from([("fixture".into(), verifying)]));
        let body = b"export const run = () => 1;";
        let sri = sri_sha512(body);
        let raw = format!(
            r#"{{"name":"demo.plugin","version":"1.0.0","contributes":{{"agentTools":[{{"id":"search","name":"Search","handler":"h.js","handlerIntegrity":"{sri}"}}]}}}}"#
        );
        let integrity = sri_sha512(raw.as_bytes());
        let signature = hex::encode(
            signing_key()
                .sign(signature_payload("demo.plugin", "1.0.0", &integrity).as_bytes())
                .to_bytes(),
        );
        let checked = check_signed_manifest(
            &policy,
            &SignedManifestEntry {
                name: "demo.plugin",
                version: "1.0.0",
                pubkey_id: Some("fixture"),
                integrity: Some(&integrity),
                signature: Some(&signature),
            },
            raw.as_bytes(),
        )
        .unwrap();
        // tempfile as non-dir would fail install; use tempdir even when reader unavailable.
        let temp = tempfile::tempdir().unwrap();
        let root = File::open(temp.path()).unwrap();
        let host = HostRuntime::new().with_checked_prepare(Arc::new(AppCheckedAgentToolPrepare));
        host.signed_roots.install("demo.plugin", checked, root).unwrap();
        let tool = ResolvedAgentTool {
            extension_name: "demo.plugin".into(),
            id: "ext-demo.plugin-search".into(),
            name: "Search".into(),
            description: None,
            plan_safe_actions: true,
            handler: Some("h.js".into()),
        };
        let result = resolve_agent_tool_invoke(&host, tool);
        assert_eq!(result.status, "refused");
        assert!(!result.dispatched);
        assert_eq!(result.reason, Some("handler_source_check_failed"));
        assert_ne!(result.reason, Some("handler_stub"));
    }
}
