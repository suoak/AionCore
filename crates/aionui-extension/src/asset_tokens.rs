//! Short-lived capability tokens for extension settings-tab assets (E1 tail ④).
//!
//! A settings tab is an iframe whose document and subresources come from the
//! backend. Instead of loading it with the caller's full credentials (session
//! cookie / desktop local secret), the UI asks for a token bound to one user
//! and one extension and loads
//! `/api/extensions/{name}/asset-session/{token}/{path}`. The token only reads
//! that extension's asset tree and expires after [`ASSET_TOKEN_TTL_MS`];
//! relative URLs inside the page inherit it because it is a path segment.

use std::collections::HashMap;
use std::sync::Mutex;

use aionui_common::now_ms;

/// Lifetime of an asset token.
pub const ASSET_TOKEN_TTL_MS: i64 = 10 * 60 * 1000;
/// Upper bound on live tokens; the oldest are dropped first.
const MAX_LIVE_TOKENS: usize = 1024;

#[derive(Debug, Clone)]
struct Grant {
    user_id: String,
    extension_name: String,
    expires_at: i64,
}

#[derive(Debug, Default)]
pub struct AssetTokenStore {
    inner: Mutex<HashMap<String, Grant>>,
}

impl AssetTokenStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Issue a token for `user_id` to read `extension_name`'s assets.
    /// Returns `(token, expires_at_ms)`.
    pub fn issue(&self, user_id: &str, extension_name: &str) -> Result<(String, i64), getrandom::Error> {
        let mut bytes = [0u8; 32];
        getrandom::getrandom(&mut bytes)?;
        let token = hex::encode(bytes);
        let now = now_ms();
        let expires_at = now + ASSET_TOKEN_TTL_MS;
        let mut map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        map.retain(|_, grant| grant.expires_at > now);
        if map.len() >= MAX_LIVE_TOKENS {
            let mut by_expiry: Vec<(String, i64)> = map.iter().map(|(k, g)| (k.clone(), g.expires_at)).collect();
            by_expiry.sort_by_key(|(_, exp)| *exp);
            for (key, _) in by_expiry.into_iter().take(map.len() + 1 - MAX_LIVE_TOKENS) {
                map.remove(&key);
            }
        }
        map.insert(
            token.clone(),
            Grant {
                user_id: user_id.to_owned(),
                extension_name: extension_name.to_owned(),
                expires_at,
            },
        );
        Ok((token, expires_at))
    }

    /// Resolve a token to its user if it is live and bound to `extension_name`.
    pub fn resolve(&self, token: &str, extension_name: &str) -> Option<String> {
        self.resolve_at(token, extension_name, now_ms())
    }

    fn resolve_at(&self, token: &str, extension_name: &str, now: i64) -> Option<String> {
        let map = self.inner.lock().unwrap_or_else(|p| p.into_inner());
        let grant = map.get(token)?;
        (grant.expires_at > now && grant.extension_name == extension_name).then(|| grant.user_id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_bound_to_extension_and_expires() {
        let store = AssetTokenStore::new();
        let (token, expires_at) = store.issue("u1", "hello").unwrap();
        assert_eq!(token.len(), 64);
        assert_eq!(store.resolve(&token, "hello").as_deref(), Some("u1"));
        assert_eq!(store.resolve(&token, "other"), None);
        assert_eq!(store.resolve("nope", "hello"), None);
        assert_eq!(store.resolve_at(&token, "hello", expires_at), None);
    }

    #[test]
    fn live_tokens_are_capped() {
        let store = AssetTokenStore::new();
        for _ in 0..(MAX_LIVE_TOKENS + 10) {
            store.issue("u1", "hello").unwrap();
        }
        assert!(store.inner.lock().unwrap().len() <= MAX_LIVE_TOKENS);
    }
}
