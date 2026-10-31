//! Paired-device WebSocket wiring (M8 E9-2).
//!
//! A phone may open the realtime socket with its `wmd_` device token to
//! receive the owner's live events. It is a receive-only client: its inbound
//! frames never reach the business routers (filesystem / SCM monitors are
//! outside the `mobile_default` scope). Revoking the device closes its
//! sockets at once.

use std::sync::Arc;

use aionui_auth::DeviceRevokedHook;
use aionui_auth::device::token::{DEVICE_INACTIVITY_TTL_MS, hash_device_token, is_device_token};
use aionui_db::{IUserDeviceRepository, IUserRepository, UserType};
use aionui_realtime::{ConnectionId, MessageRouter, WebSocketManager};

use crate::config::IdentityMode;

/// Drops inbound frames from device-token connections; everything else goes
/// to `inner`.
pub(crate) struct DeviceInboundGuard {
    pub inner: Arc<dyn MessageRouter>,
    pub manager: Arc<WebSocketManager>,
}

impl MessageRouter for DeviceInboundGuard {
    fn route(&self, conn_id: ConnectionId, user_id: &str, name: &str, data: serde_json::Value) -> bool {
        if self
            .manager
            .connection_token(conn_id)
            .is_some_and(|token| is_device_token(&token))
        {
            tracing::debug!(%conn_id, message_name = name, "device websocket inbound frame refused");
            return false;
        }
        self.inner.route(conn_id, user_id, name, data)
    }

    fn on_disconnect(&self, conn_id: ConnectionId) {
        self.inner.on_disconnect(conn_id);
    }
}

/// Resolve a device token to its owner for the WebSocket handshake, with the
/// same checks as the HTTP device channel (active, not 90 days idle, owner
/// active, AionPro identity).
pub(crate) async fn resolve_device_ws_user(
    devices: &dyn IUserDeviceRepository,
    users: &dyn IUserRepository,
    identity_mode: IdentityMode,
    token: &str,
) -> Option<String> {
    let device = devices
        .find_active_by_token_hash(&hash_device_token(token))
        .await
        .ok()??;
    if aionui_common::now_ms() - device.last_active_at() >= DEVICE_INACTIVITY_TTL_MS {
        return None;
    }
    let user = users.find_active_by_id(&device.user_id).await.ok()??;
    if identity_mode == IdentityMode::AionPro && user.user_type != UserType::Aionpro {
        return None;
    }
    Some(user.id)
}

/// Close the revoked device's sockets.
pub(crate) fn ws_kick_on_revoke(manager: Arc<WebSocketManager>) -> DeviceRevokedHook {
    Arc::new(move |device| {
        let removed = manager.disconnect_by_token(
            |token| is_device_token(token) && hash_device_token(token) == device.token_hash,
            "device revoked",
        );
        tracing::info!(device_id = %device.id, removed, "revoked device websockets closed");
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    struct Counting(AtomicUsize);
    impl MessageRouter for Counting {
        fn route(&self, _: ConnectionId, _: &str, _: &str, _: serde_json::Value) -> bool {
            self.0.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    #[test]
    fn device_connections_cannot_send_inbound_frames() {
        let manager = Arc::new(WebSocketManager::new());
        let (tx1, _rx1) = mpsc::channel(4);
        let (tx2, _rx2) = mpsc::channel(4);
        let device = manager.add_client_for_user("u".into(), format!("wmd_{}", "a".repeat(64)), tx1);
        let desktop = manager.add_client_for_user("u".into(), "jwt".into(), tx2);
        let inner = Arc::new(Counting(AtomicUsize::new(0)));
        let guard = DeviceInboundGuard {
            inner: inner.clone(),
            manager,
        };
        assert!(!guard.route(device, "u", "fs", serde_json::Value::Null));
        assert!(guard.route(desktop, "u", "fs", serde_json::Value::Null));
        assert_eq!(inner.0.load(Ordering::SeqCst), 1);
    }
}
