//! E10-2: skill import from sources (url / git / local snapshot), refresh,
//! review (D10-3), visibility and the pending-review queue.
//! E10-3: version diff and restore (restore = a new version, history kept).
//!
//! Every import / refresh goes through one pipeline:
//! fetch → find skill dir → copy into the user skills dir → snapshot →
//! `append_version` (scripts ⇒ `review_status = 'pending'`, D10-7) →
//! store the version files under `.versions/` → record the source.
#![allow(clippy::disallowed_types)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path as AxumPath, State};
use axum::routing::{get, post, put};

use aionui_api_types::{
    ApiResponse, ImportFromSourceRequest, ImportFromSourceResponse, ImportSourceKind, ReviewDecision,
    ReviewSkillRequest, SetVisibilityRequest, SkillGovernanceInfo, SkillRestoreResponse, SkillSourceDetailResponse,
    SkillSourceInfo, SkillSourceSyncResult, SkillVersionDiffResponse, SkillVersionInfo,
};
use aionui_auth::CurrentUser;
use aionui_common::{ApiError, generate_prefixed_id, now_ms};
use aionui_db::{
    AppendSkillVersionParams, ISkillRepository, ISkillSourceRepository, SkillGovernanceRow, SkillSourceRow,
    SkillVersionRow, UpsertSkillSourceParams,
};

use crate::error::ExtensionError;
use crate::skill_remote_fetch::{self as fetch, FetchError};
use crate::skill_service::{self, SkillPaths};
use crate::skill_versions;

/// D10-3: who administers a team. E12 roles are not merged yet, so the
/// default answers "nobody" and every team action is refused (team entry hidden).
#[async_trait::async_trait]
pub trait TeamRoleChecker: Send + Sync {
    async fn is_team_admin(&self, user_id: &str, team_id: &str) -> bool;
}

/// Until E12-3 lands: no one is a team admin.
pub struct NoTeamRoles;

#[async_trait::async_trait]
impl TeamRoleChecker for NoTeamRoles {
    async fn is_team_admin(&self, _user_id: &str, _team_id: &str) -> bool {
        false
    }
}

#[derive(Clone)]
pub struct SkillGovernanceState {
    pub skill_paths: SkillPaths,
    pub skill_repo: Arc<dyn ISkillRepository>,
    pub source_repo: Arc<dyn ISkillSourceRepository>,
    pub team_roles: Arc<dyn TeamRoleChecker>,
}

// ---------------------------------------------------------------------------
// API types (snake_case, same as the other /api/skills endpoints)
// ---------------------------------------------------------------------------

/// Row -> wire type conversion (the wire types live in `aionui-api-types`).
pub(crate) trait ToApi {
    type Out;
    fn to_api(self) -> Self::Out;
}

impl ToApi for SkillSourceRow {
    type Out = SkillSourceInfo;
    fn to_api(self) -> SkillSourceInfo {
        let r = self;
        SkillSourceInfo {
            kind: r.kind,
            locator: r.locator,
            git_ref: r.git_ref,
            subpath: r.subpath,
            origin_app: r.origin_app,
            trust: r.trust,
            last_fetched_at: r.last_fetched_at,
            last_commit: r.last_commit,
        }
    }
}

impl ToApi for SkillVersionRow {
    type Out = SkillVersionInfo;
    fn to_api(self) -> SkillVersionInfo {
        let r = self;
        SkillVersionInfo {
            version_no: r.version_no,
            content_hash: r.content_hash,
            has_scripts: r.has_scripts,
            source_ref: r.source_ref,
            created_by: r.created_by,
            created_at: r.created_at,
            manifest: serde_json::from_str(&r.manifest).unwrap_or(serde_json::Value::Null),
        }
    }
}

impl ToApi for SkillGovernanceRow {
    type Out = SkillGovernanceInfo;
    fn to_api(self) -> SkillGovernanceInfo {
        let r = self;
        SkillGovernanceInfo {
            skill_id: r.id,
            name: r.name,
            visibility: r.visibility,
            team_id: r.team_id,
            review_status: r.review_status,
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

fn fetch_err(e: FetchError) -> ApiError {
    match e {
        FetchError::InvalidUrl(_) | FetchError::BlockedAddress(_) => ApiError::BadRequest(e.to_string()),
        FetchError::TooLarge { .. } => ApiError::PayloadTooLarge(e.to_string()),
        FetchError::Timeout => ApiError::Timeout(e.to_string()),
        FetchError::Failed(_) => ApiError::BadGateway(e.to_string()),
        FetchError::GitUnsupported(_) => ApiError::BadRequest(e.to_string()),
    }
}

fn db_err(e: aionui_db::DbError) -> ApiError {
    ExtensionError::Db(e).into()
}

fn io_err(e: std::io::Error) -> ApiError {
    ApiError::Internal(format!("skill_version_store_failed: {e}"))
}

fn ext_err(e: ExtensionError) -> ApiError {
    e.into()
}

// ---------------------------------------------------------------------------
// Fetch → candidate skill dirs
// ---------------------------------------------------------------------------

struct Staged {
    /// Keeps the temp dir alive until the import is done.
    _tmp: Option<tempfile::TempDir>,
    root: PathBuf,
    source_ref: Option<String>,
}

async fn stage(paths: &SkillPaths, req: &ImportFromSourceRequest) -> Result<Staged, ApiError> {
    let tmp_parent = paths.user_skills_dir.join(".import-tmp");
    tokio::fs::create_dir_all(&tmp_parent).await.map_err(io_err)?;
    let new_tmp = || {
        tempfile::Builder::new()
            .prefix("src-")
            .tempdir_in(&tmp_parent)
            .map_err(io_err)
    };
    let subpath = req.subpath.as_deref().filter(|s| !s.trim().is_empty());
    let sub = subpath.map(fetch::validate_subpath).transpose().map_err(fetch_err)?;
    match req.kind {
        ImportSourceKind::Local => {
            let p = PathBuf::from(req.locator.trim());
            if req.locator.contains("://") || !p.is_absolute() {
                return Err(ApiError::BadRequest(
                    "source_url_invalid: local source must be an absolute directory path".into(),
                ));
            }
            let root = sub.map(|s| p.join(s)).unwrap_or(p);
            Ok(Staged {
                _tmp: None,
                root,
                source_ref: None,
            })
        }
        ImportSourceKind::Git => {
            let url = fetch::parse_remote_url(&req.locator).map_err(fetch_err)?;
            let tmp = new_tmp()?;
            let dest = tmp.path().join("repo");
            let cloned = fetch::git_clone(&url, req.git_ref.as_deref(), &dest)
                .await
                .map_err(fetch_err)?;
            let root = sub.map(|s| dest.join(s)).unwrap_or(dest);
            Ok(Staged {
                _tmp: Some(tmp),
                root,
                source_ref: cloned.commit,
            })
        }
        ImportSourceKind::Url => {
            let url = fetch::parse_remote_url(&req.locator).map_err(fetch_err)?;
            let body = fetch::download_https(&url).await.map_err(fetch_err)?;
            let tmp = new_tmp()?;
            let out = tmp.path().join("pkg");
            tokio::fs::create_dir_all(&out).await.map_err(io_err)?;
            if body.starts_with(b"PK\x03\x04") {
                let archive = tmp.path().join("package.zip");
                tokio::fs::write(&archive, &body).await.map_err(io_err)?;
                let (a, o) = (archive.clone(), out.clone());
                tokio::task::spawn_blocking(move || skill_service::extract_zip_archive(&a, &o))
                    .await
                    .map_err(|e| ApiError::Internal(e.to_string()))?
                    .map_err(ext_err)?;
            } else {
                let text = String::from_utf8(body)
                    .map_err(|_| ApiError::BadRequest("source_url_invalid: not a zip or SKILL.md".into()))?;
                skill_service::read_skill_info_from_content(&text).map_err(ext_err)?;
                tokio::fs::write(out.join("SKILL.md"), text).await.map_err(io_err)?;
            }
            let root = sub.map(|s| out.join(s)).unwrap_or(out);
            Ok(Staged {
                _tmp: Some(tmp),
                root,
                source_ref: None,
            })
        }
    }
}

/// The staged root itself if it holds SKILL.md, else its direct children that do
/// (an archive with a top folder, or a repo of several skills). Max 50.
fn candidate_dirs(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    if root.join("SKILL.md").is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        if !entry.file_type()?.is_dir() || name.to_string_lossy().starts_with('.') {
            continue;
        }
        if entry.path().join("SKILL.md").is_file() {
            out.push(entry.path());
        }
    }
    out.sort();
    out.truncate(50);
    Ok(out)
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

fn db_kind(kind: ImportSourceKind) -> &'static str {
    match kind {
        ImportSourceKind::Url => "url",
        ImportSourceKind::Git => "git",
        ImportSourceKind::Local => "local_snapshot",
    }
}

/// Record a new version of an already-copied skill directory (import, refresh
/// or evolution apply). Returns the version and whether it is new.
pub async fn record_skill_version(
    paths: &SkillPaths,
    source_repo: &dyn ISkillSourceRepository,
    skill_id: &str,
    skill_dir: &Path,
    source_ref: Option<&str>,
    created_by: &str,
) -> Result<(SkillVersionRow, bool), ApiError> {
    let dir = skill_dir.to_path_buf();
    let snapshot = tokio::task::spawn_blocking(move || skill_versions::snapshot_skill_dir(&dir))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::BadRequest(format!("skill_snapshot_failed: {e}")))?;
    let manifest = snapshot.manifest_json();
    let version_id = generate_prefixed_id("skver");
    let has_scripts = snapshot.has_scripts;

    // 1) Files first: stage a full copy before any DB row exists.
    let root = paths.user_skills_dir.clone();
    let staging = {
        let (root, sid, tag, src) = (
            root.clone(),
            skill_id.to_string(),
            version_id.clone(),
            skill_dir.to_path_buf(),
        );
        tokio::task::spawn_blocking(move || skill_versions::stage_version_files(&root, &sid, &tag, &src))
            .await
            .map_err(|e| ApiError::Internal(e.to_string()))
            .and_then(|r| r.map_err(io_err))
    };
    let staging = match staging {
        Ok(p) => p,
        Err(e) => {
            hold_for_review_on_failure(source_repo, skill_id, has_scripts).await;
            return Err(e);
        }
    };
    let discard_staging = |p: PathBuf| async move {
        let _ = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(p)).await;
    };

    // 2) Insert the row; inside the same transaction (version_no now known) the
    //    staged copy is renamed into place. A rename failure aborts the insert,
    //    so a version row never exists without its files.
    let placed: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let hook = |version_no: i64| -> Result<(), String> {
        let target =
            skill_versions::commit_staged_version(&root, skill_id, version_no, &staging).map_err(|e| e.to_string())?;
        *placed.lock().unwrap_or_else(|p| p.into_inner()) = Some(target);
        Ok(())
    };
    let result = source_repo
        .append_version(AppendSkillVersionParams {
            id: &version_id,
            skill_id,
            content_hash: &snapshot.content_hash,
            manifest: &manifest,
            has_scripts,
            source_ref,
            created_by,
            now: now_ms(),
            before_commit: Some(&hook),
        })
        .await;
    let placed = placed.into_inner().unwrap_or_else(|p| p.into_inner());
    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            // Files moved in but the commit failed: move them back out so the
            // number stays free, then drop the staging copy.
            if let Some(target) = placed {
                let st = staging.clone();
                let _ = tokio::task::spawn_blocking(move || std::fs::rename(&target, &st)).await;
            }
            discard_staging(staging).await;
            hold_for_review_on_failure(source_repo, skill_id, has_scripts).await;
            return Err(match e {
                aionui_db::DbError::Init(msg) if msg.starts_with("skill version files:") => {
                    ApiError::Internal(format!("skill_version_store_failed: {msg}"))
                }
                other => db_err(other),
            });
        }
    };
    if !outcome.created {
        discard_staging(staging).await;
    }
    Ok((outcome.version, outcome.created))
}

/// Record a version for an already-imported skill identified by name (used by
/// the legacy `/api/skills/import`). Missing rows are ignored.
pub async fn record_version_for_named_skill(
    state: &SkillGovernanceState,
    user_id: &str,
    name: &str,
    created_by: &str,
) -> Result<(), ApiError> {
    let Some(row) = state
        .skill_repo
        .find_by_name_any_for_user(user_id, name)
        .await
        .map_err(db_err)?
    else {
        return Ok(());
    };
    record_skill_version(
        &state.skill_paths,
        state.source_repo.as_ref(),
        &row.id,
        Path::new(&row.path),
        None,
        created_by,
    )
    .await
    .map(|_| ())
}

/// When recording a version fails, the live skill dir may already hold the new
/// content. If that content ships scripts, force `pending` so it can never be
/// used unreviewed (D10-7), even though no version row was kept.
async fn hold_for_review_on_failure(source_repo: &dyn ISkillSourceRepository, skill_id: &str, has_scripts: bool) {
    if has_scripts && let Err(e) = source_repo.set_review_status(skill_id, "pending", now_ms()).await {
        tracing::error!(skill_id, error = %e, "forcing pending after version failure failed");
    }
}

async fn import_one(
    state: &SkillGovernanceState,
    user_id: &str,
    req: &ImportFromSourceRequest,
    staged: &Staged,
    dir: &Path,
    expect_name: Option<&str>,
) -> Result<SkillSourceSyncResult, ApiError> {
    let (name, _) = skill_service::read_skill_info(dir).await.map_err(ext_err)?;
    if let Some(expected) = expect_name
        && expected != name
    {
        // D10-6: keep the local name, surface the conflict, never rename.
        return Err(ApiError::Conflict(format!(
            "skill_source_renamed: source now names the skill '{name}', local name is '{expected}'"
        )));
    }
    let imported =
        skill_service::import_skill_with_repo_for_user(&state.skill_paths, state.skill_repo.as_ref(), user_id, dir)
            .await
            .map_err(ext_err)?;
    let row = state
        .skill_repo
        .find_by_name_any_for_user(user_id, &imported.name)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::Internal("imported skill row missing".into()))?;
    let (version, changed) = record_skill_version(
        &state.skill_paths,
        state.source_repo.as_ref(),
        &row.id,
        Path::new(&row.path),
        staged.source_ref.as_deref(),
        "import",
    )
    .await?;
    let now = now_ms();
    let source_id = generate_prefixed_id("sksrc");
    let git_ref = req.git_ref.as_deref().filter(|s| !s.is_empty());
    let subpath = req.subpath.as_deref().filter(|s| !s.is_empty());
    state
        .source_repo
        .upsert_source(UpsertSkillSourceParams {
            id: &source_id,
            user_id: Some(user_id),
            skill_id: &row.id,
            kind: db_kind(req.kind),
            locator: req.locator.trim(),
            git_ref,
            subpath,
            origin_app: req.origin_app.as_deref(),
            trust: "untrusted",
            now,
        })
        .await
        .map_err(db_err)?;
    let source = state
        .source_repo
        .record_fetch(&row.id, now, staged.source_ref.as_deref())
        .await
        .map_err(db_err)?;
    let gov = state
        .source_repo
        .governance(&row.id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::NotFound("skill_not_found".into()))?;
    Ok(SkillSourceSyncResult {
        skill: gov.to_api(),
        source: source.to_api(),
        version: version.to_api(),
        changed,
    })
}

/// Owner-or-NotFound: never reveal another user's skill.
async fn owned(state: &SkillGovernanceState, user_id: &str, skill_id: &str) -> Result<SkillGovernanceRow, ApiError> {
    match state.source_repo.governance(skill_id).await.map_err(db_err)? {
        Some(g) if g.user_id.as_deref() == Some(user_id) => Ok(g),
        _ => Err(ApiError::NotFound("skill_not_found".into())),
    }
}

/// D10-3: private skill → its owner approves; team skill → a team admin.
async fn may_review(state: &SkillGovernanceState, user_id: &str, gov: &SkillGovernanceRow) -> bool {
    match (gov.visibility.as_str(), gov.team_id.as_deref()) {
        ("team", Some(team)) => state.team_roles.is_team_admin(user_id, team).await,
        ("team", None) => false,
        _ => gov.user_id.as_deref() == Some(user_id),
    }
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

pub fn skill_governance_routes(state: SkillGovernanceState) -> Router {
    Router::new()
        .route("/api/skills/sources/import", post(import_from_source))
        .route("/api/skills/reviews/pending", get(list_pending_reviews))
        .route("/api/skills/id/{skill_id}/source", get(get_source_detail))
        .route("/api/skills/id/{skill_id}/refresh", post(refresh_from_source))
        .route("/api/skills/id/{skill_id}/review", post(review_skill))
        .route("/api/skills/id/{skill_id}/visibility", put(set_visibility))
        .route(
            "/api/skills/id/{skill_id}/versions/{from}/diff/{to}",
            get(diff_versions),
        )
        .route(
            "/api/skills/id/{skill_id}/versions/{version_no}/restore",
            post(restore_version),
        )
        .with_state(state)
}

/// `POST /api/skills/sources/import`
async fn import_from_source(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    body: Result<Json<ImportFromSourceRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<ImportFromSourceResponse>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    if let Some(app) = req.origin_app.as_deref()
        && !aionui_db::models::SKILL_ORIGIN_APPS.contains(&app)
    {
        return Err(ApiError::BadRequest(format!("invalid origin_app '{app}'")));
    }
    let staged = stage(&state.skill_paths, &req).await?;
    let root = staged.root.clone();
    let dirs = tokio::task::spawn_blocking(move || candidate_dirs(&root))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::BadRequest(format!("skill_import_no_skill_found: {e}")))?;
    if dirs.is_empty() {
        return Err(ApiError::BadRequest("skill_import_no_skill_found".into()));
    }
    let mut imported = Vec::new();
    for dir in &dirs {
        imported.push(import_one(&state, &user.id, &req, &staged, dir, None).await?);
    }
    Ok(Json(ApiResponse::ok(ImportFromSourceResponse { imported })))
}

/// `POST /api/skills/id/{skill_id}/refresh` — re-fetch, no-op on same hash.
async fn refresh_from_source(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath(skill_id): AxumPath<String>,
) -> Result<Json<ApiResponse<SkillSourceSyncResult>>, ApiError> {
    let gov = owned(&state, &user.id, &skill_id).await?;
    let src = state
        .source_repo
        .find_source(&skill_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::NotFound("skill_source_not_found".into()))?;
    let kind = match src.kind.as_str() {
        "url" => ImportSourceKind::Url,
        "git" => ImportSourceKind::Git,
        "local_snapshot" => ImportSourceKind::Local,
        other => return Err(ApiError::BadRequest(format!("skill_source_not_refreshable: {other}"))),
    };
    let req = ImportFromSourceRequest {
        kind,
        locator: src.locator,
        git_ref: src.git_ref,
        subpath: src.subpath,
        origin_app: src.origin_app,
    };
    let staged = stage(&state.skill_paths, &req).await?;
    let root = staged.root.clone();
    let dirs = tokio::task::spawn_blocking(move || candidate_dirs(&root))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::BadRequest(format!("skill_import_no_skill_found: {e}")))?;
    let mut chosen = None;
    for d in &dirs {
        if let Ok((n, _)) = skill_service::read_skill_info(d).await
            && n == gov.name
        {
            chosen = Some(d.clone());
            break;
        }
    }
    let dir = match (chosen, dirs.len()) {
        (Some(d), _) => d,
        (None, 1) => dirs[0].clone(), // single skill renamed upstream → import_one reports the conflict
        _ => {
            return Err(ApiError::Conflict(
                "skill_source_renamed: local skill not found in source".into(),
            ));
        }
    };
    let result = import_one(&state, &user.id, &req, &staged, &dir, Some(&gov.name)).await?;
    Ok(Json(ApiResponse::ok(result)))
}

/// `GET /api/skills/id/{skill_id}/source`
async fn get_source_detail(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath(skill_id): AxumPath<String>,
) -> Result<Json<ApiResponse<SkillSourceDetailResponse>>, ApiError> {
    let gov = owned(&state, &user.id, &skill_id).await?;
    let source = state
        .source_repo
        .find_source(&skill_id)
        .await
        .map_err(db_err)?
        .map(ToApi::to_api);
    let versions = state
        .source_repo
        .list_versions(&skill_id)
        .await
        .map_err(db_err)?
        .into_iter()
        .map(ToApi::to_api)
        .collect();
    Ok(Json(ApiResponse::ok(SkillSourceDetailResponse {
        skill: gov.to_api(),
        source,
        versions,
    })))
}

/// `GET /api/skills/reviews/pending`
async fn list_pending_reviews(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<SkillGovernanceInfo>>>, ApiError> {
    let rows = state
        .source_repo
        .list_pending_reviews_for_user(&user.id)
        .await
        .map_err(db_err)?;
    Ok(Json(ApiResponse::ok(rows.into_iter().map(ToApi::to_api).collect())))
}

/// `POST /api/skills/id/{skill_id}/review` — only a pending skill can be decided.
async fn review_skill(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath(skill_id): AxumPath<String>,
    body: Result<Json<ReviewSkillRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SkillGovernanceInfo>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    let gov = state
        .source_repo
        .governance(&skill_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::NotFound("skill_not_found".into()))?;
    if !may_review(&state, &user.id, &gov).await {
        if gov.user_id.as_deref() == Some(user.id.as_str()) {
            return Err(ApiError::Forbidden(
                "skill_review_forbidden: team skills need a team admin".into(),
            ));
        }
        return Err(ApiError::NotFound("skill_not_found".into()));
    }
    if gov.review_status != "pending" {
        return Err(ApiError::Conflict(format!(
            "skill_review_not_pending: {}",
            gov.review_status
        )));
    }
    let status = match req.decision {
        ReviewDecision::Approve => "approved",
        ReviewDecision::Reject => "rejected",
    };
    state
        .source_repo
        .set_review_status(&skill_id, status, now_ms())
        .await
        .map_err(db_err)?;
    let gov = state
        .source_repo
        .governance(&skill_id)
        .await
        .map_err(db_err)?
        .ok_or_else(|| ApiError::NotFound("skill_not_found".into()))?;
    Ok(Json(ApiResponse::ok(gov.to_api())))
}

/// `PUT /api/skills/id/{skill_id}/visibility` — owner only; publishing to a
/// team additionally needs team admin (D10-3 / §2.5, refused until E12).
async fn set_visibility(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath(skill_id): AxumPath<String>,
    body: Result<Json<SetVisibilityRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<SkillGovernanceInfo>>, ApiError> {
    let Json(req) = body.map_err(ApiError::from)?;
    owned(&state, &user.id, &skill_id).await?;
    let team_id = req.team_id.as_deref().filter(|s| !s.trim().is_empty());
    match req.visibility.as_str() {
        "private" => {}
        "team" => {
            let team = team_id.ok_or_else(|| ApiError::BadRequest("team_id is required for team visibility".into()))?;
            if !state.team_roles.is_team_admin(&user.id, team).await {
                return Err(ApiError::Forbidden(
                    "skill_publish_forbidden: team admin required".into(),
                ));
            }
        }
        other => return Err(ApiError::BadRequest(format!("invalid visibility '{other}'"))),
    }
    state
        .source_repo
        .set_visibility(&skill_id, &req.visibility, team_id, now_ms())
        .await
        .map_err(db_err)?;
    let gov = owned(&state, &user.id, &skill_id).await?;
    Ok(Json(ApiResponse::ok(gov.to_api())))
}

/// The stored files of one recorded version, or 404.
async fn stored_version_dir(
    state: &SkillGovernanceState,
    skill_id: &str,
    version_no: i64,
) -> Result<PathBuf, ApiError> {
    let known = state
        .source_repo
        .list_versions(skill_id)
        .await
        .map_err(db_err)?
        .into_iter()
        .any(|v| v.version_no == version_no);
    let dir = skill_versions::version_dir(&state.skill_paths.user_skills_dir, skill_id, version_no)
        .map_err(|_| ApiError::NotFound("skill_version_not_found".into()))?;
    if !known || !dir.is_dir() {
        return Err(ApiError::NotFound(format!("skill_version_not_found: {version_no}")));
    }
    Ok(dir)
}

/// `GET /api/skills/id/{skill_id}/versions/{from}/diff/{to}` — owner only.
async fn diff_versions(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath((skill_id, from, to)): AxumPath<(String, i64, i64)>,
) -> Result<Json<ApiResponse<SkillVersionDiffResponse>>, ApiError> {
    owned(&state, &user.id, &skill_id).await?;
    let a = stored_version_dir(&state, &skill_id, from).await?;
    let b = stored_version_dir(&state, &skill_id, to).await?;
    let files = tokio::task::spawn_blocking(move || skill_versions::diff_version_dirs(&a, &b))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(format!("skill_version_diff_failed: {e}")))?;
    Ok(Json(ApiResponse::ok(SkillVersionDiffResponse {
        skill_id,
        from_version: from,
        to_version: to,
        files,
    })))
}

/// `POST /api/skills/id/{skill_id}/versions/{version_no}/restore` — owner only.
///
/// The current live content is recorded first (so unrecorded edits are kept as
/// a version), then the old version's files replace the live dir and are
/// recorded as a NEW version (`created_by = user`, `source_ref = restore:v{n}`).
/// History is never rewritten; restoring script-bearing content re-pends the
/// skill (D10-7). The restored SKILL.md must keep the local name (D10-6).
async fn restore_version(
    State(state): State<SkillGovernanceState>,
    Extension(user): Extension<CurrentUser>,
    AxumPath((skill_id, version_no)): AxumPath<(String, i64)>,
) -> Result<Json<ApiResponse<SkillRestoreResponse>>, ApiError> {
    let gov = owned(&state, &user.id, &skill_id).await?;
    let src = stored_version_dir(&state, &skill_id, version_no).await?;
    let (restored_name, _) = skill_service::read_skill_info(&src).await.map_err(ext_err)?;
    if restored_name != gov.name {
        return Err(ApiError::Conflict(format!(
            "skill_restore_renamed: version {version_no} names the skill '{restored_name}', local name is '{}'",
            gov.name
        )));
    }
    let row = state
        .skill_repo
        .find_by_name_any_for_user(&user.id, &gov.name)
        .await
        .map_err(db_err)?
        .filter(|r| r.id == skill_id && r.deleted_at.is_none())
        .ok_or_else(|| ApiError::NotFound("skill_not_found".into()))?;
    let live = PathBuf::from(&row.path);

    // 1) Keep whatever is live now (no-op when it equals the latest version).
    record_skill_version(
        &state.skill_paths,
        state.source_repo.as_ref(),
        &skill_id,
        &live,
        None,
        "user",
    )
    .await?;

    // 2) Swap the old version's files in.
    let (root, sid, dst) = (
        state.skill_paths.user_skills_dir.clone(),
        skill_id.clone(),
        live.clone(),
    );
    tokio::task::spawn_blocking(move || skill_versions::restore_version_into(&root, &sid, version_no, &dst))
        .await
        .map_err(|e| ApiError::Internal(e.to_string()))?
        .map_err(|e| ApiError::Internal(format!("skill_restore_failed: {e}")))?;

    // 3) Record it as a new version; scripts ⇒ pending.
    let source_ref = format!("restore:v{version_no}");
    let (version, changed) = record_skill_version(
        &state.skill_paths,
        state.source_repo.as_ref(),
        &skill_id,
        &live,
        Some(&source_ref),
        "user",
    )
    .await?;

    // Keep the skills row (description) in step with the restored SKILL.md.
    if let Ok((_, description)) = skill_service::read_skill_info(&live).await
        && row.description.as_deref() != Some(description.as_str())
        && let Err(e) = state
            .skill_repo
            .upsert_for_user(
                &user.id,
                aionui_db::UpsertSkillParams {
                    name: &row.name,
                    description: Some(&description),
                    path: &row.path,
                    source: &row.source,
                    enabled: row.enabled,
                },
            )
            .await
    {
        tracing::warn!(skill_id, error = %e, "restore: description refresh failed");
    }

    let gov = owned(&state, &user.id, &skill_id).await?;
    Ok(Json(ApiResponse::ok(SkillRestoreResponse {
        skill: gov.to_api(),
        version: version.to_api(),
        restored_from: version_no,
        changed,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    struct Fixture {
        _tmp: tempfile::TempDir,
        state: SkillGovernanceState,
        src_root: PathBuf,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = SkillPaths {
            data_dir: tmp.path().to_path_buf(),
            user_skills_dir: tmp.path().join("skills"),
            cron_skills_dir: tmp.path().join("cron").join("skills"),
            builtin_skills_dir: tmp.path().join("builtin-skills"),
            builtin_rules_dir: tmp.path().join("builtin-rules"),
            assistant_rules_dir: tmp.path().join("assistant-rules"),
            assistant_skills_dir: tmp.path().join("assistant-skills"),
        };
        let db = aionui_db::init_database_memory().await.unwrap();
        let state = SkillGovernanceState {
            skill_paths: paths,
            skill_repo: Arc::new(aionui_db::SqliteSkillRepository::new(db.pool().clone())),
            source_repo: Arc::new(aionui_db::SqliteSkillSourceRepository::new(db.pool().clone())),
            team_roles: Arc::new(NoTeamRoles),
        };
        std::mem::forget(db);
        let src_root = tmp.path().join("external");
        Fixture {
            _tmp: tmp,
            state,
            src_root,
        }
    }

    fn write_skill(dir: &Path, name: &str, with_script: bool) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test skill\n---\nbody\n"),
        )
        .unwrap();
        if with_script {
            std::fs::create_dir_all(dir.join("scripts")).unwrap();
            std::fs::write(dir.join("scripts").join("run.sh"), "echo hi\n").unwrap();
        }
    }

    async fn call(
        state: &SkillGovernanceState,
        user: CurrentUser,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = skill_governance_routes(state.clone())
            .layer(Extension(user))
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = http_body_util::BodyExt::collect(resp.into_body())
            .await
            .unwrap()
            .to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
        )
    }

    fn other_user() -> CurrentUser {
        let mut u = CurrentUser::local_default();
        u.id = "someone_else".into();
        u
    }

    #[tokio::test]
    async fn local_import_with_scripts_goes_pending_and_owner_approves() {
        let f = fixture().await;
        let dir = f.src_root.join("scripted");
        write_skill(&dir, "scripted", true);
        let me = CurrentUser::local_default();
        let (st, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy(), "origin_app": "claude"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let item = &body["data"]["imported"][0];
        assert_eq!(item["skill"]["review_status"], "pending");
        assert_eq!(item["source"]["kind"], "local_snapshot");
        assert_eq!(item["source"]["trust"], "untrusted");
        assert_eq!(item["version"]["version_no"], 1);
        assert_eq!(item["version"]["has_scripts"], true);
        let skill_id = item["skill"]["skill_id"].as_str().unwrap().to_string();
        assert!(
            skill_versions::version_dir(&f.state.skill_paths.user_skills_dir, &skill_id, 1)
                .unwrap()
                .join("scripts/run.sh")
                .is_file()
        );

        // Pending skills are hidden from the resolver-facing list.
        assert!(
            f.state
                .skill_repo
                .find_by_name_for_user(&me.id, "scripted")
                .await
                .unwrap()
                .is_none()
        );

        let (st, q) = call(
            &f.state,
            me.clone(),
            "GET",
            "/api/skills/reviews/pending",
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(q["data"].as_array().unwrap().len(), 1);

        // Another user can neither see nor approve it.
        let (st, _) = call(
            &f.state,
            other_user(),
            "POST",
            &format!("/api/skills/id/{skill_id}/review"),
            serde_json::json!({"decision": "approve"}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        let (st, b) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/review"),
            serde_json::json!({"decision": "approve"}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{b}");
        assert_eq!(b["data"]["review_status"], "approved");

        // Already decided → 409.
        let (st, _) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/review"),
            serde_json::json!({"decision": "reject"}),
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);

        // Refresh with identical content is a no-op; detail lists one version.
        let (st, r) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["data"]["changed"], false);
        assert_eq!(r["data"]["skill"]["review_status"], "approved");

        // Changed script → new version, back to pending.
        std::fs::write(dir.join("scripts").join("run.sh"), "echo changed\n").unwrap();
        let (_, r) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(r["data"]["changed"], true);
        assert_eq!(r["data"]["version"]["version_no"], 2);
        assert_eq!(r["data"]["skill"]["review_status"], "pending");

        let (_, d) = call(
            &f.state,
            me,
            "GET",
            &format!("/api/skills/id/{skill_id}/source"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(d["data"]["versions"].as_array().unwrap().len(), 2);
        assert_eq!(d["data"]["versions"][0]["version_no"], 2);
    }

    #[tokio::test]
    async fn refresh_refuses_upstream_rename() {
        let f = fixture().await;
        let dir = f.src_root.join("plain");
        write_skill(&dir, "plain", false);
        let me = CurrentUser::local_default();
        let (_, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
        assert_eq!(body["data"]["imported"][0]["skill"]["review_status"], "none");
        let skill_id = body["data"]["imported"][0]["skill"]["skill_id"]
            .as_str()
            .unwrap()
            .to_string();
        write_skill(&dir, "renamed", false);
        let (st, _) = call(
            &f.state,
            me,
            "POST",
            &format!("/api/skills/id/{skill_id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn team_visibility_needs_team_admin_and_private_is_owner_only() {
        let f = fixture().await;
        let dir = f.src_root.join("vis");
        write_skill(&dir, "vis", false);
        let me = CurrentUser::local_default();
        let (_, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
        let skill_id = body["data"]["imported"][0]["skill"]["skill_id"]
            .as_str()
            .unwrap()
            .to_string();
        let uri = format!("/api/skills/id/{skill_id}/visibility");
        let (st, _) = call(
            &f.state,
            me.clone(),
            "PUT",
            &uri,
            serde_json::json!({"visibility": "team", "team_id": "t1"}),
        )
        .await;
        assert_eq!(st, StatusCode::FORBIDDEN);
        let (st, _) = call(
            &f.state,
            me.clone(),
            "PUT",
            &uri,
            serde_json::json!({"visibility": "team"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
        let (st, _) = call(
            &f.state,
            other_user(),
            "PUT",
            &uri,
            serde_json::json!({"visibility": "private"}),
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);
        let (st, b) = call(&f.state, me, "PUT", &uri, serde_json::json!({"visibility": "private"})).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(b["data"]["visibility"], "private");
    }

    #[tokio::test]
    async fn import_refuses_file_scheme_and_internal_hosts() {
        let f = fixture().await;
        let me = CurrentUser::local_default();
        for (kind, loc) in [
            ("url", "file:///etc/passwd"),
            ("git", "file:///tmp/repo"),
            ("git", "https://127.0.0.1/repo.git"),
            ("url", "https://169.254.169.254/latest/meta-data"),
            ("url", "http://example.com/SKILL.md"),
            ("local", "file:///tmp/x"),
            ("local", "relative/dir"),
        ] {
            let (st, b) = call(
                &f.state,
                me.clone(),
                "POST",
                "/api/skills/sources/import",
                serde_json::json!({"kind": kind, "locator": loc}),
            )
            .await;
            assert_eq!(st, StatusCode::BAD_REQUEST, "{kind} {loc} {b}");
        }
        let (st, _) = call(
            &f.state,
            me,
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "git", "locator": "https://github.com/o/r.git", "ref": "--upload-pack=x"}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn version_store_failure_leaves_no_row_and_keeps_scripts_pending() {
        let f = fixture().await;
        let dir = f.src_root.join("rollback");
        write_skill(&dir, "rollback", false);
        let me = CurrentUser::local_default();
        let (st, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let skill_id = body["data"]["imported"][0]["skill"]["skill_id"]
            .as_str()
            .unwrap()
            .to_string();

        // Make v2's target unusable: an existing non-empty dir blocks the rename.
        let v2 = skill_versions::version_dir(&f.state.skill_paths.user_skills_dir, &skill_id, 2).unwrap();
        std::fs::create_dir_all(&v2).unwrap();
        std::fs::write(v2.join("squatter"), "x").unwrap();

        // Upstream adds a script, refresh must fail without leaving a v2 row.
        write_skill(&dir, "rollback", true);
        let (st, body) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        let versions = f.state.source_repo.list_versions(&skill_id).await.unwrap();
        assert_eq!(versions.len(), 1, "failed version row must be rolled back");
        assert_eq!(versions[0].version_no, 1);
        // The live dir now holds scripts, so it must not be usable unreviewed.
        let gov = f.state.source_repo.governance(&skill_id).await.unwrap().unwrap();
        assert_eq!(gov.review_status, "pending");
        // No staging leftovers.
        let parent = v2.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(".staging-"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[tokio::test]
    async fn diff_and_restore_append_a_version_and_gate_scripts() {
        let f = fixture().await;
        let dir = f.src_root.join("hist");
        write_skill(&dir, "hist", false);
        let me = CurrentUser::local_default();
        let (st, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{body}");
        let skill_id = body["data"]["imported"][0]["skill"]["skill_id"]
            .as_str()
            .unwrap()
            .to_string();
        let resolve = |state: SkillGovernanceState, user: String| async move {
            skill_service::materialize_skills_for_agent_with_repo_for_user(
                &state.skill_paths,
                state.skill_repo.as_ref(),
                &user,
                "e10-3-test",
                &["hist".to_string()],
            )
            .await
            .unwrap()
        };
        assert_eq!(resolve(f.state.clone(), me.id.clone()).await.len(), 1);

        // v2 adds a script → pending → the resolver must not deliver it.
        write_skill(&dir, "hist", true);
        let (_, r) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/refresh"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(r["data"]["version"]["version_no"], 2);
        assert_eq!(r["data"]["skill"]["review_status"], "pending");
        assert!(
            resolve(f.state.clone(), me.id.clone()).await.is_empty(),
            "pending skill must not reach an agent"
        );

        // Diff v1 → v2 lists the added script with a text body.
        let (st, d) = call(
            &f.state,
            me.clone(),
            "GET",
            &format!("/api/skills/id/{skill_id}/versions/1/diff/2"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{d}");
        let files = d["data"]["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{d}");
        assert_eq!(files[0]["path"], "scripts/run.sh");
        assert_eq!(files[0]["status"], "added");
        assert_eq!(files[0]["script"], true);
        assert!(files[0]["diff"].as_str().unwrap().contains("+echo hi"));

        // Unknown version / other user → 404.
        for (user, uri) in [
            (me.clone(), format!("/api/skills/id/{skill_id}/versions/1/diff/9")),
            (other_user(), format!("/api/skills/id/{skill_id}/versions/1/diff/2")),
        ] {
            let (st, _) = call(&f.state, user, "GET", &uri, serde_json::Value::Null).await;
            assert_eq!(st, StatusCode::NOT_FOUND, "{uri}");
        }
        let (st, _) = call(
            &f.state,
            other_user(),
            "POST",
            &format!("/api/skills/id/{skill_id}/versions/1/restore"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::NOT_FOUND);

        // Restore v1 → new v3 (history kept), script gone from the live dir.
        let (st, r) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/versions/1/restore"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::OK, "{r}");
        assert_eq!(r["data"]["changed"], true);
        assert_eq!(r["data"]["restored_from"], 1);
        assert_eq!(r["data"]["version"]["version_no"], 3);
        assert_eq!(r["data"]["version"]["source_ref"], "restore:v1");
        assert_eq!(r["data"]["version"]["created_by"], "user");
        assert_eq!(r["data"]["version"]["has_scripts"], false);
        let versions = f.state.source_repo.list_versions(&skill_id).await.unwrap();
        assert_eq!(versions.len(), 3);
        let v1 = versions.iter().find(|v| v.version_no == 1).unwrap();
        assert_eq!(versions[0].content_hash, v1.content_hash);
        let live = f
            .state
            .skill_repo
            .find_by_name_any_for_user(&me.id, "hist")
            .await
            .unwrap()
            .unwrap();
        assert!(!Path::new(&live.path).join("scripts/run.sh").exists());
        assert!(
            skill_versions::version_dir(&f.state.skill_paths.user_skills_dir, &skill_id, 2)
                .unwrap()
                .join("scripts/run.sh")
                .is_file(),
            "old versions are never touched"
        );
        // Review status is not cleared by a restore (the owner still decides).
        assert_eq!(r["data"]["skill"]["review_status"], "pending");

        // Restoring the latest content is a no-op.
        let (_, r) = call(
            &f.state,
            me.clone(),
            "POST",
            &format!("/api/skills/id/{skill_id}/versions/3/restore"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(r["data"]["changed"], false, "{r}");
        assert_eq!(r["data"]["version"]["version_no"], 3);
    }

    #[tokio::test]
    async fn restore_refuses_a_version_with_another_name() {
        let f = fixture().await;
        let dir = f.src_root.join("named");
        write_skill(&dir, "named", false);
        let me = CurrentUser::local_default();
        let (_, body) = call(
            &f.state,
            me.clone(),
            "POST",
            "/api/skills/sources/import",
            serde_json::json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
        let skill_id = body["data"]["imported"][0]["skill"]["skill_id"]
            .as_str()
            .unwrap()
            .to_string();
        // Tamper with the stored v1 name (simulates an upstream rename kept in history).
        let v1 = skill_versions::version_dir(&f.state.skill_paths.user_skills_dir, &skill_id, 1).unwrap();
        std::fs::write(v1.join("SKILL.md"), "---\nname: other\ndescription: d\n---\n").unwrap();
        let (st, _) = call(
            &f.state,
            me,
            "POST",
            &format!("/api/skills/id/{skill_id}/versions/1/restore"),
            serde_json::Value::Null,
        )
        .await;
        assert_eq!(st, StatusCode::CONFLICT);
    }
}
