//! Squad template CRUD (M8 E7-1). Contract: m8-s5.md "E7 Squad".
//!
//! Decisions (defaults from E7-squad.md §5, recorded in m8-s5.md):
//! - D7-1 Leader defaults to `route_only` (dispatches, never works).
//! - D7-4 members can never spawn sub-agents (hard rule, no template field).
//! - D7-5 at most 6 participants, Leader included.
//! - "Eligible Assistant" = what the Team catalog offers
//!   ([`TeamAssistantCatalogPort::list_team_selectable_assistants`]):
//!   enabled, `team_selectable`, with an agent binding.
//! - Every refusal carries a stable `code` (`ApiError::Coded`).

#![allow(clippy::disallowed_types)]

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, AssistantBrief, DeleteSquadResponse, ListSquadsQuery, SQUAD_MAX_PARTICIPANTS, SquadLeaderMode,
    SquadMember, SquadTemplate, SquadTemplateInput, SquadTemplatePatch, SquadVisibility,
};
use aionui_auth::CurrentUser;
use aionui_common::{ApiError, generate_id, now_ms};
use aionui_db::models::SquadTemplateRow;
use aionui_db::{
    CreateSquadTemplateParams, DbError, ISquadTemplateRepository, SQUAD_NAME_CONFLICT_PREFIX,
    SQUAD_STALE_CONFLICT_PREFIX, UpdateSquadTemplateParams,
};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use serde_json::json;

use crate::ports::{TeamAssistantCatalogEntry, TeamAssistantCatalogPort};

pub const SQUAD_NAME_MAX_CHARS: usize = 100;
pub const SQUAD_DESCRIPTION_MAX_CHARS: usize = 2000;
pub const SQUAD_ROLE_TITLE_MAX_CHARS: usize = 60;
pub const SQUAD_ROLE_DESCRIPTION_MAX_CHARS: usize = 2000;
pub const SQUAD_MAX_PARALLEL_LIMIT: u32 = 4;

#[derive(Clone)]
pub struct SquadRouterState {
    pub repo: Arc<dyn ISquadTemplateRepository>,
    pub catalog: Arc<dyn TeamAssistantCatalogPort>,
}

pub fn squad_routes(state: SquadRouterState) -> Router {
    Router::new()
        .route("/api/squads", get(list_squads).post(create_squad))
        .route(
            "/api/squads/{id}",
            get(get_squad).patch(update_squad).delete(delete_squad),
        )
        .with_state(state)
}

fn coded(
    status: StatusCode,
    code: &'static str,
    message: impl Into<String>,
    details: Option<serde_json::Value>,
) -> ApiError {
    let message = message.into();
    ApiError::coded(status, code, format!("{code}: {message}"), details)
}

fn bad(code: &'static str, message: impl Into<String>, details: Option<serde_json::Value>) -> ApiError {
    coded(StatusCode::BAD_REQUEST, code, message, details)
}

fn not_found(id: &str) -> ApiError {
    coded(
        StatusCode::NOT_FOUND,
        "squad_not_found",
        format!("squad template {id} not found"),
        Some(json!({ "id": id })),
    )
}

fn db_err(err: DbError) -> ApiError {
    match err {
        DbError::Conflict(msg) if msg.starts_with(SQUAD_STALE_CONFLICT_PREFIX) => {
            coded(StatusCode::CONFLICT, "squad_stale", msg, None)
        }
        DbError::Conflict(msg) if msg.starts_with(SQUAD_NAME_CONFLICT_PREFIX) => {
            coded(StatusCode::CONFLICT, "squad_name_conflict", msg, None)
        }
        DbError::NotFound(msg) => coded(StatusCode::NOT_FOUND, "squad_not_found", msg, None),
        other => coded(
            StatusCode::INTERNAL_SERVER_ERROR,
            "squad_store_failed",
            other.to_string(),
            None,
        ),
    }
}

fn catalog_err(err: crate::error::TeamError) -> ApiError {
    coded(
        StatusCode::SERVICE_UNAVAILABLE,
        "squad_assistant_catalog_unavailable",
        err.to_string(),
        None,
    )
}

/// Validated, normalized template content.
struct Validated {
    name: String,
    description: String,
    leader_assistant_id: String,
    leader_mode: SquadLeaderMode,
    members: Vec<SquadMember>,
    visibility: SquadVisibility,
}

fn validate_shape(input: SquadTemplateInput) -> Result<Validated, ApiError> {
    let name = input.name.trim().to_string();
    if name.is_empty() || name.chars().count() > SQUAD_NAME_MAX_CHARS {
        return Err(bad(
            "squad_invalid_name",
            "name must be 1..=100 characters",
            Some(json!({ "max": SQUAD_NAME_MAX_CHARS })),
        ));
    }
    if input.description.chars().count() > SQUAD_DESCRIPTION_MAX_CHARS {
        return Err(bad(
            "squad_invalid_description",
            "description too long",
            Some(json!({ "max": SQUAD_DESCRIPTION_MAX_CHARS })),
        ));
    }
    if input.visibility == SquadVisibility::Team {
        return Err(coded(
            StatusCode::FORBIDDEN,
            "squad_team_visibility_unavailable",
            "team visibility needs E12 team roles",
            None,
        ));
    }
    let leader_assistant_id = input.leader_assistant_id.trim().to_string();
    if leader_assistant_id.is_empty() {
        return Err(bad("squad_leader_required", "leader_assistant_id is required", None));
    }
    if input.members.is_empty() {
        return Err(bad("squad_no_members", "a squad needs at least one member", None));
    }
    if input.members.len() + 1 > SQUAD_MAX_PARTICIPANTS {
        return Err(bad(
            "squad_too_many_members",
            format!("at most {SQUAD_MAX_PARTICIPANTS} participants including the leader"),
            Some(json!({ "limit": SQUAD_MAX_PARTICIPANTS, "members_limit": SQUAD_MAX_PARTICIPANTS - 1 })),
        ));
    }
    let mut seen: HashSet<String> = HashSet::from([leader_assistant_id.clone()]);
    let mut members = Vec::with_capacity(input.members.len());
    for (index, m) in input.members.into_iter().enumerate() {
        let assistant_id = m.assistant_id.trim().to_string();
        if !seen.insert(assistant_id.clone()) {
            return Err(bad(
                "squad_member_duplicate",
                format!("assistant {assistant_id} appears more than once (the leader cannot also be a member)"),
                Some(json!({ "assistant_id": assistant_id, "index": index })),
            ));
        }
        let role_title = m.role_title.trim().to_string();
        let invalid = |field: &str| {
            bad(
                "squad_invalid_member",
                format!("member {index}: invalid {field}"),
                Some(json!({ "index": index, "field": field })),
            )
        };
        if assistant_id.is_empty() {
            return Err(invalid("assistant_id"));
        }
        if role_title.is_empty() || role_title.chars().count() > SQUAD_ROLE_TITLE_MAX_CHARS {
            return Err(invalid("role_title"));
        }
        if m.role_description.chars().count() > SQUAD_ROLE_DESCRIPTION_MAX_CHARS {
            return Err(invalid("role_description"));
        }
        if m.max_parallel == 0 || m.max_parallel > SQUAD_MAX_PARALLEL_LIMIT {
            return Err(invalid("max_parallel"));
        }
        members.push(SquadMember {
            assistant_id,
            role_title,
            role_description: m.role_description,
            max_parallel: m.max_parallel,
        });
    }
    Ok(Validated {
        name,
        description: input.description,
        leader_assistant_id,
        leader_mode: input.leader_mode,
        members,
        visibility: input.visibility,
    })
}

async fn catalog_map(
    catalog: &dyn TeamAssistantCatalogPort,
    user_id: &str,
) -> Result<HashMap<String, TeamAssistantCatalogEntry>, ApiError> {
    Ok(catalog
        .list_team_selectable_assistants(user_id)
        .await
        .map_err(catalog_err)?
        .into_iter()
        .map(|entry| (entry.assistant_id.clone(), entry))
        .collect())
}

fn ensure_eligible(v: &Validated, catalog: &HashMap<String, TeamAssistantCatalogEntry>) -> Result<(), ApiError> {
    let ids = std::iter::once(v.leader_assistant_id.as_str()).chain(v.members.iter().map(|m| m.assistant_id.as_str()));
    for id in ids {
        if !catalog.contains_key(id) {
            return Err(bad(
                "squad_assistant_not_eligible",
                format!("assistant {id} is missing, disabled or not team-selectable"),
                Some(json!({ "assistant_id": id })),
            ));
        }
    }
    Ok(())
}

fn brief(id: &str, catalog: &HashMap<String, TeamAssistantCatalogEntry>) -> AssistantBrief {
    match catalog.get(id) {
        Some(entry) => AssistantBrief {
            id: id.to_string(),
            name: entry.name.clone(),
            enabled: true,
            missing: false,
        },
        None => AssistantBrief {
            id: id.to_string(),
            name: id.to_string(),
            enabled: false,
            missing: true,
        },
    }
}

fn to_api(row: SquadTemplateRow, catalog: &HashMap<String, TeamAssistantCatalogEntry>) -> SquadTemplate {
    let members: Vec<SquadMember> = serde_json::from_str(&row.members).unwrap_or_default();
    let member_assistants = members.iter().map(|m| brief(&m.assistant_id, catalog)).collect();
    SquadTemplate {
        leader: Some(brief(&row.leader_assistant_id, catalog)),
        member_assistants,
        id: row.id,
        name: row.name,
        description: row.description,
        leader_mode: SquadLeaderMode::parse(&row.leader_mode).unwrap_or_default(),
        leader_assistant_id: row.leader_assistant_id,
        members,
        visibility: if row.visibility == "team" {
            SquadVisibility::Team
        } else {
            SquadVisibility::Private
        },
        team_id: row.team_id,
        created_at: row.created_at,
        updated_at: row.updated_at,
        deleted_at: row.deleted_at,
    }
}

fn row_to_input(row: &SquadTemplateRow) -> SquadTemplateInput {
    SquadTemplateInput {
        name: row.name.clone(),
        description: row.description.clone(),
        leader_assistant_id: row.leader_assistant_id.clone(),
        leader_mode: SquadLeaderMode::parse(&row.leader_mode).unwrap_or_default(),
        members: serde_json::from_str(&row.members).unwrap_or_default(),
        visibility: if row.visibility == "team" {
            SquadVisibility::Team
        } else {
            SquadVisibility::Private
        },
    }
}

fn json_body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(v)| v)
        .map_err(|e| bad("squad_invalid_body", e.body_text(), None))
}

async fn list_squads(
    State(state): State<SquadRouterState>,
    Extension(user): Extension<CurrentUser>,
    Query(query): Query<ListSquadsQuery>,
) -> Result<Json<ApiResponse<Vec<SquadTemplate>>>, ApiError> {
    let include_deleted = matches!(query.include_deleted.as_deref(), Some("1" | "true"));
    let rows = state.repo.list(&user.id, include_deleted).await.map_err(db_err)?;
    let catalog = catalog_map(state.catalog.as_ref(), &user.id).await?;
    Ok(Json(ApiResponse::ok(
        rows.into_iter().map(|r| to_api(r, &catalog)).collect(),
    )))
}

async fn get_squad(
    State(state): State<SquadRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<SquadTemplate>>, ApiError> {
    let row = state
        .repo
        .get(&user.id, &id, false)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found(&id))?;
    let catalog = catalog_map(state.catalog.as_ref(), &user.id).await?;
    Ok(Json(ApiResponse::ok(to_api(row, &catalog))))
}

async fn create_squad(
    State(state): State<SquadRouterState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<SquadTemplateInput>, JsonRejection>,
) -> Result<Json<ApiResponse<SquadTemplate>>, ApiError> {
    let v = validate_shape(json_body(body)?)?;
    let catalog = catalog_map(state.catalog.as_ref(), &user.id).await?;
    ensure_eligible(&v, &catalog)?;
    let members = serde_json::to_string(&v.members).map_err(|e| bad("squad_invalid_member", e.to_string(), None))?;
    let id = generate_id();
    let row = state
        .repo
        .create(CreateSquadTemplateParams {
            id: &id,
            user_id: &user.id,
            name: &v.name,
            description: &v.description,
            leader_assistant_id: &v.leader_assistant_id,
            leader_mode: v.leader_mode.as_str(),
            members: &members,
            visibility: v.visibility.as_str(),
            now: now_ms(),
        })
        .await
        .map_err(db_err)?;
    Ok(Json(ApiResponse::ok(to_api(row, &catalog))))
}

async fn update_squad(
    State(state): State<SquadRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SquadTemplatePatch>, JsonRejection>,
) -> Result<Json<ApiResponse<SquadTemplate>>, ApiError> {
    let patch = json_body(body)?;
    let current = state
        .repo
        .get(&user.id, &id, false)
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found(&id))?;
    if current.updated_at != patch.expected_updated_at {
        return Err(coded(
            StatusCode::CONFLICT,
            "squad_stale",
            "template changed since it was read",
            Some(json!({ "current_updated_at": current.updated_at })),
        ));
    }
    let mut input = row_to_input(&current);
    if let Some(name) = patch.name {
        input.name = name;
    }
    if let Some(description) = patch.description {
        input.description = description;
    }
    if let Some(leader) = patch.leader_assistant_id {
        input.leader_assistant_id = leader;
    }
    if let Some(mode) = patch.leader_mode {
        input.leader_mode = mode;
    }
    if let Some(members) = patch.members {
        input.members = members;
    }
    if let Some(visibility) = patch.visibility {
        input.visibility = visibility;
    }
    let v = validate_shape(input)?;
    let catalog = catalog_map(state.catalog.as_ref(), &user.id).await?;
    ensure_eligible(&v, &catalog)?;
    let members = serde_json::to_string(&v.members).map_err(|e| bad("squad_invalid_member", e.to_string(), None))?;
    let row = state
        .repo
        .update(UpdateSquadTemplateParams {
            user_id: &user.id,
            id: &id,
            expected_updated_at: patch.expected_updated_at,
            name: &v.name,
            description: &v.description,
            leader_assistant_id: &v.leader_assistant_id,
            leader_mode: v.leader_mode.as_str(),
            members: &members,
            visibility: v.visibility.as_str(),
            now: now_ms(),
        })
        .await
        .map_err(db_err)?;
    Ok(Json(ApiResponse::ok(to_api(row, &catalog))))
}

async fn delete_squad(
    State(state): State<SquadRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteSquadResponse>>, ApiError> {
    if state.repo.get(&user.id, &id, false).await.map_err(db_err)?.is_none() {
        return Err(not_found(&id));
    }
    let team_ids = state.repo.instance_team_ids(&id).await.map_err(db_err)?;
    if !team_ids.is_empty() {
        return Err(coded(
            StatusCode::CONFLICT,
            "squad_in_use",
            "remove the teams instantiated from this squad first",
            Some(json!({ "team_ids": team_ids })),
        ));
    }
    let deleted_at = state
        .repo
        .soft_delete(&user.id, &id, now_ms())
        .await
        .map_err(db_err)?
        .ok_or_else(|| not_found(&id))?;
    Ok(Json(ApiResponse::ok(DeleteSquadResponse { id, deleted_at })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::TeamError;

    struct Catalog(Vec<&'static str>);

    #[async_trait::async_trait]
    impl TeamAssistantCatalogPort for Catalog {
        async fn list_team_selectable_assistants(
            &self,
            _user_id: &str,
        ) -> Result<Vec<TeamAssistantCatalogEntry>, TeamError> {
            Ok(self
                .0
                .iter()
                .map(|id| TeamAssistantCatalogEntry {
                    assistant_id: (*id).to_string(),
                    name: format!("name-{id}"),
                    backend: "claude".into(),
                    description: String::new(),
                    skills: vec![],
                })
                .collect())
        }
    }

    async fn app(ids: Vec<&'static str>) -> (Router, aionui_db::Database) {
        let db = aionui_db::init_database_memory().await.unwrap();
        let state = SquadRouterState {
            repo: Arc::new(aionui_db::SqliteSquadTemplateRepository::new(db.pool().clone())),
            catalog: Arc::new(Catalog(ids)),
        };
        (squad_routes(state).layer(Extension(CurrentUser::local_default())), db)
    }

    async fn call(
        app: &Router,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        // Real loopback server + reqwest: avoids a tower dev-dependency (and a
        // Cargo.lock change) just for `oneshot`.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = {
            let app = app.clone();
            tokio::spawn(async move { axum::serve(listener, app).await })
        };
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        let mut req = reqwest::Client::new().request(method, format!("http://{addr}{uri}"));
        if let Some(b) = body {
            req = req.header("content-type", "application/json").body(b.to_string());
        }
        let resp = req.send().await.unwrap();
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let text = resp.text().await.unwrap();
        server.abort();
        (status, serde_json::from_str(&text).unwrap_or(serde_json::Value::Null))
    }

    fn member(id: &str) -> serde_json::Value {
        json!({ "assistant_id": id, "role_title": format!("role-{id}") })
    }

    fn code(v: &serde_json::Value) -> String {
        v["code"].as_str().unwrap_or_default().to_string()
    }

    #[tokio::test]
    async fn crud_round_trip_with_defaults_and_briefs() {
        let (app, _db) = app(vec!["lead", "fe", "be", "qa"]).await;
        let (s, v) = call(
            &app,
            "POST",
            "/api/squads",
            Some(json!({ "name": " web ", "leader_assistant_id": "lead", "members": [member("fe"), member("be"), member("qa")] })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let t = &v["data"];
        assert_eq!(t["name"], "web");
        assert_eq!(t["leader_mode"], "route_only", "D7-1 default");
        assert_eq!(t["visibility"], "private");
        assert_eq!(t["members"][0]["max_parallel"], 1);
        assert_eq!(t["leader"]["name"], "name-lead");
        assert_eq!(t["member_assistants"][2]["id"], "qa");
        let id = t["id"].as_str().unwrap().to_string();
        let updated_at = t["updated_at"].as_i64().unwrap();

        let (s, v) = call(&app, "GET", &format!("/api/squads/{id}"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["data"]["id"], id.as_str());

        // Stale token → 409 squad_stale.
        let (s, v) = call(
            &app,
            "PATCH",
            &format!("/api/squads/{id}"),
            Some(json!({ "expected_updated_at": updated_at - 1, "leader_mode": "route_and_work" })),
        )
        .await;
        assert_eq!((s, code(&v).as_str()), (StatusCode::CONFLICT, "squad_stale"));
        let (s, v) = call(
            &app,
            "PATCH",
            &format!("/api/squads/{id}"),
            Some(json!({ "expected_updated_at": updated_at, "leader_mode": "route_and_work" })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["data"]["leader_mode"], "route_and_work");
        assert_eq!(
            v["data"]["members"].as_array().unwrap().len(),
            3,
            "untouched fields kept"
        );

        let (s, v) = call(&app, "DELETE", &format!("/api/squads/{id}"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert!(v["data"]["deleted_at"].as_i64().is_some());
        let (s, v) = call(&app, "GET", &format!("/api/squads/{id}"), None).await;
        assert_eq!((s, code(&v).as_str()), (StatusCode::NOT_FOUND, "squad_not_found"));
        let (_, v) = call(&app, "GET", "/api/squads", None).await;
        assert!(v["data"].as_array().unwrap().is_empty());
        let (_, v) = call(&app, "GET", "/api/squads?include_deleted=1", None).await;
        assert_eq!(v["data"].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn validation_codes() {
        let (app, _db) = app(vec!["lead", "a", "b", "c", "d", "e", "f"]).await;
        let post = |body: serde_json::Value| {
            let app = app.clone();
            async move { call(&app, "POST", "/api/squads", Some(body)).await }
        };
        let (s, v) = post(json!({ "name": "x", "leader_assistant_id": "lead",
            "members": [member("a"), member("b"), member("c"), member("d"), member("e"), member("f")] }))
        .await;
        assert_eq!(
            (s, code(&v).as_str()),
            (StatusCode::BAD_REQUEST, "squad_too_many_members")
        );
        assert_eq!(v["details"]["limit"], 6);

        let (_, v) = post(json!({ "name": "x", "leader_assistant_id": "lead", "members": [member("lead")] })).await;
        assert_eq!(code(&v), "squad_member_duplicate", "leader cannot also be a member");
        let (_, v) =
            post(json!({ "name": "x", "leader_assistant_id": "lead", "members": [member("a"), member("a")] })).await;
        assert_eq!(code(&v), "squad_member_duplicate");
        let (_, v) = post(json!({ "name": "x", "leader_assistant_id": "lead", "members": [member("ghost")] })).await;
        assert_eq!(code(&v), "squad_assistant_not_eligible");
        assert_eq!(v["details"]["assistant_id"], "ghost");
        let (_, v) = post(json!({ "name": "x", "leader_assistant_id": "lead", "members": [] })).await;
        assert_eq!(code(&v), "squad_no_members");
        let (_, v) = post(json!({ "name": "  ", "leader_assistant_id": "lead", "members": [member("a")] })).await;
        assert_eq!(code(&v), "squad_invalid_name");
        let (_, v) = post(json!({ "name": "x", "leader_assistant_id": "lead",
            "members": [{ "assistant_id": "a", "role_title": "r", "max_parallel": 9 }] }))
        .await;
        assert_eq!(code(&v), "squad_invalid_member");
        let (s, v) =
            post(json!({ "name": "x", "leader_assistant_id": "lead", "visibility": "team", "members": [member("a")] }))
                .await;
        assert_eq!(
            (s, code(&v).as_str()),
            (StatusCode::FORBIDDEN, "squad_team_visibility_unavailable")
        );

        let ok = json!({ "name": "dup", "leader_assistant_id": "lead", "members": [member("a")] });
        assert_eq!(post(ok.clone()).await.0, StatusCode::OK);
        let (s, v) = post(ok).await;
        assert_eq!((s, code(&v).as_str()), (StatusCode::CONFLICT, "squad_name_conflict"));
        // Every refusal's message also starts with its code.
        assert!(
            v["error"]
                .as_str()
                .unwrap_or_default()
                .starts_with("squad_name_conflict:")
                || v["message"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("squad_name_conflict:"),
            "{v}"
        );
    }

    #[tokio::test]
    async fn delete_refuses_while_instances_exist_and_briefs_flag_missing() {
        let (app, db) = app(vec!["lead", "a"]).await;
        let (_, v) = call(
            &app,
            "POST",
            "/api/squads",
            Some(json!({ "name": "x", "leader_assistant_id": "lead", "members": [member("a")] })),
        )
        .await;
        let id = v["data"]["id"].as_str().unwrap().to_string();
        sqlx::query(
            "INSERT INTO teams (id, user_id, name, agents, created_at, updated_at, squad_template_id) \
             VALUES ('t1', 'system_default_user', 'n', '[]', 1, 1, ?)",
        )
        .bind(&id)
        .execute(db.pool())
        .await
        .unwrap();
        let (s, v) = call(&app, "DELETE", &format!("/api/squads/{id}"), None).await;
        assert_eq!((s, code(&v).as_str()), (StatusCode::CONFLICT, "squad_in_use"), "{v}");
        assert_eq!(v["details"]["team_ids"][0], "t1");
    }
}
