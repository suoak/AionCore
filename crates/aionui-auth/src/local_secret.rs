//! Per-launch local API secret for the embedded desktop (`--local`) mode.
//!
//! In local mode the backend historically trusted every loopback caller as the
//! desktop user. Any process on the machine (agent subprocesses, MCP servers,
//! scripts) could therefore drive the full API. The desktop launcher now mints
//! a random secret per launch and hands it to the backend over stdin (never on
//! argv, never in the environment). Requests that present it in
//! [`LOCAL_API_SECRET_HEADER`] are the desktop app; everything else must
//! authenticate like a WebUI client (session cookie / bearer JWT) or through
//! the scoped conversation-runtime token channel.

use std::fmt;
use std::sync::Arc;

use axum::http::HeaderMap;

/// Header carrying the per-launch local API secret.
pub const LOCAL_API_SECRET_HEADER: &str = "x-aionui-local-secret";

/// Environment flag telling the backend to read the local secret from stdin.
///
/// The flag carries no secret material itself; the secret is the first line
/// written to the backend's stdin by the launcher.
pub const LOCAL_AUTH_STDIN_ENV: &str = "AIONUI_LOCAL_AUTH_STDIN";

/// Minimum accepted secret length (characters). The desktop launcher sends 43
/// base64url characters (32 random bytes).
pub const LOCAL_API_SECRET_MIN_LEN: usize = 32;
const LOCAL_API_SECRET_MAX_LEN: usize = 512;

/// Validated local API secret. `Debug` never prints the value.
#[derive(Clone)]
pub struct LocalApiSecret(Arc<str>);

/// Why a candidate local secret was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalApiSecretError {
    TooShort,
    TooLong,
    InvalidCharacters,
}

impl fmt::Display for LocalApiSecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooShort => write!(
                f,
                "local API secret must be at least {LOCAL_API_SECRET_MIN_LEN} characters"
            ),
            Self::TooLong => write!(f, "local API secret is too long"),
            Self::InvalidCharacters => write!(f, "local API secret must be visible ASCII without spaces"),
        }
    }
}

impl std::error::Error for LocalApiSecretError {}

impl LocalApiSecret {
    /// Validate and wrap a secret. Surrounding whitespace (the trailing newline
    /// from stdin) is trimmed first.
    pub fn parse(raw: &str) -> Result<Self, LocalApiSecretError> {
        let value = raw.trim();
        if value.len() < LOCAL_API_SECRET_MIN_LEN {
            return Err(LocalApiSecretError::TooShort);
        }
        if value.len() > LOCAL_API_SECRET_MAX_LEN {
            return Err(LocalApiSecretError::TooLong);
        }
        if !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(LocalApiSecretError::InvalidCharacters);
        }
        Ok(Self(Arc::from(value)))
    }

    /// Constant-time comparison against a presented value.
    pub fn matches(&self, presented: &str) -> bool {
        constant_time_eq(presented.as_bytes(), self.0.as_bytes())
    }

    /// Whether `headers` carry this secret in [`LOCAL_API_SECRET_HEADER`].
    pub fn matches_headers(&self, headers: &HeaderMap) -> bool {
        headers
            .get(LOCAL_API_SECRET_HEADER)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| self.matches(value.trim()))
    }
}

impl fmt::Debug for LocalApiSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("LocalApiSecret([REDACTED])")
    }
}

/// Length-independent constant-time byte comparison.
pub(crate) fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let max_len = left.len().max(right.len());
    let mut diff = left.len() ^ right.len();
    for idx in 0..max_len {
        let l = left.get(idx).copied().unwrap_or(0);
        let r = right.get(idx).copied().unwrap_or(0);
        diff |= usize::from(l ^ r);
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    const GOOD: &str = "0123456789abcdefghijklmnopqrstuvwxyzABCDEFG";

    #[test]
    fn parse_trims_and_validates() {
        let secret = LocalApiSecret::parse(&format!("{GOOD}\n")).unwrap();
        assert!(secret.matches(GOOD));
        assert!(!secret.matches("nope"));
        assert_eq!(
            LocalApiSecret::parse("short").unwrap_err(),
            LocalApiSecretError::TooShort
        );
        assert_eq!(
            LocalApiSecret::parse(&format!("{GOOD} {GOOD}")).unwrap_err(),
            LocalApiSecretError::InvalidCharacters
        );
        assert_eq!(
            LocalApiSecret::parse(&"a".repeat(LOCAL_API_SECRET_MAX_LEN + 1)).unwrap_err(),
            LocalApiSecretError::TooLong
        );
    }

    #[test]
    fn debug_is_redacted() {
        let secret = LocalApiSecret::parse(GOOD).unwrap();
        let printed = format!("{secret:?}");
        assert!(!printed.contains(GOOD));
        assert!(printed.contains("REDACTED"));
    }

    #[test]
    fn matches_headers_requires_exact_value() {
        let secret = LocalApiSecret::parse(GOOD).unwrap();
        let mut headers = HeaderMap::new();
        assert!(!secret.matches_headers(&headers));
        headers.insert(
            LOCAL_API_SECRET_HEADER,
            HeaderValue::from_static("wrong-wrong-wrong-wrong-wrong-wrong"),
        );
        assert!(!secret.matches_headers(&headers));
        headers.insert(LOCAL_API_SECRET_HEADER, HeaderValue::from_str(GOOD).unwrap());
        assert!(secret.matches_headers(&headers));
    }
}
