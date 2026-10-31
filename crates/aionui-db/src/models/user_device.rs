use aionui_common::TimestampMs;

/// One paired device (M8 E9-1). Deliberately not `Serialize`: `token_hash`
/// must not leak through an API response by accident.
#[derive(Debug, Clone, sqlx::FromRow, PartialEq)]
pub struct UserDeviceRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    /// `ios` | `android` | `web`.
    pub platform: String,
    /// Lowercase hex SHA-256 of the opaque device token.
    pub token_hash: String,
    /// JSON array of scope names.
    pub scopes: String,
    /// `none` | `im` | `apns` | `fcm`.
    pub push_kind: String,
    pub push_target: Option<String>,
    pub last_seen_at: Option<TimestampMs>,
    pub created_at: TimestampMs,
    pub revoked_at: Option<TimestampMs>,
    pub revoked_by: Option<String>,
}

impl UserDeviceRow {
    /// Last activity, falling back to the pairing time.
    pub fn last_active_at(&self) -> TimestampMs {
        self.last_seen_at.unwrap_or(self.created_at)
    }
}
