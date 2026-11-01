//! M8 E10-7 regression tests (CSBU WorkMate skills 2.0).
//!
//! 1. The experience library (`experience_articles`, including E10-6
//!    evolution evidence) never reaches daily chat: not the skill catalog,
//!    not the resolver an agent session uses, not the per-conversation skill
//!    view, and not the first-message skills index.
//! 2. An assistant's skill binding survives "update from source" and
//!    "restore version": the binding is kept and the next session resolves
//!    the new current version.
//!
//! Runs through `create_router` against an in-memory DB rooted at a temp
//! data dir, so the governance and skill-evolution routes are the production
//! ones.

mod common;

use std::path::Path;
use std::sync::Arc;

use aionui_ai_agent::{SkillIndex, prepare_first_message_with_skills_index};
use aionui_app::{AppConfig, AppServices, create_router};
use aionui_conversation::skill_resolver::{ExtensionSkillResolver, SkillResolver};
use aionui_db::{
    EvolutionEvidence, EvolutionSignal, SqliteExperienceArticleRepository, SqliteSkillRepository,
    record_evolution_evidence,
};
use axum::http::StatusCode;
use serde_json::{Value, json};
use tempfile::TempDir;
use tower::ServiceExt;

use common::{body_json, get_with_token, json_with_token, setup_and_login};

const SECRET: &str = "E107_EXPERIENCE_MARKER_do_not_leak";
const EVIDENCE_SECRET: &str = "E107_EVIDENCE_MARKER_do_not_leak";

struct Fixture {
    app: axum::Router,
    services: AppServices,
    token: String,
    csrf: String,
    user_id: String,
    src_root: std::path::PathBuf,
    _tmp: TempDir,
}

async fn fixture() -> Fixture {
    let tmp = TempDir::new().unwrap();
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: tmp.path().join("data"),
        work_dir: tmp.path().join("work"),
        ..Default::default()
    };
    std::fs::create_dir_all(&config.data_dir).unwrap();
    std::fs::create_dir_all(&config.work_dir).unwrap();
    let services = AppServices::from_config(db, &config).await.unwrap();
    let mut app = create_router(&services).await.expect("build router");
    let (token, csrf) = setup_and_login(&mut app, &services, "admin", "e107-password").await;
    let user_id = services
        .user_repo
        .find_by_username("admin")
        .await
        .unwrap()
        .expect("admin user")
        .id;
    let src_root = tmp.path().join("external");
    Fixture {
        app,
        services,
        token,
        csrf,
        user_id,
        src_root,
        _tmp: tmp,
    }
}

impl Fixture {
    async fn send(&self, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let req = if method == "GET" {
            get_with_token(uri, &self.token)
        } else {
            json_with_token(method, uri, body, &self.token, &self.csrf)
        };
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        (status, body_json(resp).await)
    }

    fn resolver(&self) -> ExtensionSkillResolver {
        ExtensionSkillResolver::new(
            self.services.skill_paths.clone(),
            Arc::new(SqliteSkillRepository::new(self.services.database.pool().clone())),
        )
    }
}

fn write_skill(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: E10-7 bound skill\n---\n{body}\n"),
    )
    .unwrap();
}

fn tree_contains(root: &Path, needle: &str) -> Option<std::path::PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        // Skill views link to skill roots; follow them, they are small.
        if path.is_dir() {
            if let Some(hit) = tree_contains(&path, needle) {
                return Some(hit);
            }
        } else if std::fs::read(&path)
            .map(|bytes| String::from_utf8_lossy(&bytes).contains(needle))
            .unwrap_or(false)
        {
            return Some(path);
        }
    }
    None
}

async fn import_local(f: &Fixture, dir: &Path) -> String {
    let (st, body) = f
        .send(
            "POST",
            "/api/skills/sources/import",
            json!({"kind": "local", "locator": dir.to_string_lossy()}),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{body}");
    let item = &body["data"]["imported"][0];
    assert_eq!(item["skill"]["review_status"], "none", "{body}");
    item["skill"]["skill_id"].as_str().unwrap().to_string()
}

async fn bound_bodies(f: &Fixture, assistant_id: &str) -> (Vec<String>, Vec<String>) {
    let (st, a) = f
        .send("GET", &format!("/api/assistants/{assistant_id}"), Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{a}");
    // `defaults.skills` is what a new session of this assistant starts with.
    let names: Vec<String> = a["data"]["defaults"]["skills"]["value"]
        .as_array()
        .unwrap_or_else(|| panic!("assistant without bound skills: {a}"))
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let bodies = f
        .resolver()
        .load_skill_bodies_for_user(&f.user_id, &names)
        .await
        .into_iter()
        .map(|s| s.body)
        .collect();
    (names, bodies)
}

/// E10-7 (acceptance 6): experience articles and E10-6 evidence stay out of
/// every daily-chat channel.
#[tokio::test]
async fn experience_library_never_reaches_daily_chat() {
    let f = fixture().await;

    // A normal bound skill, so the channels below are not trivially empty.
    let dir = f.src_root.join("e107-daily");
    write_skill(&dir, "e107-daily", "daily body");
    import_local(&f, &dir).await;

    // Experience written through the public route…
    let (st, body) = f
        .send(
            "POST",
            "/api/skill-evolution/experience",
            json!({
                "title": "e107-experience",
                "kind": "pattern",
                "body_md": format!("Always do the thing. {SECRET}"),
            }),
        )
        .await;
    assert!(st.is_success(), "{st} {body}");
    let article_id = body["data"]["id"].as_str().expect("article id").to_string();

    // …and E10-6 evidence written the way the review/board hooks do.
    let repo = SqliteExperienceArticleRepository::new(f.services.database.pool().clone());
    let evidence = record_evolution_evidence(
        &repo,
        &EvolutionEvidence {
            owner_user_id: &f.user_id,
            signal: EvolutionSignal::WorkItemReturned,
            ref_id: "wi_e107",
            conversation_id: None,
            title: "e107-evidence",
            body_md: EVIDENCE_SECRET,
        },
    )
    .await
    .unwrap();

    // The experience library itself still has both.
    let (st, listed) = f.send("GET", "/api/skill-evolution/experience", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{listed}");
    assert!(listed.to_string().contains(SECRET), "{listed}");

    // 1. Skill catalog (what the skill picker and the agent catalog read).
    let (st, skills) = f.send("GET", "/api/skills", Value::Null).await;
    assert_eq!(st, StatusCode::OK, "{skills}");
    let catalog = skills.to_string();
    assert!(catalog.contains("e107-daily"), "{catalog}");
    for needle in [SECRET, EVIDENCE_SECRET, "e107-experience", "e107-evidence"] {
        assert!(!catalog.contains(needle), "skill catalog leaks {needle}: {catalog}");
    }

    // 2. The resolver a chat session uses: experience is not a skill by any
    //    name, and auto-inject builtins never include it.
    let resolver = f.resolver();
    let names: Vec<String> = [
        "e107-daily",
        "e107-experience",
        "e107-evidence",
        article_id.as_str(),
        evidence.id.as_str(),
        "experience",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    let resolved = resolver.resolve_skills_for_user(&f.user_id, &names).await;
    let resolved_names: Vec<&str> = resolved.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(resolved_names, ["e107-daily"]);
    for name in resolver.auto_inject_names().await {
        assert!(
            !name.contains("experience") && !name.contains("e107"),
            "auto-inject {name}"
        );
    }

    // 3. Skill bodies and the first-message skills index.
    let bodies = resolver.load_skill_bodies_for_user(&f.user_id, &names).await;
    assert_eq!(bodies.len(), 1);
    let index: Vec<SkillIndex> = resolved
        .iter()
        .map(|s| SkillIndex {
            name: s.name.clone(),
            description: "E10-7 bound skill".into(),
        })
        .collect();
    let first = prepare_first_message_with_skills_index("hello", &index, None);
    let prompt = format!("{first}\n{}", bodies[0].body);
    assert!(prompt.contains("e107-daily"), "{prompt}");
    for needle in [SECRET, EVIDENCE_SECRET] {
        assert!(!prompt.contains(needle), "daily-chat prompt leaks {needle}");
    }

    // 4. The per-conversation skill view the agent CLI reads.
    let linked = resolver.sync_skill_view(&f.user_id, "conv_e107", &resolved).await;
    assert_eq!(linked, 1);
    let view =
        aionui_extension::skill_view::view_dir(&f.services.skill_paths.data_dir, &f.user_id, "conv_e107").unwrap();
    assert!(view.is_dir());
    for needle in [SECRET, EVIDENCE_SECRET] {
        assert_eq!(tree_contains(&view, needle), None, "skill view leaks {needle}");
    }

    // Nothing in the skills tree on disk carries experience text either.
    for needle in [SECRET, EVIDENCE_SECRET] {
        assert_eq!(
            tree_contains(&f.services.skill_paths.user_skills_dir, needle),
            None,
            "user skills dir leaks {needle}"
        );
    }
}

/// E10-7 (acceptance 2): "update from source" and "restore" keep the
/// assistant binding, and the next session resolves the new version.
#[tokio::test]
async fn assistant_binding_survives_update_and_restore() {
    let f = fixture().await;
    let dir = f.src_root.join("e107-bound");
    write_skill(&dir, "e107-bound", "version one");
    let skill_id = import_local(&f, &dir).await;

    let (st, a) = f
        .send(
            "POST",
            "/api/assistants",
            json!({
                "name": "E10-7 assistant",
                "agent_id": "632f31d2",
                "enabled_skills": ["e107-bound"],
                "custom_skill_names": ["e107-bound"],
            }),
        )
        .await;
    assert!(st.is_success(), "{st} {a}");
    let assistant_id = a["data"]["id"].as_str().expect("assistant id").to_string();

    let (names, bodies) = bound_bodies(&f, &assistant_id).await;
    assert_eq!(names, ["e107-bound"]);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("version one"), "{bodies:?}");

    // Update from source → v2; binding kept, next session sees v2.
    write_skill(&dir, "e107-bound", "version two");
    let (st, r) = f
        .send("POST", &format!("/api/skills/id/{skill_id}/refresh"), Value::Null)
        .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    assert_eq!(r["data"]["changed"], true);
    assert_eq!(r["data"]["version"]["version_no"], 2);
    assert_eq!(r["data"]["skill"]["skill_id"], skill_id.as_str());
    let (names, bodies) = bound_bodies(&f, &assistant_id).await;
    assert_eq!(names, ["e107-bound"]);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("version two"), "{bodies:?}");

    // Restore v1 → appended as v3; binding kept, next session sees v1 text.
    let (st, r) = f
        .send(
            "POST",
            &format!("/api/skills/id/{skill_id}/versions/1/restore"),
            Value::Null,
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{r}");
    let (names, bodies) = bound_bodies(&f, &assistant_id).await;
    assert_eq!(names, ["e107-bound"]);
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("version one"), "{bodies:?}");

    let (_, a) = f
        .send("GET", &format!("/api/assistants/{assistant_id}"), Value::Null)
        .await;
    assert_eq!(
        a["data"]["capabilities"]["default_skill_ids"],
        json!(["e107-bound"]),
        "{a}"
    );
    assert_eq!(
        a["data"]["capabilities"]["custom_skill_names"],
        json!(["e107-bound"]),
        "{a}"
    );

    let (_, d) = f
        .send("GET", &format!("/api/skills/id/{skill_id}/source"), Value::Null)
        .await;
    let versions: Vec<i64> = d["data"]["versions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["version_no"].as_i64().unwrap())
        .collect();
    assert_eq!(versions, [3, 2, 1], "history is append-only: {d}");
}
