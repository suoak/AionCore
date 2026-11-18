use super::*;
use aionui_api_types::{TaskLane, TaskNeedsYouReason, TaskSource};

#[test]
fn lanes_follow_plan_b_table() {
    use TaskBoardStatus::*;
    let cases = [
        (Backlog, "draft", TaskLane::Todo, None),
        (Todo, "ready", TaskLane::Todo, None),
        (Todo, "running", TaskLane::InProgress, None),
        (InProgress, "running", TaskLane::InProgress, None),
        (InProgress, "ready", TaskLane::InProgress, None),
        (
            InProgress,
            "waiting_approval",
            TaskLane::NeedsYou,
            Some(TaskNeedsYouReason::PlanApproval),
        ),
        (
            Todo,
            "waiting_approval",
            TaskLane::NeedsYou,
            Some(TaskNeedsYouReason::PlanApproval),
        ),
        (
            InProgress,
            "paused",
            TaskLane::NeedsYou,
            Some(TaskNeedsYouReason::Input),
        ),
        (
            InProgress,
            "failed",
            TaskLane::NeedsYou,
            Some(TaskNeedsYouReason::Failed),
        ),
        (
            InReview,
            "completed",
            TaskLane::NeedsYou,
            Some(TaskNeedsYouReason::Review),
        ),
        (Done, "completed", TaskLane::Done, None),
        (Done, "failed", TaskLane::Done, None),
        (Cancelled, "failed", TaskLane::Cancelled, None),
    ];
    for (board, run, lane, reason) in cases {
        assert_eq!(derive_lane(board, run), (lane, reason), "{board:?} / {run}");
    }
}

#[test]
fn source_is_stored_or_inferred() {
    assert_eq!(task_source(Some("cron"), None, None), TaskSource::Cron);
    assert_eq!(task_source(None, Some("github:o/r#1"), Some("c")), TaskSource::Github);
    assert_eq!(task_source(None, None, Some("c")), TaskSource::Conversation);
    assert_eq!(task_source(None, None, None), TaskSource::Manual);
    assert_eq!(task_source(Some("bogus"), None, None), TaskSource::Manual);
}

#[test]
fn plan_summary_counts_steps_and_picks_the_current_one() {
    let content = serde_json::json!({
        "entries": [
            {"content": "Read code", "status": "completed"},
            {"content": "Write fix", "status": "in_progress"},
            {"content": "Run tests", "status": "pending"}
        ],
        "turn_id": "t1"
    })
    .to_string();
    let plan = plan_summary(&content).unwrap();
    assert_eq!((plan.done, plan.total), (1, 3));
    assert_eq!(plan.current.as_deref(), Some("Write fix"));

    let pending = serde_json::json!({"entries": [{"content": "A", "status": "completed"}, {"content": "B"}]});
    assert_eq!(
        plan_summary(&pending.to_string()).unwrap().current.as_deref(),
        Some("B")
    );

    let finished = serde_json::json!({"entries": [{"content": "A", "status": "completed"}]});
    let plan = plan_summary(&finished.to_string()).unwrap();
    assert_eq!((plan.done, plan.total, plan.current), (1, 1, None));
}

#[test]
fn plan_summary_ignores_non_plans_and_truncates_long_steps() {
    assert!(plan_summary("not json").is_none());
    assert!(plan_summary("{}").is_none());
    assert!(plan_summary(r#"{"entries":[]}"#).is_none());
    let long = "x".repeat(PLAN_STEP_MAX_CHARS + 10);
    let content = serde_json::json!({"entries": [{"content": long, "status": "pending"}]}).to_string();
    let current = plan_summary(&content).unwrap().current.unwrap();
    assert_eq!(current.chars().count(), PLAN_STEP_MAX_CHARS + 1);
    assert!(current.ends_with('…'));
}
