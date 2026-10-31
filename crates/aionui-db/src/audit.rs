//! Audit ledger helpers (M8 E12-2): secret redaction, the row hash, and
//! [`AuditSink`], the handle write points use to record events.
//!
//! Redaction rule (D12-3 default): non-secret values are kept as-is; secret
//! values are replaced by `***sha256:<12 hex>` so a change can still be told
//! apart ("the key changed") without storing the key. A value counts as
//! secret when its key looks like a credential (`api_key`, `token`,
//! `password`, `authorization`, `*_encrypted`, …), when it sits inside an
//! `env` / `headers` map, or when the string itself looks like a token
//! (`sk-…`, `ghp_…`, `Bearer …`, … of token length). Numbers, booleans and null are kept.
use std::sync::Arc;

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::DbError;
use crate::models::AuditEventRow;
use crate::repository::audit_event::{IAuditEventRepository, NewAuditEvent};

/// `prev_hash` of the first row.
pub const AUDIT_GENESIS_HASH: &str = "0000000000000000000000000000000000000000000000000000000000000000";
/// Prefix of a redacted secret value.
pub const REDACTED_PREFIX: &str = "***";
/// Longest summary kept (characters); longer ones are cut with `…`.
pub const AUDIT_SUMMARY_MAX_CHARS: usize = 1000;
/// Largest redacted diff kept (bytes of JSON); larger ones are replaced by a
/// marker with the original size.
pub const AUDIT_DIFF_MAX_BYTES: usize = 64 * 1024;

/// Normalised key fragments that mark a credential.
const SECRET_KEY_MARKERS: &[&str] = &[
    "apikey",
    "secret",
    "password",
    "passwd",
    "passphrase",
    "token",
    "authorization",
    "privatekey",
    "credential",
    "cookie",
    "accesskey",
    "signature",
];
/// Keys whose whole value (a map of names to values) is treated as secret.
const SECRET_CONTAINER_KEYS: &[&str] = &["env", "headers", "httpheaders"];
/// String prefixes of well-known token formats.
const SECRET_VALUE_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xapp-",
    "wmd_",
    "AKIA",
];
/// Authorization-header schemes carrying a credential.
const SECRET_SCHEME_PREFIXES: &[&str] = &["Bearer ", "bearer "];
/// Shortest string a token prefix match applies to (keeps e.g. the locale
/// `sk-SK` readable).
const SECRET_VALUE_MIN_LEN: usize = 20;

fn normalize_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Whether a JSON key names a credential.
pub fn is_secret_key(key: &str) -> bool {
    let key = normalize_key(key);
    key.ends_with("encrypted") || SECRET_KEY_MARKERS.iter().any(|marker| key.contains(marker))
}

fn is_secret_container_key(key: &str) -> bool {
    let key = normalize_key(key);
    SECRET_CONTAINER_KEYS.contains(&key.as_str())
}

/// Whether a string looks like a token regardless of its key.
pub fn looks_like_secret_value(value: &str) -> bool {
    if SECRET_SCHEME_PREFIXES
        .iter()
        .any(|scheme| value.starts_with(scheme) && value.len() > scheme.len())
    {
        return true;
    }
    value.len() >= SECRET_VALUE_MIN_LEN
        && !value.chars().any(char::is_whitespace)
        && SECRET_VALUE_PREFIXES.iter().any(|prefix| value.starts_with(prefix))
}

/// `***sha256:<first 12 hex chars>` of `value`.
pub fn secret_fingerprint(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    format!("{REDACTED_PREFIX}sha256:{}", &hex::encode(digest)[..12])
}

fn redact_string(value: &str) -> Value {
    if value.is_empty() || value.starts_with(REDACTED_PREFIX) {
        // Empty means "no secret"; an already redacted value stays stable.
        Value::String(value.to_owned())
    } else {
        Value::String(secret_fingerprint(value))
    }
}

/// Redact every string leaf (the value sits under a secret key).
fn redact_all(value: &Value) -> Value {
    match value {
        Value::String(s) => redact_string(s),
        Value::Array(items) => Value::Array(items.iter().map(redact_all).collect()),
        Value::Object(map) => Value::Object(map.iter().map(|(k, v)| (k.clone(), redact_all(v))).collect()),
        other => other.clone(),
    }
}

/// Return `diff` with secret values replaced by fingerprints.
pub fn redact_diff(diff: &Value) -> Value {
    match diff {
        Value::String(s) if looks_like_secret_value(s) => redact_string(s),
        Value::Array(items) => Value::Array(items.iter().map(redact_diff).collect()),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, value) in map {
                let redacted = if is_secret_key(key) || is_secret_container_key(key) {
                    redact_all(value)
                } else {
                    redact_diff(value)
                };
                out.insert(key.clone(), redacted);
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// Redacted diff as stored JSON text, size-capped.
pub fn redacted_diff_text(diff: &Value) -> String {
    let text = redact_diff(diff).to_string();
    if text.len() <= AUDIT_DIFF_MAX_BYTES {
        text
    } else {
        serde_json::json!({ "truncated": true, "original_bytes": text.len() }).to_string()
    }
}

/// Summary cut to [`AUDIT_SUMMARY_MAX_CHARS`]; control characters other than
/// newline and tab become spaces.
pub fn clean_summary(summary: &str) -> String {
    let cleaned: String = summary
        .chars()
        .map(|c| {
            if c.is_control() && c != '\n' && c != '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if cleaned.chars().count() <= AUDIT_SUMMARY_MAX_CHARS {
        cleaned
    } else {
        let mut cut: String = cleaned.chars().take(AUDIT_SUMMARY_MAX_CHARS - 1).collect();
        cut.push('…');
        cut
    }
}

/// Whether `action` is a valid dotted action name: 1-128 chars of
/// `[a-z0-9_.-]`, not starting or ending with `.`.
pub fn is_valid_action(action: &str) -> bool {
    !action.is_empty()
        && action.len() <= 128
        && !action.starts_with('.')
        && !action.ends_with('.')
        && action
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-'))
}

/// Fields of a row that feed its hash (everything except `seq` and `hash`).
#[derive(Debug, Clone, Copy)]
pub struct AuditHashInput<'a> {
    pub id: &'a str,
    pub ts: i64,
    pub user_id: Option<&'a str>,
    pub actor_kind: &'a str,
    pub actor_id: Option<&'a str>,
    pub via: &'a str,
    pub action: &'a str,
    pub target_kind: Option<&'a str>,
    pub target_id: Option<&'a str>,
    pub summary: &'a str,
    pub diff: Option<&'a str>,
    pub approver_id: Option<&'a str>,
    pub approval_id: Option<&'a str>,
    pub request_fingerprint: Option<&'a str>,
    pub prev_hash: &'a str,
}

impl<'a> AuditHashInput<'a> {
    pub fn from_row(row: &'a AuditEventRow) -> Self {
        Self {
            id: &row.id,
            ts: row.ts,
            user_id: row.user_id.as_deref(),
            actor_kind: &row.actor_kind,
            actor_id: row.actor_id.as_deref(),
            via: &row.via,
            action: &row.action,
            target_kind: row.target_kind.as_deref(),
            target_id: row.target_id.as_deref(),
            summary: &row.summary,
            diff: row.diff.as_deref(),
            approver_id: row.approver_id.as_deref(),
            approval_id: row.approval_id.as_deref(),
            request_fingerprint: row.request_fingerprint.as_deref(),
            prev_hash: &row.prev_hash,
        }
    }
}

/// Lowercase hex `SHA-256(prev_hash "\n" canonical_json)`, where
/// `canonical_json` is a JSON array of the row's fields in a fixed order
/// (versioned by a leading `"v1"`). External tools can recompute it from an
/// export.
pub fn compute_audit_hash(input: &AuditHashInput<'_>) -> String {
    let canonical = serde_json::json!([
        "v1",
        input.id,
        input.ts,
        input.user_id,
        input.actor_kind,
        input.actor_id,
        input.via,
        input.action,
        input.target_kind,
        input.target_id,
        input.summary,
        input.diff,
        input.approver_id,
        input.approval_id,
        input.request_fingerprint,
    ])
    .to_string();
    let mut hasher = Sha256::new();
    hasher.update(input.prev_hash.as_bytes());
    hasher.update(b"\n");
    hasher.update(canonical.as_bytes());
    hex::encode(hasher.finalize())
}

/// Handle used by write points to record audit events.
///
/// [`AuditSink::record`] never fails the caller's operation: a ledger error
/// is logged (`target: "audit"`) and `None` is returned. Use
/// [`AuditSink::try_record`] where the action must not proceed unaudited.
#[derive(Clone)]
pub struct AuditSink {
    repo: Arc<dyn IAuditEventRepository>,
}

impl std::fmt::Debug for AuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditSink").finish_non_exhaustive()
    }
}

impl AuditSink {
    pub fn new(repo: Arc<dyn IAuditEventRepository>) -> Self {
        Self { repo }
    }

    pub fn repository(&self) -> &Arc<dyn IAuditEventRepository> {
        &self.repo
    }

    pub async fn try_record(&self, event: NewAuditEvent) -> Result<AuditEventRow, DbError> {
        self.repo.append(&event).await
    }

    pub async fn record(&self, event: NewAuditEvent) -> Option<AuditEventRow> {
        match self.repo.append(&event).await {
            Ok(row) => Some(row),
            Err(error) => {
                tracing::error!(
                    target: "audit",
                    action = %event.action,
                    error = %error,
                    "failed to append audit event"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn secret_keys_are_fingerprinted_and_other_values_kept() {
        let diff = json!({
            "name": "OpenAI",
            "api_key": "sk-live-123",
            "apiKeyEncrypted": "abc",
            "max_tokens": 4096,
            "nested": { "clientSecret": "s3cr3t", "base_url": "https://api.example.com" },
            "env": { "PATH": "/usr/bin", "N": 3 },
            "headers": { "X-Trace": "t" },
            "list": ["plain", "ghp_abcdefghijklmnopqrstuvwxyz", "sk-SK", "Bearer abc"],
            "empty_token": "",
            "password": null
        });
        let out = redact_diff(&diff);
        assert_eq!(out["name"], "OpenAI");
        assert_eq!(out["api_key"], json!(secret_fingerprint("sk-live-123")));
        assert!(out["apiKeyEncrypted"].as_str().unwrap().starts_with("***sha256:"));
        assert_eq!(out["max_tokens"], 4096);
        assert!(out["nested"]["clientSecret"].as_str().unwrap().starts_with("***"));
        assert_eq!(out["nested"]["base_url"], "https://api.example.com");
        assert!(out["env"]["PATH"].as_str().unwrap().starts_with("***"));
        assert_eq!(out["env"]["N"], 3);
        assert!(out["headers"]["X-Trace"].as_str().unwrap().starts_with("***"));
        assert_eq!(out["list"][0], "plain");
        assert!(out["list"][1].as_str().unwrap().starts_with("***"));
        assert_eq!(out["list"][2], "sk-SK");
        assert!(out["list"][3].as_str().unwrap().starts_with("***"));
        assert_eq!(out["empty_token"], "");
        assert_eq!(out["password"], Value::Null);
        let text = out.to_string();
        for secret in ["sk-live-123", "s3cr3t", "/usr/bin", "ghp_abcdef", "Bearer abc"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
    }

    #[test]
    fn fingerprint_is_stable_and_redaction_idempotent() {
        assert_eq!(secret_fingerprint("a"), secret_fingerprint("a"));
        assert_ne!(secret_fingerprint("a"), secret_fingerprint("b"));
        assert_eq!(secret_fingerprint("a").len(), "***sha256:".len() + 12);
        let once = redact_diff(&json!({ "token": "x" }));
        assert_eq!(redact_diff(&once), once);
    }

    #[test]
    fn oversized_diff_is_replaced_by_marker() {
        let big = json!({ "blob": "x".repeat(AUDIT_DIFF_MAX_BYTES + 10) });
        let text = redacted_diff_text(&big);
        let parsed: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed["truncated"], true);
    }

    #[test]
    fn summary_is_cleaned_and_capped() {
        assert_eq!(clean_summary("a\u{0}b\nc"), "a b\nc");
        let long = "x".repeat(AUDIT_SUMMARY_MAX_CHARS + 5);
        let cut = clean_summary(&long);
        assert_eq!(cut.chars().count(), AUDIT_SUMMARY_MAX_CHARS);
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn action_names_are_validated() {
        assert!(is_valid_action("config.provider.update"));
        assert!(is_valid_action("tool.high_risk"));
        assert!(!is_valid_action(""));
        assert!(!is_valid_action("Config.Update"));
        assert!(!is_valid_action(".x"));
        assert!(!is_valid_action("x."));
        assert!(!is_valid_action("a b"));
        assert!(!is_valid_action(&"a".repeat(129)));
    }

    #[test]
    fn hash_depends_on_every_field_and_prev_hash() {
        let base = AuditHashInput {
            id: "aud_1",
            ts: 1,
            user_id: Some("u1"),
            actor_kind: "user",
            actor_id: Some("u1"),
            via: "desktop",
            action: "config.provider.update",
            target_kind: Some("provider"),
            target_id: Some("p1"),
            summary: "s",
            diff: Some("{}"),
            approver_id: None,
            approval_id: None,
            request_fingerprint: None,
            prev_hash: AUDIT_GENESIS_HASH,
        };
        let h = compute_audit_hash(&base);
        assert_eq!(h.len(), 64);
        assert_eq!(h, compute_audit_hash(&base));
        assert_ne!(h, compute_audit_hash(&AuditHashInput { summary: "t", ..base }));
        assert_ne!(h, compute_audit_hash(&AuditHashInput { prev_hash: &h, ..base }));
        assert_ne!(
            h,
            compute_audit_hash(&AuditHashInput {
                approver_id: Some("u2"),
                ..base
            })
        );
        // None and "" are distinct.
        assert_ne!(
            h,
            compute_audit_hash(&AuditHashInput {
                target_id: Some(""),
                ..base
            })
        );
        assert_ne!(
            compute_audit_hash(&AuditHashInput {
                target_id: None,
                ..base
            }),
            compute_audit_hash(&AuditHashInput {
                target_id: Some(""),
                ..base
            })
        );
    }
}
