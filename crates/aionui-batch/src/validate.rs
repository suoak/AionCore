//! Create-request validation with the accepted defaults:
//! D8-2 trial 1..=5 (default 1), D8-3 agent mode only,
//! D8-4 concurrency 1..=16 (default 4) and 30 min per item.

use std::path::{Path, PathBuf};

use aionui_api_types::{
    BATCH_DEFAULT_CONCURRENCY, BATCH_DEFAULT_ITEM_TIMEOUT_SEC, BATCH_DEFAULT_TRIAL_SIZE, BATCH_MAX_CONCURRENCY,
    BATCH_MAX_ITEM_TIMEOUT_SEC, BATCH_MAX_TARGETS, BATCH_MAX_TRIAL_SIZE, BATCH_MIN_ITEM_TIMEOUT_SEC, BATCH_MODE_AGENT,
    BatchCheckSpec, CreateBatchJobRequest,
};
use serde_json::Value;

use crate::checks::CompiledChecks;
use crate::error::BatchError;
use crate::template::PromptTemplate;

pub const MAX_TITLE_CHARS: usize = 200;
const MAX_TARGET_BYTES: usize = 16 * 1024;

/// Where one item runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetLocation {
    /// A git repository; the item gets its own managed worktree.
    Repo(PathBuf),
    /// An existing directory used in place (no worktree).
    Workspace(PathBuf),
}

impl TargetLocation {
    pub fn path(&self) -> &Path {
        match self {
            Self::Repo(p) | Self::Workspace(p) => p,
        }
    }
}

/// Reads `repo` / `workspace` from a target object (exactly one, absolute).
pub fn target_location(target: &Value) -> Result<TargetLocation, BatchError> {
    let obj = target
        .as_object()
        .ok_or_else(|| BatchError::bad("each target must be a JSON object"))?;
    let field = |k: &str| -> Result<Option<PathBuf>, BatchError> {
        match obj.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if !s.trim().is_empty() => {
                let p = PathBuf::from(s.trim());
                if !p.is_absolute() {
                    return Err(BatchError::bad(format!("target.{k} must be an absolute path")));
                }
                Ok(Some(p))
            }
            Some(_) => Err(BatchError::bad(format!("target.{k} must be a non-empty string"))),
        }
    };
    match (field("repo")?, field("workspace")?) {
        (Some(r), None) => Ok(TargetLocation::Repo(r)),
        (None, Some(w)) => Ok(TargetLocation::Workspace(w)),
        (Some(_), Some(_)) => Err(BatchError::bad("target must set only one of 'repo' or 'workspace'")),
        (None, None) => Err(BatchError::bad("target must set 'repo' or 'workspace'")),
    }
}

/// A create request after validation; numbers are already defaulted.
#[derive(Debug)]
pub struct ValidatedBatchJob {
    pub title: String,
    pub prompt: String,
    pub template: PromptTemplate,
    pub targets: Vec<Value>,
    pub locations: Vec<TargetLocation>,
    pub checks: Vec<BatchCheckSpec>,
    pub compiled_checks: CompiledChecks,
    pub assistant_id: String,
    pub agent_type: Option<String>,
    pub project_id: Option<String>,
    pub concurrency: u32,
    pub item_timeout_sec: u32,
    pub trial_size: u32,
}

pub fn validate_create(req: CreateBatchJobRequest) -> Result<ValidatedBatchJob, BatchError> {
    let title = req.title.trim().to_string();
    if title.is_empty() {
        return Err(BatchError::bad("title must not be empty"));
    }
    if title.chars().count() > MAX_TITLE_CHARS {
        return Err(BatchError::bad(format!("title exceeds {MAX_TITLE_CHARS} characters")));
    }
    if let Some(mode) = req.mode.as_deref()
        && mode != BATCH_MODE_AGENT
    {
        return Err(BatchError::bad(format!(
            "batch jobs only support mode '{BATCH_MODE_AGENT}' (got '{mode}')"
        )));
    }
    let assistant_id = req.assistant_id.trim().to_string();
    if assistant_id.is_empty() {
        return Err(BatchError::bad("assistant_id is required"));
    }

    let concurrency = req.concurrency.unwrap_or(BATCH_DEFAULT_CONCURRENCY);
    if !(1..=BATCH_MAX_CONCURRENCY).contains(&concurrency) {
        return Err(BatchError::bad(format!(
            "concurrency must be between 1 and {BATCH_MAX_CONCURRENCY}"
        )));
    }
    let item_timeout_sec = req.item_timeout_sec.unwrap_or(BATCH_DEFAULT_ITEM_TIMEOUT_SEC);
    if !(BATCH_MIN_ITEM_TIMEOUT_SEC..=BATCH_MAX_ITEM_TIMEOUT_SEC).contains(&item_timeout_sec) {
        return Err(BatchError::bad(format!(
            "item_timeout_sec must be between {BATCH_MIN_ITEM_TIMEOUT_SEC} and {BATCH_MAX_ITEM_TIMEOUT_SEC}"
        )));
    }

    if req.targets.is_empty() {
        return Err(BatchError::bad("targets must not be empty"));
    }
    if req.targets.len() > BATCH_MAX_TARGETS {
        return Err(BatchError::bad(format!(
            "at most {BATCH_MAX_TARGETS} targets are allowed"
        )));
    }
    let trial_size = req.trial_size.unwrap_or(BATCH_DEFAULT_TRIAL_SIZE);
    if !(1..=BATCH_MAX_TRIAL_SIZE).contains(&trial_size) {
        return Err(BatchError::bad(format!(
            "trial_size must be between 1 and {BATCH_MAX_TRIAL_SIZE} (the trial is mandatory)"
        )));
    }
    if trial_size as usize > req.targets.len() {
        return Err(BatchError::bad("trial_size must not exceed the number of targets"));
    }

    let template = PromptTemplate::parse(&req.template.prompt)?;
    let mut locations = Vec::with_capacity(req.targets.len());
    for (i, target) in req.targets.iter().enumerate() {
        let seq = i as i64 + 1;
        let loc = target_location(target).map_err(|e| BatchError::bad(format!("targets[{i}]: {e}")))?;
        if target.to_string().len() > MAX_TARGET_BYTES {
            return Err(BatchError::bad(format!(
                "targets[{i}] exceeds {MAX_TARGET_BYTES} bytes"
            )));
        }
        template.render(target, seq)?;
        locations.push(loc);
    }

    let compiled_checks = CompiledChecks::compile(&req.checks)?;
    let non_empty = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    Ok(ValidatedBatchJob {
        title,
        prompt: req.template.prompt,
        template,
        targets: req.targets,
        locations,
        checks: req.checks,
        compiled_checks,
        assistant_id,
        agent_type: non_empty(req.agent_type),
        project_id: non_empty(req.project_id),
        concurrency,
        item_timeout_sec,
        trial_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(v: Value) -> CreateBatchJobRequest {
        let mut base = json!({
            "title": "Release check",
            "template": {"prompt": "Run the release check for {{target.name}}"},
            "targets": [{"repo": "/tmp/r1", "name": "r1"}, {"workspace": "/tmp/w2", "name": "w2"}],
            "checks": [{"kind": "file_exists", "path": "release-check.json"}],
            "assistant_id": "asst-1"
        });
        for (k, val) in v.as_object().unwrap() {
            base[k] = val.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn defaults_follow_accepted_decisions() {
        let v = validate_create(req(json!({}))).unwrap();
        assert_eq!(v.concurrency, 4);
        assert_eq!(v.item_timeout_sec, 1800);
        assert_eq!(v.trial_size, 1);
        assert_eq!(v.locations[0], TargetLocation::Repo("/tmp/r1".into()));
        assert_eq!(v.locations[1], TargetLocation::Workspace("/tmp/w2".into()));
        assert_eq!(v.compiled_checks.len(), 1);
    }

    #[test]
    fn rejects_out_of_range_and_non_agent_mode() {
        for bad in [
            json!({"mode": "plan"}),
            json!({"concurrency": 0}),
            json!({"concurrency": 17}),
            json!({"item_timeout_sec": 10}),
            json!({"item_timeout_sec": 99999}),
            json!({"trial_size": 0}),
            json!({"trial_size": 6}),
            json!({"trial_size": 3}),
            json!({"title": "  "}),
            json!({"assistant_id": ""}),
            json!({"targets": []}),
            json!({"checks": []}),
        ] {
            assert!(validate_create(req(bad.clone())).is_err(), "{bad}");
        }
        assert!(validate_create(req(json!({"mode": "agent", "concurrency": 16}))).is_ok());
    }

    #[test]
    fn rejects_bad_targets() {
        for bad in [
            json!({"targets": ["just-a-string"]}),
            json!({"targets": [{"name": "x"}]}),
            json!({"targets": [{"repo": "relative/path", "name": "x"}]}),
            json!({"targets": [{"repo": "/a", "workspace": "/b", "name": "x"}]}),
            json!({"targets": [{"repo": 7, "name": "x"}]}),
            json!({"targets": [{"repo": "/a"}]}),
        ] {
            assert!(validate_create(req(bad.clone())).is_err(), "{bad}");
        }
        let too_many: Vec<Value> = (0..=BATCH_MAX_TARGETS)
            .map(|i| json!({"repo": "/r", "name": i}))
            .collect();
        assert!(validate_create(req(json!({"targets": too_many}))).is_err());
    }
}
