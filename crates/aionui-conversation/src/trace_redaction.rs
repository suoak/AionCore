use std::sync::OnceLock;

use regex::Regex;

const MAX_TRACE_TEXT_CHARS: usize = 4_000;
const REDACTED: &str = "[REDACTED]";

fn secret_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(
            r#"(?ix)
            (authorization\s*[:=]\s*(?:bearer\s+)?|bearer\s+|(?:api[_-]?key|access[_-]?token|refresh[_-]?token|token|password|passwd|cookie|secret)\s*[:=]\s*["']?)
            ([^\s,"';}]+)
            "#,
        )
        .expect("trace redaction regex is valid")
    })
}

pub(crate) fn redact_text(value: &str) -> String {
    secret_pattern()
        .replace_all(value, |captures: &regex::Captures<'_>| {
            format!("{}{}", &captures[1], REDACTED)
        })
        .into_owned()
}

pub(crate) fn redact_and_bound(value: &str) -> String {
    let redacted = redact_text(value);
    let mut bounded: String = redacted.chars().take(MAX_TRACE_TEXT_CHARS).collect();
    if redacted.chars().count() > MAX_TRACE_TEXT_CHARS {
        bounded.push('…');
    }
    bounded
}

pub(crate) fn sanitize_json(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| {
                    let normalized = key.to_ascii_lowercase().replace('-', "_");
                    let sensitive = [
                        "authorization",
                        "api_key",
                        "apikey",
                        "access_token",
                        "refresh_token",
                        "password",
                        "passwd",
                        "cookie",
                        "secret",
                        "token",
                    ]
                    .iter()
                    .any(|candidate| normalized == *candidate || normalized.ends_with(&format!("_{candidate}")));
                    (
                        key.clone(),
                        if sensitive {
                            serde_json::Value::String(REDACTED.into())
                        } else {
                            sanitize_json(value)
                        },
                    )
                })
                .collect(),
        ),
        serde_json::Value::Array(values) => {
            serde_json::Value::Array(values.iter().take(100).map(sanitize_json).collect())
        }
        serde_json::Value::String(value) => serde_json::Value::String(redact_and_bound(value)),
        value => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_named_and_inline_secrets() {
        let sanitized = sanitize_json(&serde_json::json!({
            "authorization": "Bearer abc123",
            "nested": { "api_key": "sk-secret", "message": "password=hunter2" },
            "safe": "visible"
        }));
        let encoded = sanitized.to_string();
        assert!(!encoded.contains("abc123"));
        assert!(!encoded.contains("sk-secret"));
        assert!(!encoded.contains("hunter2"));
        assert!(encoded.contains("visible"));
    }

    #[test]
    fn bounds_persisted_text() {
        let value = redact_and_bound(&"x".repeat(MAX_TRACE_TEXT_CHARS + 20));
        assert_eq!(value.chars().count(), MAX_TRACE_TEXT_CHARS + 1);
        assert!(value.ends_with('…'));
    }
}
