use std::fmt::Write as _;

use sha2::{Digest, Sha256};

/// Prefix that tells a device token apart from a session JWT in
/// `Authorization: Bearer`.
pub const DEVICE_TOKEN_PREFIX: &str = "wmd_";
/// Random bytes behind a device token.
const DEVICE_TOKEN_BYTES: usize = 32;
/// The only scope minted in M8.
pub const MOBILE_DEFAULT_SCOPE: &str = "mobile_default";
/// Unrevoked devices per user (D9-6).
pub const MAX_DEVICES_PER_USER: i64 = 5;
/// Device tokens do not expire, but one unused for this long stops working
/// and is revoked as `inactivity` on its next use (D9-2).
pub const DEVICE_INACTIVITY_TTL_MS: i64 = 90 * 24 * 60 * 60 * 1000;
/// Minimum gap between `last_seen_at` writes for one device.
pub(crate) const LAST_SEEN_THROTTLE_MS: i64 = 60 * 1000;

/// Marker inserted into request extensions when the caller authenticated with
/// a paired-device token (alongside the owner's `CurrentUser`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevicePrincipal {
    pub device_id: String,
    pub scopes: Vec<String>,
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Mint a new opaque device token. Returned to the device once; only its
/// [`hash_device_token`] is stored.
pub fn generate_device_token() -> String {
    let mut buf = [0u8; DEVICE_TOKEN_BYTES];
    getrandom::getrandom(&mut buf).expect("OS entropy source unavailable");
    format!("{DEVICE_TOKEN_PREFIX}{}", hex(&buf))
}

/// Lowercase hex SHA-256 of a device token.
pub fn hash_device_token(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

/// Whether a bearer credential is shaped like a device token.
pub fn is_device_token(token: &str) -> bool {
    token.starts_with(DEVICE_TOKEN_PREFIX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_prefixed_random_and_hash_stably() {
        let a = generate_device_token();
        let b = generate_device_token();
        assert_ne!(a, b);
        assert!(is_device_token(&a));
        assert_eq!(a.len(), DEVICE_TOKEN_PREFIX.len() + 64);
        let hash = hash_device_token(&a);
        assert_eq!(hash.len(), 64);
        assert_eq!(hash, hash_device_token(&a));
        assert_ne!(hash, hash_device_token(&b));
        assert!(!hash.contains(&a[DEVICE_TOKEN_PREFIX.len()..]));
        assert!(!is_device_token("eyJhbGciOiJIUzI1NiJ9.x.y"));
    }
}
