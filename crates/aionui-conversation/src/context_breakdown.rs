//! Context-ring breakdown (M7 E5-3).
//!
//! Builds the `GET /api/conversations/{id}/context-breakdown` response from
//! the persisted / live `context_usage` JSON. Provider-reported categories
//! (an optional `breakdown` array inside `context_usage`) win; local
//! estimates are used only when the provider reported no categories, and are
//! always marked `estimated`. Segments are clamped so they never exceed `used`, and
//! the remainder is an `other` segment, so the ring always sums to `used`.

use aionui_api_types::{ContextBreakdownResponse, ContextBreakdownSegment, ContextSegmentSource};
use serde_json::Value;

const MAX_LABEL_CHARS: usize = 64;
const MAX_SEGMENTS: usize = 32;
const OTHER_KEY: &str = "other";

/// Categories that describe unused capacity rather than context content.
const CAPACITY_KEYS: &[&str] = &["free_space", "free", "autocompact_buffer", "reserved"];

/// Rough per-schema cost of one advertised (non-deferred) tool. Real schemas
/// range from ~50 to several thousand tokens; this is labeled `estimated`.
pub(crate) const EST_TOKENS_PER_TOOL_SCHEMA: u64 = 300;
/// Rough cost of one skill listing (name + description) in the system prompt.
pub(crate) const EST_TOKENS_PER_SKILL_LISTING: u64 = 100;

/// Local estimates from the live tool surface: tool schemas and skill listings.
pub(crate) fn estimates_from_tool_surface(
    surface: Option<&aionui_api_types::ConversationToolSurface>,
) -> Vec<EstimatedSegment> {
    use aionui_api_types::ToolSurfaceKind;
    let Some(surface) = surface else {
        return Vec::new();
    };
    let (mut tools, mut skills) = (0u64, 0u64);
    if surface.entries.is_empty() {
        tools = surface.tools.len() as u64;
    } else {
        for entry in &surface.entries {
            match entry.kind {
                ToolSurfaceKind::Skill => skills += 1,
                ToolSurfaceKind::Builtin | ToolSurfaceKind::Mcp if entry.deferred != Some(true) => tools += 1,
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    if tools > 0 {
        out.push(EstimatedSegment {
            key: "tools".to_owned(),
            label: "Tool schemas".to_owned(),
            tokens: tools.saturating_mul(EST_TOKENS_PER_TOOL_SCHEMA),
        });
    }
    if skills > 0 {
        out.push(EstimatedSegment {
            key: "skills".to_owned(),
            label: "Skills".to_owned(),
            tokens: skills.saturating_mul(EST_TOKENS_PER_SKILL_LISTING),
        });
    }
    out
}

/// A locally estimated segment supplied by the caller.
#[derive(Debug, Clone)]
pub(crate) struct EstimatedSegment {
    pub key: String,
    pub label: String,
    pub tokens: u64,
}

fn slug(name: &str) -> String {
    let mut out = String::new();
    let mut last_us = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_us = false;
        } else if !last_us && !out.is_empty() {
            out.push('_');
            last_us = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

fn clean_label(name: &str) -> String {
    name.trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_LABEL_CHARS)
        .collect()
}

fn reported_categories(usage: &Value) -> Vec<(String, String, u64)> {
    let Some(items) = usage.get("breakdown").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String, u64)> = Vec::new();
    for item in items {
        let Some(name) = item.get("label").or_else(|| item.get("name")).and_then(Value::as_str) else {
            continue;
        };
        let key = item
            .get("key")
            .and_then(Value::as_str)
            .map(slug)
            .unwrap_or_else(|| slug(name));
        if key.is_empty() || key == OTHER_KEY || CAPACITY_KEYS.contains(&key.as_str()) {
            continue;
        }
        let tokens = item.get("tokens").and_then(Value::as_u64).unwrap_or(0);
        if tokens == 0 {
            continue;
        }
        if let Some(existing) = out.iter_mut().find(|(k, _, _)| *k == key) {
            existing.2 = existing.2.saturating_add(tokens);
        } else if out.len() < MAX_SEGMENTS {
            out.push((key, clean_label(name), tokens));
        }
    }
    out
}

/// Build the breakdown. `usage` is the `context_usage` JSON (live or
/// persisted); `estimates` are local estimates (skills, tool schemas, ...).
pub(crate) fn build_context_breakdown(
    conversation_id: &str,
    usage: Option<&Value>,
    estimates: &[EstimatedSegment],
) -> ContextBreakdownResponse {
    let used = usage.and_then(|u| u.get("used")).and_then(Value::as_u64);
    let size = usage
        .and_then(|u| u.get("size").or_else(|| u.get("max")))
        .and_then(Value::as_u64)
        .filter(|size| *size > 0);
    let Some(used) = used else {
        return ContextBreakdownResponse {
            conversation_id: conversation_id.to_owned(),
            used: None,
            size,
            source: ContextSegmentSource::Unavailable,
            segments: Vec::new(),
        };
    };

    let mut segments = Vec::new();
    let mut remaining = used;
    let mut any_estimated = false;
    let reported = usage.map(reported_categories).unwrap_or_default();
    for (key, label, tokens) in &reported {
        let tokens = (*tokens).min(remaining);
        if tokens == 0 {
            break;
        }
        remaining -= tokens;
        segments.push(ContextBreakdownSegment {
            key: key.clone(),
            label: label.clone(),
            tokens,
            source: ContextSegmentSource::Reported,
        });
    }
    // Provider categories already cover the whole context (claude reports
    // system tools / MCP tools / skills itself), so estimates are only used when
    // the provider reported none — mixing both would double-count.
    let estimates = if reported.is_empty() { estimates } else { &[] };
    for estimate in estimates {
        let key = slug(&estimate.key);
        if key.is_empty()
            || key == OTHER_KEY
            || segments.iter().any(|segment| segment.key == key)
            || segments.len() >= MAX_SEGMENTS
        {
            continue;
        }
        let tokens = estimate.tokens.min(remaining);
        if tokens == 0 {
            continue;
        }
        remaining -= tokens;
        any_estimated = true;
        segments.push(ContextBreakdownSegment {
            key,
            label: clean_label(&estimate.label),
            tokens,
            source: ContextSegmentSource::Estimated,
        });
    }
    if remaining > 0 {
        segments.push(ContextBreakdownSegment {
            key: OTHER_KEY.to_owned(),
            label: "Other".to_owned(),
            tokens: remaining,
            // The remainder is exact only when every named slice was reported.
            source: if any_estimated {
                ContextSegmentSource::Estimated
            } else {
                ContextSegmentSource::Reported
            },
        });
    }

    ContextBreakdownResponse {
        conversation_id: conversation_id.to_owned(),
        used: Some(used),
        size,
        source: ContextSegmentSource::Reported,
        segments,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sum(resp: &ContextBreakdownResponse) -> u64 {
        resp.segments.iter().map(|segment| segment.tokens).sum()
    }

    fn estimate(key: &str, tokens: u64) -> EstimatedSegment {
        EstimatedSegment {
            key: key.to_owned(),
            label: key.to_owned(),
            tokens,
        }
    }

    #[test]
    fn missing_usage_is_unavailable() {
        let resp = build_context_breakdown("c1", None, &[estimate("skills", 10)]);
        assert_eq!(resp.source, ContextSegmentSource::Unavailable);
        assert_eq!(resp.used, None);
        assert!(resp.segments.is_empty());

        let resp = build_context_breakdown("c1", Some(&json!({"size": 1000})), &[]);
        assert_eq!(resp.source, ContextSegmentSource::Unavailable);
        assert_eq!(resp.size, Some(1000));
    }

    #[test]
    fn used_without_categories_is_single_reported_other() {
        let usage = json!({"used": 500, "size": 200000});
        let resp = build_context_breakdown("c1", Some(&usage), &[]);
        assert_eq!(resp.used, Some(500));
        assert_eq!(resp.size, Some(200000));
        assert_eq!(resp.segments.len(), 1);
        assert_eq!(resp.segments[0].key, "other");
        assert_eq!(resp.segments[0].source, ContextSegmentSource::Reported);
        assert_eq!(sum(&resp), 500);
    }

    #[test]
    fn reported_categories_win_and_capacity_is_dropped() {
        let usage = json!({
            "used": 1000,
            "size": 200000,
            "breakdown": [
                {"name": "System prompt", "tokens": 300},
                {"name": "System tools", "tokens": 200},
                {"name": "Messages", "tokens": 100},
                {"name": "Free space", "tokens": 190000},
                {"name": "Autocompact buffer", "tokens": 9000},
                {"name": "messages", "tokens": 50},
                {"name": "Empty", "tokens": 0}
            ]
        });
        let resp = build_context_breakdown(
            "c1",
            Some(&usage),
            &[estimate("system_tools", 999), estimate("skills", 120)],
        );
        let keys: Vec<_> = resp.segments.iter().map(|segment| segment.key.as_str()).collect();
        assert_eq!(keys, ["system_prompt", "system_tools", "messages", "other"]);
        assert_eq!(resp.segments[1].tokens, 200, "reported value beats estimate");
        assert!(
            resp.segments
                .iter()
                .all(|segment| segment.source == ContextSegmentSource::Reported),
            "estimates never mix into a provider breakdown"
        );
        assert_eq!(resp.segments[2].tokens, 150, "duplicate keys merge");
        assert_eq!(resp.segments[3].tokens, 1000 - 300 - 200 - 150);
        assert_eq!(sum(&resp), 1000);
    }

    #[test]
    fn estimates_fill_in_without_provider_categories() {
        let usage = json!({"used": 1000});
        let resp = build_context_breakdown("c1", Some(&usage), &[estimate("tools", 600), estimate("skills", 100)]);
        let keys: Vec<_> = resp.segments.iter().map(|segment| segment.key.as_str()).collect();
        assert_eq!(keys, ["tools", "skills", "other"]);
        assert!(
            resp.segments
                .iter()
                .all(|segment| segment.source == ContextSegmentSource::Estimated)
        );
        assert_eq!(sum(&resp), 1000);
    }

    #[test]
    fn tool_surface_estimates_skip_deferred_tools() {
        use aionui_api_types::{ConversationToolSurface, McpIsolation, ToolSurfaceEntry, ToolSurfaceKind};
        assert!(estimates_from_tool_surface(None).is_empty());
        let entry = |name: &str, kind: ToolSurfaceKind, deferred: Option<bool>| ToolSurfaceEntry {
            name: name.to_owned(),
            canonical: name.to_owned(),
            kind,
            server: None,
            deferred,
        };
        let mut surface = ConversationToolSurface::launch_config(Vec::new(), McpIsolation::Strict);
        surface.tools = vec!["Read".into(), "Bash".into()];
        let names_only = estimates_from_tool_surface(Some(&surface));
        assert_eq!(names_only.len(), 1);
        assert_eq!(names_only[0].tokens, 2 * EST_TOKENS_PER_TOOL_SCHEMA);

        surface.entries = vec![
            entry("Read", ToolSurfaceKind::Builtin, None),
            entry("mcp__a__x", ToolSurfaceKind::Mcp, Some(false)),
            entry("mcp__a__y", ToolSurfaceKind::Mcp, Some(true)),
            entry("skill:pdf", ToolSurfaceKind::Skill, None),
        ];
        let estimates = estimates_from_tool_surface(Some(&surface));
        assert_eq!(estimates[0].key, "tools");
        assert_eq!(estimates[0].tokens, 2 * EST_TOKENS_PER_TOOL_SCHEMA);
        assert_eq!(estimates[1].key, "skills");
        assert_eq!(estimates[1].tokens, EST_TOKENS_PER_SKILL_LISTING);
    }

    #[test]
    fn segments_are_clamped_to_used() {
        let usage = json!({
            "used": 100,
            "breakdown": [{"name": "System prompt", "tokens": 80}, {"name": "Messages", "tokens": 80}]
        });
        let resp = build_context_breakdown("c1", Some(&usage), &[estimate("skills", 50)]);
        assert_eq!(sum(&resp), 100);
        assert_eq!(resp.segments.len(), 2);
        assert_eq!(resp.segments[1].tokens, 20);
        assert!(resp.segments.iter().all(|segment| segment.key != "other"));

        let usage = json!({"used": 100});
        let resp = build_context_breakdown("c1", Some(&usage), &[estimate("skills", 500)]);
        assert_eq!(sum(&resp), 100);
        assert_eq!(resp.segments.len(), 1);
        assert_eq!(resp.segments[0].source, ContextSegmentSource::Estimated);
    }

    #[test]
    fn labels_are_sanitized_and_bounded() {
        let long = "x".repeat(200);
        let usage = json!({
            "used": 10,
            "breakdown": [{"name": format!("\u{0007}{long}"), "tokens": 5}, {"key": "other", "name": "Other", "tokens": 5}]
        });
        let resp = build_context_breakdown("c1", Some(&usage), &[]);
        assert_eq!(resp.segments[0].label.chars().count(), MAX_LABEL_CHARS);
        assert!(!resp.segments[0].label.chars().any(char::is_control));
        assert_eq!(resp.segments.iter().filter(|segment| segment.key == "other").count(), 1);
        assert_eq!(sum(&resp), 10);
    }
}
