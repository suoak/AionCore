//! Prompt templates: `{{target}}` (whole target, compact JSON),
//! `{{target.<key>}}` (one top-level field; strings render raw, other
//! values as compact JSON) and `{{seq}}` (1-based item number). Anything
//! else inside `{{ }}` is rejected at create time so a typo never reaches
//! an agent.

use serde_json::Value;

use crate::error::BatchError;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Target,
    Field(String),
    Seq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptTemplate {
    segments: Vec<Segment>,
}

pub const MAX_PROMPT_CHARS: usize = 32 * 1024;

impl PromptTemplate {
    pub fn parse(prompt: &str) -> Result<Self, BatchError> {
        if prompt.trim().is_empty() {
            return Err(BatchError::bad("template.prompt must not be empty"));
        }
        if prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(BatchError::bad(format!(
                "template.prompt exceeds {MAX_PROMPT_CHARS} characters"
            )));
        }
        let mut segments = Vec::new();
        let mut rest = prompt;
        while let Some(start) = rest.find("{{") {
            if start > 0 {
                segments.push(Segment::Literal(rest[..start].to_string()));
            }
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| BatchError::bad("template.prompt has an unterminated '{{'"))?;
            let name = after[..end].trim();
            segments.push(parse_placeholder(name)?);
            rest = &after[end + 2..];
        }
        if !rest.is_empty() {
            segments.push(Segment::Literal(rest.to_string()));
        }
        Ok(Self { segments })
    }

    /// Top-level target fields the template references.
    pub fn fields(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Field(f) => Some(f.as_str()),
            _ => None,
        })
    }

    /// Renders for one item. Fails when a referenced field is missing or null.
    pub fn render(&self, target: &Value, seq: i64) -> Result<String, BatchError> {
        let mut out = String::new();
        for seg in &self.segments {
            match seg {
                Segment::Literal(s) => out.push_str(s),
                Segment::Seq => out.push_str(&seq.to_string()),
                Segment::Target => out.push_str(&target.to_string()),
                Segment::Field(f) => match target.get(f) {
                    None | Some(Value::Null) => {
                        return Err(BatchError::bad(format!(
                            "target #{seq} has no field '{f}' referenced by the template"
                        )));
                    }
                    Some(Value::String(s)) => out.push_str(s),
                    Some(v) => out.push_str(&v.to_string()),
                },
            }
        }
        Ok(out)
    }
}

fn parse_placeholder(name: &str) -> Result<Segment, BatchError> {
    match name {
        "target" => Ok(Segment::Target),
        "seq" => Ok(Segment::Seq),
        _ => {
            let field = name
                .strip_prefix("target.")
                .filter(|f| !f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            field.map(|f| Segment::Field(f.to_string())).ok_or_else(|| {
                BatchError::bad(format!(
                    "unknown template placeholder '{{{{{name}}}}}' (use {{{{target}}}}, {{{{target.<key>}}}} or {{{{seq}}}})"
                ))
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn renders_fields_target_and_seq() {
        let t =
            PromptTemplate::parse("Check {{ target.name }} (#{{seq}}) v={{target.version}} raw={{target}}").unwrap();
        let target = json!({"repo": "/r", "name": "alpha", "version": 3});
        let out = t.render(&target, 2).unwrap();
        let raw = out.strip_prefix("Check alpha (#2) v=3 raw=").expect("fields rendered");
        // Key order depends on serde_json's `preserve_order` (feature-unified
        // across the workspace), so compare values, not text.
        assert_eq!(serde_json::from_str::<serde_json::Value>(raw).unwrap(), target);
        assert_eq!(t.fields().collect::<Vec<_>>(), vec!["name", "version"]);
    }

    #[test]
    fn literal_only_template_is_kept_verbatim() {
        let t = PromptTemplate::parse("Write release-check.json { not a placeholder }").unwrap();
        assert_eq!(
            t.render(&json!({}), 1).unwrap(),
            "Write release-check.json { not a placeholder }"
        );
    }

    #[test]
    fn rejects_unknown_unterminated_and_empty() {
        assert!(PromptTemplate::parse("{{targets.name}}").is_err());
        assert!(PromptTemplate::parse("{{target.}}").is_err());
        assert!(PromptTemplate::parse("{{target.a b}}").is_err());
        assert!(PromptTemplate::parse("hello {{seq").is_err());
        assert!(PromptTemplate::parse("   ").is_err());
    }

    #[test]
    fn missing_or_null_field_fails_render() {
        let t = PromptTemplate::parse("{{target.name}}").unwrap();
        assert!(t.render(&json!({"name": null}), 1).is_err());
        assert!(t.render(&json!({}), 1).is_err());
    }
}
