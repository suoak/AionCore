use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

#[tokio::test]
async fn test_local_mode_skips_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();

    let router = aionui_app::create_router(&services).await.expect("build router");

    // Health check should work
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // An authenticated endpoint should work WITHOUT a token in local mode
    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_ne!(response.status(), StatusCode::FORBIDDEN);

    services.database.close().await;
}

#[tokio::test]
async fn test_non_local_mode_requires_auth() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = aionui_app::AppServices::from_config(db, &aionui_app::AppConfig::default())
        .await
        .unwrap();

    let router = aionui_app::create_router(&services).await.expect("build router");

    let response = router
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["code"], "UNAUTHORIZED");

    services.database.close().await;
}

#[tokio::test]
async fn test_local_mode_aionpro_requires_session_and_allows_bootstrap_provision_without_csrf() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        identity_mode: aionui_app::IdentityMode::AionPro,
        bootstrap_secret: Some("bootstrap-secret".to_string()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");

    let business_response = router
        .clone()
        .oneshot(Request::builder().uri("/api/settings").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(business_response.status(), StatusCode::UNAUTHORIZED);

    let provision_response = router
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/auth/internal/external-users/pro-app")
                .header("content-type", "application/json")
                .header("x-aioncore-bootstrap-secret", "bootstrap-secret")
                .body(Body::from(r#"{"user_type":"aionpro","username":"Pro App User"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(provision_response.status(), StatusCode::OK);
    let body = to_bytes(provision_response.into_body(), usize::MAX).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["data"]["user_type"], "aionpro");
    assert_ne!(json["data"]["user_id"], "system_default_user");

    services.database.close().await;
}

const LOCAL_SECRET: &str = "e2e-local-secret-0123456789-abcdefghijklmnop";

async fn local_secret_router() -> (axum::Router, aionui_app::AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        local: true,
        identity_mode: aionui_app::IdentityMode::Local,
        local_api_secret: Some(aionui_auth::LocalApiSecret::parse(LOCAL_SECRET).unwrap()),
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");
    (router, services)
}

#[tokio::test]
async fn local_mode_with_secret_rejects_unauthenticated_loopback_callers() {
    let (router, services) = local_secret_router().await;

    // Health stays public for the launcher's readiness probe.
    let response = router
        .clone()
        .oneshot(Request::builder().uri("/health").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    for uri in [
        "/api/settings",
        "/api/mcp/servers",
        "/api/providers",
        "/api/conversations",
    ] {
        let response = router
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri} must require auth");
    }

    // Creating an MCP server (persistent command execution) is refused.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/mcp/servers")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"name":"evil","transport":{"type":"stdio","command":"sh"}}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // WebUI password reset (returns the new password) is refused.
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/webui/reset-password")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    services.database.close().await;
}

#[tokio::test]
async fn local_mode_with_secret_accepts_desktop_header() {
    let (router, services) = local_secret_router().await;

    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/settings")
                .header("x-aionui-local-secret", LOCAL_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let response = router
        .oneshot(
            Request::builder()
                .uri("/api/auth/internal/users/system")
                .header("x-aionui-local-secret", LOCAL_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    services.database.close().await;
}

#[tokio::test]
async fn local_mode_cors_rejects_foreign_origins() {
    let (router, services) = local_secret_router().await;

    let preflight = |origin: &'static str| {
        Request::builder()
            .method("OPTIONS")
            .uri("/api/settings")
            .header("origin", origin)
            .header("access-control-request-method", "GET")
            .header("access-control-request-headers", "x-aionui-local-secret")
            .body(Body::empty())
            .unwrap()
    };

    let response = router.clone().oneshot(preflight("https://evil.example")).await.unwrap();
    assert!(
        response.headers().get("access-control-allow-origin").is_none(),
        "foreign origin must not be allowed"
    );

    let response = router
        .clone()
        .oneshot(preflight("http://localhost:5173"))
        .await
        .unwrap();
    assert_eq!(
        response.headers().get("access-control-allow-origin").unwrap(),
        "http://localhost:5173"
    );

    let response = router.oneshot(preflight("file://")).await.unwrap();
    assert_eq!(
        response.headers().get("access-control-allow-origin").unwrap(),
        "file://"
    );

    services.database.close().await;
}

#[tokio::test]
async fn forged_host_is_rejected_before_auth() {
    let (router, services) = local_secret_router().await;

    for host in ["evil.example", "evil.example:25808", "127.0.0.1.nip.io:25808"] {
        for uri in ["/health", "/api/settings", "/ws"] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header("host", host)
                        .header("x-aionui-local-secret", LOCAL_SECRET)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::FORBIDDEN, "{host} {uri}");
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["code"], "HOST_NOT_ALLOWED", "{host} {uri}");
        }
    }

    for host in ["127.0.0.1:25808", "localhost:5173", "[::1]:49152"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/settings")
                    .header("host", host)
                    .header("x-aionui-local-secret", LOCAL_SECRET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{host}");
    }

    services.database.close().await;
}

#[tokio::test]
async fn configured_remote_host_is_accepted() {
    let db = aionui_db::init_database_memory().await.unwrap();
    let config = aionui_app::AppConfig {
        allowed_hosts: vec!["workmate.lan".into()],
        ..Default::default()
    };
    let services = aionui_app::AppServices::from_config(db, &config).await.unwrap();
    let router = aionui_app::create_router(&services).await.expect("build router");

    let status = |host: &'static str| {
        let router = router.clone();
        async move {
            router
                .oneshot(
                    Request::builder()
                        .uri("/health")
                        .header("host", host)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status()
        }
    };
    assert_eq!(status("workmate.lan:25809").await, StatusCode::OK);
    assert_eq!(status("other.lan:25809").await, StatusCode::FORBIDDEN);

    services.database.close().await;
}
