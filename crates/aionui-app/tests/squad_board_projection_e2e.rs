//! M8 S5 debt: Squad run → board/work-item projection, end to end over the
//! real app (SQLite repos, team service, board service and the composition
//! relay). Earlier coverage used fake ports only.
//!
//! - an instance started for a work item (E7-6) puts `squad_run` on that
//!   card only; unrelated cards carry none;
//! - the card follows the derived state of the instance's team tasks;
//! - the newest instance for a work item wins;
//! - team task events reach the board as `task.board.updated` with
//!   `change: "squad_run"`, deduplicated, and ignored for superseded
//!   instances.

mod common;

use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{Value, json};
use tower::ServiceExt;

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use common::{body_json, build_app, get_with_token, json_with_token, setup_and_login};

const AGENT_ID: &str = "2d23ff1c";
const LEAD: &str = "squad-proj-lead";
const MEMBER: &str = "squad-proj-member";

struct Ctx {
    app: axum::Router,
    services: aionui_app::AppServices,
    token: String,
    csrf: String,
}

impl Ctx {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = match body {
            Some(body) => json_with_token(method, path, body, &self.token, &self.csrf),
            None => get_with_token(path, &self.token),
        };
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    async fn work_item(&self, title: &str) -> String {
        let (s, v) = self
            .call(
                "POST",
                "/api/task-sessions",
                Some(json!({ "title": title, "objective": "o", "mode": "agent", "agent_type": "claude" })),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["data"]["id"].as_str().unwrap().to_owned()
    }

    async fn board_card(&self, id: &str) -> Value {
        let (s, v) = self.call("GET", "/api/task-sessions/board", None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v["data"]["columns"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["items"].as_array().unwrap().iter())
            .find(|item| item["id"] == id)
            .cloned()
            .unwrap_or_else(|| panic!("card {id} not on board: {v}"))
    }

    async fn instantiate(&self, squad_id: &str, work_item_id: &str) -> String {
        let (s, v) = self
            .call(
                "POST",
                &format!("/api/squads/{squad_id}/instantiate"),
                Some(json!({ "work_item_id": work_item_id })),
            )
            .await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        assert_eq!(v["data"]["work_item_id"], work_item_id);
        v["data"]["team"]["id"].as_str().unwrap().to_owned()
    }

    async fn insert_task(&self, team_id: &str, id: &str, status: &str) {
        sqlx::query(
            "INSERT INTO team_tasks \
             (id, team_id, subject, description, status, owner, blocked_by, blocks, metadata, created_at, updated_at) \
             VALUES (?, ?, 'subject', NULL, ?, NULL, '[]', '[]', NULL, 10, 10)",
        )
        .bind(id)
        .bind(team_id)
        .bind(status)
        .execute(self.services.database.pool())
        .await
        .unwrap();
    }

    async fn set_task(&self, id: &str, status: &str) {
        sqlx::query("UPDATE team_tasks SET status = ?, updated_at = updated_at + 1 WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(self.services.database.pool())
            .await
            .unwrap();
    }

    async fn user_id(&self) -> String {
        sqlx::query_scalar("SELECT user_id FROM task_sessions LIMIT 1")
            .fetch_one(self.services.database.pool())
            .await
            .unwrap()
    }

    fn team_task_changed(&self, user_id: &str, team_id: &str) {
        self.services.event_bus.broadcast(WebSocketMessage::new(
            "team.taskChanged",
            json!({ "user_id": user_id, "team_id": team_id }),
        ));
    }
}

async fn ensure_assistants(ctx: &mut Ctx) {
    let command = std::env::current_exe().unwrap().to_string_lossy().to_string();
    let source_info = json!({ "binary_name": command }).to_string();
    sqlx::query(
        "UPDATE agent_metadata \
         SET agent_source = 'custom', agent_source_info = ?, command = ?, args = '[]', env = '[]', \
             updated_at = unixepoch('now','subsec') * 1000 \
         WHERE agent_id = ?",
    )
    .bind(&source_info)
    .bind(&command)
    .bind(AGENT_ID)
    .execute(ctx.services.database.pool())
    .await
    .unwrap();
    ctx.services.agent_registry.reload_one(AGENT_ID).await.unwrap();
    for (id, name) in [(LEAD, "Squad Lead"), (MEMBER, "Squad Member")] {
        let (s, v) = ctx
            .call(
                "POST",
                "/api/assistants",
                Some(json!({ "id": id, "name": name, "agent_id": AGENT_ID })),
            )
            .await;
        assert!(s == StatusCode::CREATED || s == StatusCode::CONFLICT, "{s} {v}");
    }
}

async fn setup() -> (Ctx, String) {
    let (mut app, services) = build_app().await;
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "StrongP@ss1").await;
    let mut ctx = Ctx {
        app,
        services,
        token,
        csrf,
    };
    ensure_assistants(&mut ctx).await;
    let (s, v) = ctx
        .call(
            "POST",
            "/api/squads",
            Some(json!({
                "name": "proj",
                "leader_assistant_id": LEAD,
                "leader_mode": "route_and_work",
                "members": [{ "assistant_id": MEMBER, "role_title": "dev" }]
            })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let squad_id = v["data"]["id"].as_str().unwrap().to_owned();
    (ctx, squad_id)
}

async fn assign_to_squad(ctx: &Ctx, work_item_id: &str, squad_id: &str) {
    let (s, v) = ctx
        .call(
            "PATCH",
            &format!("/api/task-sessions/{work_item_id}/board"),
            Some(json!({ "board_status": "todo", "assignee": { "kind": "squad", "id": squad_id } })),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

async fn next_squad_run_event(
    rx: &mut tokio::sync::broadcast::Receiver<WebSocketMessage<Value>>,
    work_item_id: &str,
) -> Option<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let msg = match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
            _ => return None,
        };
        if msg.name == "task.board.updated"
            && msg.data["change"] == "squad_run"
            && msg.data["task_session_id"] == work_item_id
        {
            return Some(msg.data);
        }
    }
}

#[tokio::test]
async fn squad_run_projects_onto_linked_card_only_and_follows_team_tasks() {
    let (ctx, squad_id) = setup().await;
    let linked = ctx.work_item("linked").await;
    let other = ctx.work_item("other").await;
    assign_to_squad(&ctx, &linked, &squad_id).await;

    // Backlog never starts a Squad.
    let (s, v) = ctx
        .call(
            "POST",
            &format!("/api/squads/{squad_id}/instantiate"),
            Some(json!({ "work_item_id": other })),
        )
        .await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    let team = ctx.instantiate(&squad_id, &linked).await;
    // No tasks and no run yet: nothing to show.
    assert!(ctx.board_card(&linked).await["squad_run"].is_null());

    ctx.insert_task(&team, "t1", "pending").await;
    ctx.insert_task(&team, "t2", "pending").await;
    let card = ctx.board_card(&linked).await;
    assert_eq!(card["squad_run"]["team_id"], team, "{card}");
    assert_eq!(card["squad_run"]["state"], "waiting_backlog", "{card}");
    assert_eq!(card["squad_run"]["leader_mode"], "route_and_work", "{card}");
    assert!(
        ctx.board_card(&other).await["squad_run"].is_null(),
        "unlinked card has no squad_run"
    );

    // Team endpoint and board card agree.
    let (s, v) = ctx.call("GET", &format!("/api/teams/{team}/squad-run"), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["state"], "waiting_backlog", "{v}");

    ctx.set_task("t1", "in_progress").await;
    assert_eq!(ctx.board_card(&linked).await["squad_run"]["state"], "paused");
    ctx.set_task("t1", "completed").await;
    ctx.set_task("t2", "deleted").await;
    assert_eq!(ctx.board_card(&linked).await["squad_run"]["state"], "finished");

    ctx.services.database.close().await;
}

#[tokio::test]
async fn newest_instance_wins_and_relay_pushes_deduplicated_squad_run_changes() {
    let (ctx, squad_id) = setup().await;
    let linked = ctx.work_item("linked").await;
    assign_to_squad(&ctx, &linked, &squad_id).await;
    let user_id = ctx.user_id().await;

    let first = ctx.instantiate(&squad_id, &linked).await;
    ctx.insert_task(&first, "a1", "in_progress").await;
    assert_eq!(ctx.board_card(&linked).await["squad_run"]["team_id"], first);

    // Make sure the second instance is strictly newer.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = ctx.instantiate(&squad_id, &linked).await;
    assert_ne!(first, second);
    ctx.insert_task(&second, "b1", "pending").await;
    let card = ctx.board_card(&linked).await;
    assert_eq!(
        card["squad_run"]["team_id"], second,
        "newest instance drives the card: {card}"
    );
    assert_eq!(card["squad_run"]["state"], "waiting_backlog");

    let mut rx = ctx.services.event_bus.subscribe();

    // A superseded instance's event does not touch the card.
    ctx.team_task_changed(&user_id, &first);
    // The live instance's event pushes the card with change=squad_run.
    ctx.team_task_changed(&user_id, &second);
    let event = next_squad_run_event(&mut rx, &linked).await.expect("squad_run push");
    assert_eq!(event["item"]["squad_run"]["team_id"], second, "{event}");
    assert_eq!(event["item"]["squad_run"]["state"], "waiting_backlog", "{event}");
    assert_eq!(event["from_board_status"], "todo", "card did not move: {event}");
    assert_eq!(event["item"]["board_status"], "todo", "{event}");

    // Same state again: deduplicated.
    ctx.team_task_changed(&user_id, &second);
    assert!(next_squad_run_event(&mut rx, &linked).await.is_none(), "no repeat push");

    // State change: pushed again.
    ctx.set_task("b1", "completed").await;
    ctx.team_task_changed(&user_id, &second);
    let event = next_squad_run_event(&mut rx, &linked).await.expect("finished push");
    assert_eq!(event["item"]["squad_run"]["state"], "finished", "{event}");

    ctx.services.database.close().await;
}
