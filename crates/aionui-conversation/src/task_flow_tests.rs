use super::*;
use serde_json::json;

fn flow(value: serde_json::Value) -> CreateTaskFlowFields {
    serde_json::from_value(value).unwrap()
}

#[test]
fn defaults_source_from_the_conversation_binding() {
    let empty = CreateTaskFlowFields::default();
    assert_eq!(
        validate_task_flow(&empty, false, None).unwrap().source,
        TaskSource::Manual
    );
    assert_eq!(
        validate_task_flow(&empty, true, None).unwrap().source,
        TaskSource::Conversation
    );
}

#[test]
fn clients_cannot_claim_core_sources() {
    for source in ["github", "cron", "agent"] {
        let err = validate_task_flow(&flow(json!({ "source": source })), false, None).unwrap_err();
        assert!(err.to_string().contains("set by Core"), "{source}: {err}");
    }
    let core = validate_task_flow(&flow(json!({ "source": "manual" })), false, Some(TaskSource::Github)).unwrap();
    assert_eq!(core.source, TaskSource::Github);
}

#[test]
fn stores_trimmed_values_and_the_prompt_object() {
    let validated = validate_task_flow(
        &flow(json!({
            "assistant_id": "  codex-helper ",
            "prompt": { "text": "fix login", "files": ["a.ts"] },
            "repo_path": "/repo/app",
            "base_ref": "release/3.0",
            "plan_first": true,
            "source": "conversation",
            "source_ref": "msg-1"
        })),
        true,
        None,
    )
    .unwrap();
    assert_eq!(validated.assistant_id.as_deref(), Some("codex-helper"));
    assert_eq!(validated.repo_path.as_deref(), Some("/repo/app"));
    assert_eq!(validated.base_ref.as_deref(), Some("release/3.0"));
    assert!(validated.plan_first);
    let prompt: serde_json::Value = serde_json::from_str(validated.prompt_json.as_deref().unwrap()).unwrap();
    assert_eq!(prompt["text"], "fix login");
}

#[test]
fn rejects_bad_values() {
    let cases = [
        json!({ "prompt": "text" }),
        json!({ "prompt": { "text": "x".repeat(TASK_PROMPT_MAX_BYTES) } }),
        json!({ "repo_path": "relative/path" }),
        json!({ "repo_path": "   " }),
        json!({ "assistant_id": "a\u{0}b" }),
        json!({ "base_ref": "--upload-pack=x" }),
        json!({ "base_ref": "main..evil" }),
        json!({ "base_ref": "a b" }),
    ];
    for case in cases {
        assert!(
            validate_task_flow(&flow(case.clone()), false, None).is_err(),
            "{case} should be rejected"
        );
    }
}

#[test]
fn git_ref_check_matches_common_refs() {
    for good in ["main", "release/3.0", "feature/x-1", "v1.2.3", "HEAD"] {
        assert!(is_safe_git_ref(good), "{good}");
    }
    for bad in [
        "", "-x", "/main", "main/", "a..b", "a.lock", "a@{1}", "@", "a~1", "a^", "a:b", "a?", "a*", "a[", "a\\b",
        ".hidden", "a/.b", "a//b", "a.",
    ] {
        assert!(!is_safe_git_ref(bad), "{bad}");
    }
}
