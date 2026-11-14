use axum::extract::Request;
use axum::http::header::{
    CONTENT_SECURITY_POLICY, HeaderValue, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS, X_XSS_PROTECTION,
};
use axum::middleware::Next;
use axum::response::Response;

fn allows_embedding(path: &str) -> bool {
    let mut segments = path.trim_start_matches('/').split('/');
    matches!(
        (segments.next(), segments.next(), segments.next(), segments.next(),),
        (
            Some("api"),
            Some("extensions"),
            Some(_extension_name),
            Some("assets" | "asset-session")
        )
    )
}

/// Response CSP for extension-served assets (`/api/extensions/{name}/assets/...`
/// and `/api/extensions/{name}/asset-session/...`).
///
/// Everything except same-origin scripts/styles/images/fonts is denied: no
/// network (`connect-src 'none'`), no plugins, no `<base>` rewrites, no form
/// posts. `'unsafe-inline'` is kept for `script-src` / `style-src` only because
/// existing settings-tab pages ship inline `<style>` / `<script>`. Pages that
/// carry their own stricter `<meta http-equiv="Content-Security-Policy">`
/// (the built-in samples) still block inline code, since browsers enforce every
/// policy present (header ∩ meta).
///
/// `sandbox allow-scripts` (internal security review, finding F1): an extension
/// page is served from the **same origin as the app** (WebUI) or as the backend
/// API (desktop). Without a sandbox, its script could reach `parent` / a
/// same-origin popup and call the API with the user's session, whatever the
/// embedding iframe's `sandbox` attribute says (an iframe with
/// `allow-scripts allow-same-origin`, or the page opened top-level, is not
/// isolated). The header sandbox gives the document an opaque origin in every
/// context: scripts run, but there is no same-origin DOM, storage or cookie
/// access, no popups, no top navigation and no form submission. Same-URL
/// subresources (`'self'`) still load, so packaged pages keep working.
pub const EXTENSION_ASSET_CSP: &str = "default-src 'none'; script-src 'self' 'unsafe-inline'; \
style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self' data:; connect-src 'none'; \
object-src 'none'; base-uri 'none'; form-action 'none'; sandbox allow-scripts";

/// Office preview proxies serve content loaded in same-origin iframes by
/// the web UI; `DENY` here blanks the preview (iOfficeAI/AionUi#3177).
fn allows_same_origin_embedding(path: &str) -> bool {
    path.starts_with("/api/ppt-proxy/") || path.starts_with("/api/office-watch-proxy/")
}

/// Middleware that adds security response headers to every response.
///
/// Headers set:
/// - `X-Frame-Options: DENY` — prevent clickjacking on non-embeddable routes
///   (`SAMEORIGIN` + `frame-ancestors 'self'` on office preview proxies)
/// - `Content-Security-Policy: EXTENSION_ASSET_CSP` on extension asset routes
/// - `X-Content-Type-Options: nosniff` — prevent MIME sniffing
/// - `X-XSS-Protection: 1; mode=block` — enable XSS filter
/// - `Referrer-Policy: strict-origin-when-cross-origin` — limit referrer leakage
pub async fn security_headers_middleware(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    if allows_same_origin_embedding(&path) {
        headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("SAMEORIGIN"));
        // Append rather than insert: CSP allows multiple policies and all
        // are enforced, so an upstream policy is never silently dropped.
        headers.append(
            CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("frame-ancestors 'self'"),
        );
    } else if allows_embedding(&path) {
        // Extension assets stay frameable (no X-Frame-Options) but are locked
        // down by CSP. Append so a handler-set policy is never dropped.
        headers.append(CONTENT_SECURITY_POLICY, HeaderValue::from_static(EXTENSION_ASSET_CSP));
    } else {
        headers.insert(X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    }
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers.insert(X_XSS_PROTECTION, HeaderValue::from_static("1; mode=block"));
    headers.insert(
        REFERRER_POLICY,
        HeaderValue::from_static("strict-origin-when-cross-origin"),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::routing::get;
    use axum::{Router, middleware};
    use tower::ServiceExt;

    #[tokio::test]
    async fn all_security_headers_present() {
        let app = Router::new()
            .route("/test", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(axum::http::Request::builder().uri("/test").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
        assert_eq!(response.headers().get("x-content-type-options").unwrap(), "nosniff");
        assert_eq!(response.headers().get("x-xss-protection").unwrap(), "1; mode=block");
        assert_eq!(
            response.headers().get("referrer-policy").unwrap(),
            "strict-origin-when-cross-origin"
        );
    }

    #[tokio::test]
    async fn security_headers_on_error_responses() {
        let app = Router::new()
            .route(
                "/error",
                get(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
            )
            .layer(middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/error")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::INTERNAL_SERVER_ERROR);
        // Security headers still present even on error responses
        assert_eq!(response.headers().get("x-frame-options").unwrap(), "DENY");
    }

    #[tokio::test]
    async fn office_proxy_routes_get_sameorigin_frame_headers() {
        // Regression for iOfficeAI/AionUi#3177: the office preview proxies
        // serve iframe content for the same-origin web UI; `DENY` blanks
        // the preview iframe in browsers.
        let app = Router::new()
            .route("/api/ppt-proxy/{port}", get(|| async { "ok" }))
            .route("/api/office-watch-proxy/{port}/{*path}", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers_middleware));

        for uri in ["/api/ppt-proxy/41307", "/api/office-watch-proxy/41307/some/page"] {
            let response = app
                .clone()
                .oneshot(axum::http::Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();

            assert_eq!(
                response.headers().get("x-frame-options").unwrap(),
                "SAMEORIGIN",
                "{uri} must stay frameable by the same-origin web UI"
            );
            assert_eq!(
                response.headers().get("content-security-policy").unwrap(),
                "frame-ancestors 'self'",
                "{uri} should carry the modern frame-ancestors policy"
            );
            assert_eq!(response.headers().get("x-content-type-options").unwrap(), "nosniff");
        }
    }

    #[tokio::test]
    async fn extension_asset_routes_omit_frame_deny_header() {
        let app = Router::new()
            .route(
                "/api/extensions/hello/assets/settings/index.html",
                get(|| async { "ok" }),
            )
            .layer(middleware::from_fn(security_headers_middleware));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/api/extensions/hello/assets/settings/index.html")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.headers().get("x-frame-options").is_none());
        assert_eq!(response.headers().get("x-content-type-options").unwrap(), "nosniff");
    }

    async fn get_headers(app: Router, uri: &str) -> axum::http::HeaderMap {
        app.oneshot(axum::http::Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
            .headers()
            .clone()
    }

    #[test]
    fn extension_asset_csp_locks_down_everything_but_same_origin_inline_ui() {
        let directives: Vec<&str> = EXTENSION_ASSET_CSP.split("; ").collect();
        assert_eq!(
            directives,
            vec![
                "default-src 'none'",
                "script-src 'self' 'unsafe-inline'",
                "style-src 'self' 'unsafe-inline'",
                "img-src 'self' data:",
                "font-src 'self' data:",
                "connect-src 'none'",
                "object-src 'none'",
                "base-uri 'none'",
                "form-action 'none'",
                "sandbox allow-scripts",
            ]
        );
        // The sandbox must never grant a same-origin escape, popups, top navigation or forms.
        let sandbox = directives.iter().find(|d| d.starts_with("sandbox")).unwrap();
        for forbidden in [
            "allow-same-origin",
            "allow-popups",
            "allow-top-navigation",
            "allow-forms",
            "allow-modals",
            "allow-downloads",
        ] {
            assert!(!sandbox.contains(forbidden), "{forbidden}");
        }
        assert!(HeaderValue::from_str(EXTENSION_ASSET_CSP).is_ok());
    }

    #[tokio::test]
    async fn extension_asset_and_asset_session_routes_carry_csp() {
        let app = Router::new()
            .route("/api/extensions/{name}/assets/{*path}", get(|| async { "ok" }))
            .route(
                "/api/extensions/{name}/asset-session/{token}/{*path}",
                get(|| async { "ok" }),
            )
            .layer(middleware::from_fn(security_headers_middleware));

        for uri in [
            "/api/extensions/hello/assets/settings/index.html",
            "/api/extensions/workmate.sample-shortcuts/assets/panels/shortcuts.html",
            "/api/extensions/hello/asset-session/tok123/settings/index.html",
        ] {
            let headers = get_headers(app.clone(), uri).await;
            let csp: Vec<_> = headers.get_all("content-security-policy").iter().collect();
            assert_eq!(csp.len(), 1, "{uri} should carry exactly one CSP");
            assert_eq!(csp[0], EXTENSION_ASSET_CSP, "{uri}");
            assert!(headers.get("x-frame-options").is_none(), "{uri} must stay frameable");
            assert_eq!(headers.get("x-content-type-options").unwrap(), "nosniff");
        }
    }

    #[tokio::test]
    async fn extension_asset_csp_is_appended_not_replacing_a_handler_policy() {
        let app = Router::new()
            .route(
                "/api/extensions/{name}/assets/{*path}",
                get(|| async { ([("content-security-policy", "sandbox allow-scripts")], "ok") }),
            )
            .layer(middleware::from_fn(security_headers_middleware));

        let headers = get_headers(app, "/api/extensions/hello/assets/a.html").await;
        let csp: Vec<_> = headers
            .get_all("content-security-policy")
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .collect();
        assert_eq!(
            csp,
            vec!["sandbox allow-scripts".to_string(), EXTENSION_ASSET_CSP.to_string()]
        );
    }

    #[tokio::test]
    async fn non_asset_extension_routes_get_no_asset_csp_and_stay_deny() {
        let app = Router::new()
            .route("/api/extensions/{name}/asset-token", get(|| async { "ok" }))
            .route("/api/extensions/panels", get(|| async { "ok" }))
            .route("/api/extensions/{name}/settings", get(|| async { "ok" }))
            .layer(middleware::from_fn(security_headers_middleware));

        for uri in [
            "/api/extensions/hello/asset-token",
            "/api/extensions/panels",
            "/api/extensions/hello/settings",
        ] {
            let headers = get_headers(app.clone(), uri).await;
            assert!(headers.get("content-security-policy").is_none(), "{uri}");
            assert_eq!(headers.get("x-frame-options").unwrap(), "DENY", "{uri}");
        }
    }
}
