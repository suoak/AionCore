use aionui_common::TimestampMs;

use crate::error::DbError;
use crate::models::UserDeviceRow;

/// Insert payload. `id` is minted by the repository.
#[derive(Debug, Clone)]
pub struct CreateUserDeviceParams<'a> {
    pub user_id: &'a str,
    pub name: &'a str,
    /// `ios` | `android` | `web`.
    pub platform: &'a str,
    /// Lowercase hex SHA-256 of the device token (never the token itself).
    pub token_hash: &'a str,
    /// JSON array of scope names.
    pub scopes: &'a str,
    pub created_at: TimestampMs,
}

#[async_trait::async_trait]
pub trait IUserDeviceRepository: Send + Sync {
    /// Insert a device unless the user already has `max_active` unrevoked
    /// devices. The count and the insert are one statement, so concurrent
    /// pairings cannot overshoot the cap. `Ok(None)` means the cap was hit.
    async fn create_within_limit(
        &self,
        params: &CreateUserDeviceParams<'_>,
        max_active: i64,
    ) -> Result<Option<UserDeviceRow>, DbError>;

    /// Unrevoked device owning `token_hash`.
    async fn find_active_by_token_hash(&self, token_hash: &str) -> Result<Option<UserDeviceRow>, DbError>;

    /// Device by id, scoped to its owner (revoked ones included).
    async fn find_for_user(&self, user_id: &str, id: &str) -> Result<Option<UserDeviceRow>, DbError>;

    /// The user's devices, newest first (revoked ones included).
    async fn list_for_user(&self, user_id: &str) -> Result<Vec<UserDeviceRow>, DbError>;

    async fn count_active(&self, user_id: &str) -> Result<i64, DbError>;

    /// Record activity. No-op for revoked devices.
    async fn touch_last_seen(&self, id: &str, at: TimestampMs) -> Result<(), DbError>;

    /// Revoke an unrevoked device of `user_id`. `false` when it does not exist,
    /// belongs to someone else, or is already revoked.
    async fn revoke(&self, user_id: &str, id: &str, at: TimestampMs, revoked_by: &str) -> Result<bool, DbError>;
}
