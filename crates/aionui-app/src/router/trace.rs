//! HTTP request access-log layer.

use std::time::Instant;

use aionui_common::{ApiErrorLogContext, generate_short_id};
use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;

const REQUEST_ID_HEADER: &str = "x-request-id";
const MAX_QUERY_KEYS: usize = 16;

pub(super) fn with_access_log(router: Router) -> Router {
    router.layer(middleware::from_fn(access_log))
}

async fn access_log(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let path = redact_path(request.uri().path());
    let query_keys = query_keys(request.uri().query());
    let request_id = request
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.trim().is_empty())
        .map(str::to_owned)
        .unwrap_or_else(generate_short_id);

    let response = next.run(request).await;
    let status = response.status().as_u16();
    let latency_ms = started.elapsed().as_millis() as u64;
    let error_context = response.extensions().get::<ApiErrorLogContext>();
    let error_code = error_context.map(|context| context.code).unwrap_or("");
    let error_message = error_context.map(|context| context.message.as_str()).unwrap_or("");

    if status >= 500 {
        tracing::error!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            error_code,
            error_message,
            "http response"
        );
    } else if status >= 400 {
        tracing::warn!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            error_code,
            error_message,
            "http response"
        );
    } else {
        tracing::info!(
            request_id = %request_id,
            method = %method,
            path = %path,
            query_keys = %query_keys,
            status,
            latency_ms,
            "http response"
        );
    }

    response
}

/// Path segment that replaces a credential carried in the URL path.
const REDACTED_SEGMENT: &str = "[redacted]";

/// The request path as logged: credentials carried **in the path** are replaced, like query values
/// are omitted. Today that is the extension asset-session token
/// (`/api/extensions/{name}/asset-session/{token}/...`), a bearer credential for that extension's
/// assets; it must not end up in log files or bug reports.
fn redact_path(path: &str) -> String {
    let segments: Vec<&str> = path.split('/').collect();
    // ["", "api", "extensions", name, "asset-session", token, ...]
    if segments.len() > 5 && segments[1] == "api" && segments[2] == "extensions" && segments[4] == "asset-session" {
        let mut out = segments.clone();
        out[5] = REDACTED_SEGMENT;
        return out.join("/");
    }
    path.to_owned()
}

fn query_keys(query: Option<&str>) -> String {
    query
        .into_iter()
        .flat_map(|query| query.split('&'))
        .filter_map(|pair| {
            let key = pair.split_once('=').map_or(pair, |(key, _)| key).trim();
            (!key.is_empty()).then_some(key)
        })
        .take(MAX_QUERY_KEYS)
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_session_tokens_are_redacted_from_logged_paths() {
        assert_eq!(
            redact_path("/api/extensions/hello/asset-session/0123abcd/settings/index.html"),
            "/api/extensions/hello/asset-session/[redacted]/settings/index.html"
        );
        assert_eq!(
            redact_path("/api/extensions/hello/asset-session/0123abcd"),
            "/api/extensions/hello/asset-session/[redacted]"
        );
        for unchanged in [
            "/api/extensions/hello/assets/settings/index.html",
            "/api/extensions/hello/asset-token",
            "/api/extensions",
            "/api/conversations/abc",
            "/",
        ] {
            assert_eq!(redact_path(unchanged), unchanged);
        }
    }

    #[test]
    fn query_keys_omit_values() {
        assert_eq!(
            query_keys(Some("path=/Users/alice/project&token=secret&flag&empty=")),
            "path,token,flag,empty"
        );
    }
}
