//! M9 E13 B2/B5: the production built-in packs through the real app startup and router.
//!
//! A fresh Host data directory, no `WORKMATE_HUB_SIGNING_PUBKEYS` configured by the test: startup
//! installs and binds the three curator-signed packs from `builtin_extensions::builtins()`, the
//! registry scans the signed-install directory, and the authenticated routes list the theme, the
//! panel and the work-panel view and serve their verified assets with the extension asset CSP.

mod common;

use axum::http::StatusCode;
use http_body_util::BodyExt;
use tower::ServiceExt;

use aionui_app::{AppConfig, AppServices, create_router};

use common::{body_json, get_with_token, setup_and_login};

const ASSET_CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
img-src 'self' data:; font-src 'self' data:; connect-src 'none'; object-src 'none'; base-uri 'none'; \
form-action 'none'; sandbox allow-scripts";

fn repo_root() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

async fn build_app_in(data_dir: &std::path::Path) -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = AppConfig {
        data_dir: data_dir.to_path_buf(),
        work_dir: data_dir.to_path_buf(),
        app_version: "1.0.0".to_string(),
        ..Default::default()
    };
    let services = AppServices::from_config(db, &config).await.unwrap();
    let router = create_router(&services).await.expect("build router");
    (router, services)
}

fn items(json: &serde_json::Value) -> Vec<serde_json::Value> {
    json["data"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn production_builtins_install_at_startup_and_serve_through_the_app_router() {
    let tmp = tempfile::tempdir().unwrap();
    let data_dir = tmp.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();

    // Two starts on the same data dir: install, then restore from disk.
    for pass in 0..2 {
        let (mut app, services) = build_app_in(&data_dir).await;
        let user = format!("builtin{pass}");
        let (token, _csrf) = setup_and_login(&mut app, &services, &user, "pass1").await;

        for name in [
            "workmate.theme-dusk-ember",
            "workmate.sample-shortcuts",
            "workmate.sample-checklist",
        ] {
            assert!(
                data_dir
                    .join("extensions-signed")
                    .join(name)
                    .join("1.0.0")
                    .join("aion-extension.json")
                    .is_file(),
                "{name} installed under the Host signed directory"
            );
        }

        let get = |uri: &str| app.clone().oneshot(get_with_token(uri, &token));

        let themes = items(&body_json(get("/api/extensions/themes").await.unwrap()).await);
        let dusk: Vec<_> = themes
            .iter()
            .filter(|t| {
                t["id"]
                    .as_str()
                    .unwrap_or_default()
                    .starts_with("ext-workmate.theme-dusk-ember-")
            })
            .collect();
        assert_eq!(dusk.len(), 1, "pass {pass}: {themes:?}");
        assert!(dusk[0]["name"].as_str().unwrap().starts_with("Dusk & Ember"));
        assert!(dusk[0]["css"].as_str().unwrap().contains("[data-theme='dark']"));

        let panels = items(&body_json(get("/api/extensions/panels").await.unwrap()).await);
        let shortcuts: Vec<_> = panels
            .iter()
            .filter(|p| p["extensionName"] == "workmate.sample-shortcuts")
            .collect();
        assert_eq!(shortcuts.len(), 1, "pass {pass}: {panels:?}");
        assert_eq!(shortcuts[0]["title"], "Shortcuts & tips");

        let views = items(&body_json(get("/api/extensions/work-panel-views").await.unwrap()).await);
        let checklist: Vec<_> = views
            .iter()
            .filter(|v| v["extensionName"] == "workmate.sample-checklist")
            .collect();
        assert_eq!(checklist.len(), 1, "pass {pass}: {views:?}");
        assert_eq!(checklist[0]["title"], "Plan checklist");
        assert_eq!(checklist[0]["placement"], "secondary");

        for (entry, rel) in [
            (
                shortcuts[0]["entryPoint"].as_str().unwrap().to_owned(),
                "workmate.sample-shortcuts/panels/shortcuts.html",
            ),
            (
                checklist[0]["entryPoint"].as_str().unwrap().to_owned(),
                "workmate.sample-checklist/views/checklist.html",
            ),
            (
                "/api/extensions/workmate.theme-dusk-ember/assets/assets/cover.svg".to_owned(),
                "workmate.theme-dusk-ember/assets/cover.svg",
            ),
        ] {
            let response = get(&entry).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{entry}");
            let headers = response.headers().clone();
            let csp: Vec<_> = headers.get_all("content-security-policy").iter().collect();
            assert_eq!(csp, vec![ASSET_CSP], "{entry}");
            assert!(headers.get("x-frame-options").is_none(), "{entry}");
            assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(
                body.as_ref(),
                std::fs::read(repo_root().join("extensions").join(rel)).unwrap(),
                "{entry}"
            );
        }

        // The signed manifest is never an asset.
        let response = get("/api/extensions/workmate.sample-shortcuts/assets/aion-extension.json")
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
}
