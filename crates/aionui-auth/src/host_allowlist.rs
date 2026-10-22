#![allow(clippy::disallowed_types)]

//! `Host` header allowlist (DNS-rebinding defence).
//!
//! The backend listens on loopback and trusts loopback callers (local mode
//! authenticates with a per-launch secret, but cookies, CORS and the WebUI
//! session all key off the origin). A DNS-rebinding page reaches
//! `127.0.0.1:<port>` under its own hostname, so the `Host` it sends is not one
//! of ours: rejecting unknown `Host` values closes that path.
//!
//! Allowed by default: `localhost`, `127.0.0.1`, `[::1]` (any port) and the
//! address the server is bound to when it is a concrete host. WebUI remote
//! access through the bundled web host rewrites `Host` to loopback, so it needs
//! nothing extra; a deployment that exposes the backend under other names lists
//! them with `--allowed-hosts` / `AIONCORE_ALLOWED_HOSTS` (comma separated,
//! `*` disables the check).
//!
//! A request without a `Host` header is let through: every browser sends one,
//! and the check exists to stop browsers, not local tools.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::Response;

use aionui_common::ApiError;

/// Environment variable carrying extra allowed hosts (same syntax as the flag).
pub const ALLOWED_HOSTS_ENV: &str = "AIONCORE_ALLOWED_HOSTS";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostAllowlist {
    allow_any: bool,
    /// Lower-cased host names / IP literals (no port, IPv6 without brackets).
    hosts: Vec<String>,
}

impl HostAllowlist {
    /// Build the allowlist for a server bound to `bind_host`, plus `extra`
    /// entries (`*` disables the check). Wildcard binds (`0.0.0.0`, `::`) are
    /// served under names we cannot know, so they keep the check off unless
    /// the operator lists the names explicitly.
    pub fn new(bind_host: &str, extra: &[String]) -> Self {
        let mut hosts: Vec<String> = ["localhost", "127.0.0.1", "::1"]
            .iter()
            .map(|h| (*h).to_owned())
            .collect();
        let mut allow_any = false;
        let mut configured = false;
        for entry in extra.iter().flat_map(|e| e.split(',')) {
            let entry = entry.trim();
            if entry.is_empty() {
                continue;
            }
            configured = true;
            if entry == "*" {
                allow_any = true;
            } else if let Some(host) = normalize_host(entry) {
                hosts.push(host);
            }
        }
        let bind = bind_host.trim();
        if is_wildcard(bind) {
            if !configured {
                allow_any = true;
            }
        } else if let Some(host) = normalize_host(bind) {
            hosts.push(host);
        }
        hosts.sort();
        hosts.dedup();
        Self { allow_any, hosts }
    }

    /// Loopback-only allowlist (tests, embedded use).
    pub fn loopback() -> Self {
        Self::new("127.0.0.1", &[])
    }

    pub fn allows_any(&self) -> bool {
        self.allow_any
    }

    /// Whether a raw `Host` header value is acceptable.
    pub fn allows(&self, host_header: &str) -> bool {
        if self.allow_any {
            return true;
        }
        match normalize_host(host_header) {
            Some(host) => self.hosts.contains(&host),
            None => false,
        }
    }
}

fn is_wildcard(host: &str) -> bool {
    matches!(host, "0.0.0.0" | "::" | "[::]" | "::0")
}

/// `Host` value → lower-cased host without port; IPv6 brackets dropped.
/// Rejects anything with characters a hostname cannot carry.
fn normalize_host(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    let host = if let Some(rest) = value.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        if !(tail.is_empty() || valid_port(tail.strip_prefix(':')?)) {
            return None;
        }
        addr.to_owned()
    } else if value.matches(':').count() > 1 {
        // Bare IPv6 literal (config entries only; a Host header brackets it).
        value.clone()
    } else {
        match value.split_once(':') {
            Some((name, port)) if valid_port(port) => name.to_owned(),
            Some(_) => return None,
            None => value.clone(),
        }
    };
    let host = host.trim_end_matches('.').to_owned();
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':'))
    {
        return None;
    }
    Some(host)
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port.len() <= 5 && port.bytes().all(|b| b.is_ascii_digit())
}

/// Reject requests whose `Host` is not on the allowlist with `403 HOST_NOT_ALLOWED`.
pub async fn host_allowlist_middleware(
    State(allowlist): State<Arc<HostAllowlist>>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    if !allowlist.allow_any {
        let host = request
            .headers()
            .get(header::HOST)
            .map(|value| value.to_str().unwrap_or("\u{0}"))
            .or_else(|| request.uri().authority().map(|authority| authority.as_str()));
        if let Some(host) = host
            && !allowlist.allows(host)
        {
            tracing::warn!(host = %host.escape_debug(), "rejected request with a Host outside the allowlist");
            return Err(ApiError::coded(
                StatusCode::FORBIDDEN,
                "HOST_NOT_ALLOWED",
                "Request Host is not allowed for this server.",
                None,
            ));
        }
    }
    Ok(next.run(request).await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts_are_allowed_on_any_port() {
        let list = HostAllowlist::loopback();
        for host in [
            "localhost",
            "localhost:25808",
            "LOCALHOST:5173",
            "127.0.0.1",
            "127.0.0.1:49152",
            "[::1]",
            "[::1]:25808",
            "localhost.",
        ] {
            assert!(list.allows(host), "{host}");
        }
    }

    #[test]
    fn foreign_and_malformed_hosts_are_rejected() {
        let list = HostAllowlist::loopback();
        for host in [
            "evil.example",
            "evil.example:25808",
            "127.0.0.1.nip.io",
            "localhost.evil.example",
            "192.168.1.5:25809",
            "127.0.0.2",
            "[::2]:1",
            "localhost:abc",
            "localhost:25808@evil",
            "",
            "[::1",
        ] {
            assert!(!list.allows(host), "{host}");
        }
    }

    #[test]
    fn concrete_bind_host_and_configured_hosts_are_allowed() {
        let list = HostAllowlist::new("192.168.1.5", &["workmate.lan, 10.0.0.7:8080".into(), "fd00::1".into()]);
        assert!(list.allows("192.168.1.5:25808"));
        assert!(list.allows("workmate.lan"));
        assert!(list.allows("10.0.0.7:9999"));
        assert!(list.allows("[fd00::1]:25808"));
        assert!(!list.allows("other.lan"));
        assert!(!list.allows_any());
    }

    #[test]
    fn wildcard_bind_without_configuration_keeps_the_check_off() {
        assert!(HostAllowlist::new("0.0.0.0", &[]).allows("anything.example"));
        assert!(HostAllowlist::new("::", &[]).allows_any());
        let configured = HostAllowlist::new("0.0.0.0", &["workmate.example".into()]);
        assert!(configured.allows("workmate.example:443"));
        assert!(!configured.allows("evil.example"));
    }

    #[test]
    fn star_disables_the_check() {
        assert!(HostAllowlist::new("127.0.0.1", &["*".into()]).allows("evil.example"));
    }
}
