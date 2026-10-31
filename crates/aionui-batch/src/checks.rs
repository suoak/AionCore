//! Artifact checks. A batch item passes only when every check passes on the
//! files present in its workspace after the agent finished; the agent's
//! reply is never consulted.
//!
//! All paths and patterns are relative to the workspace. `..`, absolute
//! paths and symlinks that resolve outside the workspace are rejected, the
//! glob walk never descends into `.git` and never follows symlinks, and
//! JSON reads are capped at [`MAX_JSON_BYTES`]. Schemas are compiled at
//! create time; remote `$ref`s cannot resolve (no resolve-http, D8-6).

use std::path::{Component, Path, PathBuf};

use aionui_api_types::{BATCH_MAX_CHECKS, BatchCheckResult, BatchCheckSpec};
use globset::{GlobBuilder, GlobMatcher};
use serde_json::Value;

use crate::error::BatchError;

pub const MAX_JSON_BYTES: u64 = 1024 * 1024;
pub const MAX_WALK_ENTRIES: usize = 200_000;
const MAX_WALK_DEPTH: usize = 64;
const MAX_DETAIL_CHARS: usize = 600;
const MAX_SCHEMA_ERRORS: usize = 3;

enum CompiledCheck {
    FileExists {
        path: PathBuf,
    },
    Glob {
        pattern: String,
        matcher: GlobMatcher,
        min: u32,
        max: Option<u32>,
    },
    JsonSchema {
        path: PathBuf,
        validator: Box<jsonschema::Validator>,
    },
}

/// Checks compiled once per job (create-time validation and every item
/// evaluation use the same code path).
pub struct CompiledChecks {
    checks: Vec<CompiledCheck>,
}

impl std::fmt::Debug for CompiledChecks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompiledChecks")
            .field("len", &self.checks.len())
            .finish()
    }
}

impl CompiledChecks {
    pub fn compile(specs: &[BatchCheckSpec]) -> Result<Self, BatchError> {
        if specs.is_empty() {
            return Err(BatchError::bad(
                "at least one artifact check is required (items are judged only by artifacts)",
            ));
        }
        if specs.len() > BATCH_MAX_CHECKS {
            return Err(BatchError::bad(format!(
                "at most {BATCH_MAX_CHECKS} checks are allowed"
            )));
        }
        let checks = specs
            .iter()
            .enumerate()
            .map(|(i, spec)| compile_one(spec).map_err(|e| BatchError::bad(format!("checks[{i}]: {e}"))))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { checks })
    }

    pub fn len(&self) -> usize {
        self.checks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// Evaluates every check against `workspace` (blocking filesystem IO;
    /// call from `spawn_blocking`). One result per check, in order.
    pub fn evaluate(&self, workspace: &Path) -> Vec<BatchCheckResult> {
        let root = match dunce::canonicalize(workspace) {
            Ok(root) if root.is_dir() => root,
            _ => {
                return self
                    .checks
                    .iter()
                    .map(|c| fail(c.kind(), "item workspace is missing"))
                    .collect();
            }
        };
        let mut walked: Option<Result<Vec<String>, String>> = None;
        self.checks
            .iter()
            .map(|check| match check {
                CompiledCheck::FileExists { path } => match resolve_inside(&root, path) {
                    Ok(p) if p.is_file() => pass("file_exists", None),
                    Ok(_) => fail("file_exists", format!("{} is not a regular file", display(path))),
                    Err(e) => fail("file_exists", e),
                },
                CompiledCheck::Glob {
                    pattern,
                    matcher,
                    min,
                    max,
                } => {
                    let files = walked.get_or_insert_with(|| walk_files(&root));
                    match files {
                        Err(e) => fail("glob", e.clone()),
                        Ok(files) => {
                            let n = files.iter().filter(|f| matcher.is_match(f.as_str())).count();
                            let n32 = u32::try_from(n).unwrap_or(u32::MAX);
                            let ok = n32 >= *min && max.is_none_or(|m| n32 <= m);
                            let bound = match max {
                                Some(m) => format!("{min}..={m}"),
                                None => format!(">= {min}"),
                            };
                            let detail = format!("{n} file(s) match '{pattern}' (expected {bound})");
                            if ok {
                                pass("glob", Some(detail))
                            } else {
                                fail("glob", detail)
                            }
                        }
                    }
                }
                CompiledCheck::JsonSchema { path, validator } => match read_json(&root, path) {
                    Err(e) => fail("json_schema", e),
                    Ok(instance) => {
                        let errors: Vec<String> = validator
                            .iter_errors(&instance)
                            .take(MAX_SCHEMA_ERRORS)
                            .map(|e| {
                                let at = e.instance_path().to_string();
                                if at.is_empty() {
                                    e.to_string()
                                } else {
                                    format!("{at}: {e}")
                                }
                            })
                            .collect();
                        if errors.is_empty() {
                            pass("json_schema", None)
                        } else {
                            fail(
                                "json_schema",
                                format!("{} does not match the schema: {}", display(path), errors.join("; ")),
                            )
                        }
                    }
                },
            })
            .collect()
    }
}

/// True when there is at least one result and every one passed.
pub fn all_passed(results: &[BatchCheckResult]) -> bool {
    !results.is_empty() && results.iter().all(|r| r.ok)
}

impl CompiledCheck {
    fn kind(&self) -> &'static str {
        match self {
            Self::FileExists { .. } => "file_exists",
            Self::Glob { .. } => "glob",
            Self::JsonSchema { .. } => "json_schema",
        }
    }
}

fn compile_one(spec: &BatchCheckSpec) -> Result<CompiledCheck, String> {
    match spec {
        BatchCheckSpec::FileExists { path } => Ok(CompiledCheck::FileExists {
            path: safe_relative(path)?,
        }),
        BatchCheckSpec::Glob { pattern, min, max } => {
            safe_pattern(pattern)?;
            let min = min.unwrap_or(1);
            if let Some(max) = max
                && *max < min
            {
                return Err(format!("glob max ({max}) is below min ({min})"));
            }
            let matcher = GlobBuilder::new(pattern)
                .literal_separator(true)
                .build()
                .map_err(|e| format!("invalid glob '{pattern}': {e}"))?
                .compile_matcher();
            Ok(CompiledCheck::Glob {
                pattern: pattern.clone(),
                matcher,
                min,
                max: *max,
            })
        }
        BatchCheckSpec::JsonSchema { path, schema } => {
            let path = safe_relative(path)?;
            if !schema.is_object() && !schema.is_boolean() {
                return Err("schema must be a JSON object or boolean".into());
            }
            let validator = jsonschema::validator_for(schema).map_err(|e| format!("invalid JSON schema: {e}"))?;
            Ok(CompiledCheck::JsonSchema {
                path,
                validator: Box::new(validator),
            })
        }
    }
}

/// A non-empty relative path made only of normal components.
fn safe_relative(raw: &str) -> Result<PathBuf, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("path must not be empty".into());
    }
    if trimmed.contains('\\') {
        return Err(format!("path '{raw}' must use '/' separators"));
    }
    let mut out = PathBuf::new();
    for comp in Path::new(trimmed).components() {
        match comp {
            Component::Normal(c) => out.push(c),
            Component::CurDir => {}
            _ => {
                return Err(format!(
                    "path '{raw}' must be relative to the item workspace without '..'"
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("path '{raw}' does not name a file"));
    }
    Ok(out)
}

fn safe_pattern(pattern: &str) -> Result<(), String> {
    if pattern.trim().is_empty() {
        return Err("glob pattern must not be empty".into());
    }
    if pattern.starts_with('/') || pattern.contains('\\') || pattern.contains(':') {
        return Err(format!("glob '{pattern}' must be relative with '/' separators"));
    }
    if pattern.split('/').any(|seg| seg == "..") {
        return Err(format!("glob '{pattern}' must not contain '..'"));
    }
    Ok(())
}

/// Canonicalizes `root/rel` and requires it to stay inside `root`
/// (defeats symlinks pointing outside the workspace).
fn resolve_inside(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    let joined = root.join(rel);
    let resolved = dunce::canonicalize(&joined).map_err(|_| format!("{} not found", display(rel)))?;
    if !resolved.starts_with(root) {
        return Err(format!("{} resolves outside the item workspace", display(rel)));
    }
    Ok(resolved)
}

fn read_json(root: &Path, rel: &Path) -> Result<Value, String> {
    let path = resolve_inside(root, rel)?;
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", display(rel)))?;
    if !meta.is_file() {
        return Err(format!("{} is not a regular file", display(rel)));
    }
    if meta.len() > MAX_JSON_BYTES {
        return Err(format!("{} is larger than {} bytes", display(rel), MAX_JSON_BYTES));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", display(rel)))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("{} is not valid JSON: {e}", display(rel)))
}

/// Regular files under `root` as '/'-separated relative paths, skipping
/// `.git` and never following symlinks.
fn walk_files(root: &Path) -> Result<Vec<String>, String> {
    let mut files = Vec::new();
    let walker = walkdir::WalkDir::new(root)
        .follow_links(false)
        .max_depth(MAX_WALK_DEPTH)
        .into_iter()
        .filter_entry(|e| e.depth() == 0 || e.file_name() != ".git");
    for (seen, entry) in walker.enumerate() {
        if seen >= MAX_WALK_ENTRIES {
            return Err(format!(
                "workspace has more than {MAX_WALK_ENTRIES} entries; narrow the check"
            ));
        }
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Ok(rel) = entry.path().strip_prefix(root) {
            let parts: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            files.push(parts.join("/"));
        }
    }
    Ok(files)
}

fn display(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

fn truncate(s: String) -> String {
    if s.chars().count() <= MAX_DETAIL_CHARS {
        return s;
    }
    let mut out: String = s.chars().take(MAX_DETAIL_CHARS).collect();
    out.push('…');
    out
}

fn pass(kind: &str, detail: Option<String>) -> BatchCheckResult {
    BatchCheckResult {
        kind: kind.to_string(),
        ok: true,
        detail: detail.map(truncate),
    }
}

fn fail(kind: &str, detail: impl Into<String>) -> BatchCheckResult {
    BatchCheckResult {
        kind: kind.to_string(),
        ok: false,
        detail: Some(truncate(detail.into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn specs(v: Value) -> Vec<BatchCheckSpec> {
        serde_json::from_value(v).unwrap()
    }

    fn release_schema() -> Value {
        json!({
            "type": "object",
            "required": ["version", "ready"],
            "properties": {"version": {"type": "string"}, "ready": {"type": "boolean"}}
        })
    }

    #[test]
    fn compile_rejects_unsafe_paths_and_bad_specs() {
        for bad in [
            json!([]),
            json!([{"kind": "file_exists", "path": "../x"}]),
            json!([{"kind": "file_exists", "path": "/etc/passwd"}]),
            json!([{"kind": "file_exists", "path": "a\\b"}]),
            json!([{"kind": "file_exists", "path": "  "}]),
            json!([{"kind": "glob", "pattern": "../**"}]),
            json!([{"kind": "glob", "pattern": "/abs/*"}]),
            json!([{"kind": "glob", "pattern": "a/[", "min": 1}]),
            json!([{"kind": "glob", "pattern": "*.md", "min": 3, "max": 2}]),
            json!([{"kind": "json_schema", "path": "a.json", "schema": "nope"}]),
            json!([{"kind": "json_schema", "path": "a.json", "schema": {"type": 12}}]),
        ] {
            assert!(CompiledChecks::compile(&specs(bad.clone())).is_err(), "{bad}");
        }
        let many: Vec<Value> = (0..=BATCH_MAX_CHECKS)
            .map(|_| json!({"kind": "file_exists", "path": "a"}))
            .collect();
        assert!(CompiledChecks::compile(&specs(Value::Array(many))).is_err());
    }

    #[test]
    fn remote_refs_do_not_resolve() {
        let s = specs(json!([{"kind": "json_schema", "path": "a.json",
            "schema": {"$ref": "https://example.invalid/schema.json"}}]));
        assert!(CompiledChecks::compile(&s).is_err());
    }

    #[test]
    fn release_check_passes_and_fails_on_artifacts_only() {
        let dir = tempfile::tempdir().unwrap();
        let checks = CompiledChecks::compile(&specs(json!([
            {"kind": "file_exists", "path": "release-check.json"},
            {"kind": "json_schema", "path": "./release-check.json", "schema": release_schema()}
        ])))
        .unwrap();

        let missing = checks.evaluate(dir.path());
        assert!(!all_passed(&missing));
        assert!(missing.iter().all(|r| !r.ok));

        std::fs::write(dir.path().join("release-check.json"), r#"{"version": 1}"#).unwrap();
        let bad = checks.evaluate(dir.path());
        assert!(bad[0].ok);
        assert!(!bad[1].ok);
        let detail = bad[1].detail.clone().unwrap();
        assert!(detail.contains("does not match"), "{detail}");

        std::fs::write(
            dir.path().join("release-check.json"),
            r#"{"version": "1.2.0", "ready": true}"#,
        )
        .unwrap();
        assert!(all_passed(&checks.evaluate(dir.path())));
    }

    #[test]
    fn invalid_or_oversized_json_fails() {
        let dir = tempfile::tempdir().unwrap();
        let checks = CompiledChecks::compile(&specs(json!([
            {"kind": "json_schema", "path": "a.json", "schema": true}
        ])))
        .unwrap();
        std::fs::write(dir.path().join("a.json"), "{not json").unwrap();
        assert!(!checks.evaluate(dir.path())[0].ok);
        let big = format!("\"{}\"", "x".repeat(MAX_JSON_BYTES as usize + 1));
        std::fs::write(dir.path().join("a.json"), big).unwrap();
        let r = checks.evaluate(dir.path());
        assert!(!r[0].ok);
        assert!(r[0].detail.as_deref().unwrap().contains("larger than"));
    }

    #[test]
    fn glob_counts_files_with_bounds_and_skips_git() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("reports/sub")).unwrap();
        std::fs::create_dir_all(dir.path().join(".git")).unwrap();
        std::fs::write(dir.path().join("reports/a.md"), "a").unwrap();
        std::fs::write(dir.path().join("reports/b.md"), "b").unwrap();
        std::fs::write(dir.path().join("reports/sub/c.md"), "c").unwrap();
        std::fs::write(dir.path().join(".git/x.md"), "x").unwrap();

        let eval = |v: Value| CompiledChecks::compile(&specs(v)).unwrap().evaluate(dir.path());
        assert!(eval(json!([{"kind": "glob", "pattern": "reports/*.md", "min": 2, "max": 2}]))[0].ok);
        assert!(!eval(json!([{"kind": "glob", "pattern": "reports/*.md", "min": 3}]))[0].ok);
        assert!(eval(json!([{"kind": "glob", "pattern": "**/*.md", "min": 3, "max": 3}]))[0].ok);
        assert!(!eval(json!([{"kind": "glob", "pattern": "**/x.md"}]))[0].ok);
        assert!(!eval(json!([{"kind": "glob", "pattern": "*.txt"}]))[0].ok);
    }

    #[test]
    fn missing_workspace_fails_every_check() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        let checks = CompiledChecks::compile(&specs(json!([
            {"kind": "file_exists", "path": "a"}, {"kind": "glob", "pattern": "*"}
        ])))
        .unwrap();
        let r = checks.evaluate(&gone);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|r| !r.ok));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_workspace_is_rejected() {
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.json"), "{}").unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path().join("secret.json"), dir.path().join("link.json")).unwrap();
        let checks = CompiledChecks::compile(&specs(json!([
            {"kind": "file_exists", "path": "link.json"},
            {"kind": "json_schema", "path": "link.json", "schema": true}
        ])))
        .unwrap();
        let r = checks.evaluate(dir.path());
        assert!(r.iter().all(|r| !r.ok), "{r:?}");
        assert!(r[0].detail.as_deref().unwrap().contains("outside"));
    }
}
