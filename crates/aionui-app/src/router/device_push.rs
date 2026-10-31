//! M8 E9-6: push approval / blocked Inbox items to paired devices over IM.
//!
//! A device with `push_kind = im` names one of the owner's IM channel
//! sessions (`push_target`, an `assistant_sessions.id`). When an approval or
//! blocked item is raised, every such device's chat gets one short message:
//! the item type, its title, and a deep link back into the app. No summary
//! or other body text is sent (design §2.4). APNs/FCM are not implemented
//! (D9-3).

use std::collections::BTreeSet;
use std::sync::Arc;

use aionui_auth::DevicePushTargetValidator;
use aionui_auth::device::token::DEVICE_INACTIVITY_TTL_MS;
use aionui_channel::stream_relay::ChannelSender;
use aionui_conversation::InboxPushHook;
use aionui_db::{IChannelRepository, IUserDeviceRepository, InboxItemRow};
use async_trait::async_trait;

use super::autopilot_notifier::{im_text_message, resolve_im_target};

/// Deep-link scheme registered by the desktop and mobile apps.
const DEEP_LINK_SCHEME: &str = "csbu-workmate";
const IM_TITLE_MAX_CHARS: usize = 80;

/// What is pushed for one Inbox item.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DevicePushItem {
    pub user_id: String,
    pub item_id: String,
    pub kind: String,
    pub title: String,
}

impl DevicePushItem {
    fn from_row(row: &InboxItemRow) -> Self {
        Self {
            user_id: row.user_id.clone(),
            item_id: row.id.clone(),
            kind: row.kind.clone(),
            title: row.title.clone(),
        }
    }

    /// Type + title + deep link; never the item summary.
    pub(crate) fn im_text(&self) -> String {
        let label = match self.kind.as_str() {
            "approval" => "Approval needed",
            "blocked" => "Blocked",
            other => other,
        };
        let title: String = self.title.trim().chars().take(IM_TITLE_MAX_CHARS).collect();
        let link = format!("{DEEP_LINK_SCHEME}://inbox/{}", self.item_id);
        if title.is_empty() || title == label {
            format!("CSBU WorkMate · {label}\n{link}")
        } else {
            format!("CSBU WorkMate · {label}: {title}\n{link}")
        }
    }
}

#[derive(Clone)]
pub(crate) struct ChannelDevicePush {
    devices: Arc<dyn IUserDeviceRepository>,
    channel_repo: Arc<dyn IChannelRepository>,
    sender: Arc<dyn ChannelSender>,
}

impl ChannelDevicePush {
    pub(crate) fn new(
        devices: Arc<dyn IUserDeviceRepository>,
        channel_repo: Arc<dyn IChannelRepository>,
        sender: Arc<dyn ChannelSender>,
    ) -> Self {
        Self {
            devices,
            channel_repo,
            sender,
        }
    }

    /// IM channel sessions of the user's active, recently used devices that
    /// asked for IM push. Several devices on one chat get one message.
    async fn im_targets(&self, user_id: &str) -> Vec<String> {
        let rows = match self.devices.list_for_user(user_id).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(user_id, %error, "device push: listing devices failed");
                return Vec::new();
            }
        };
        let now = aionui_common::now_ms();
        rows.into_iter()
            .filter(|d| d.revoked_at.is_none() && d.push_kind == "im")
            .filter(|d| now - d.last_active_at() < DEVICE_INACTIVITY_TTL_MS)
            .filter_map(|d| d.push_target)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Deliver one item. Returns how many IM messages were sent.
    pub(crate) async fn deliver(&self, item: &DevicePushItem) -> usize {
        let mut sent = 0;
        for channel_id in self.im_targets(&item.user_id).await {
            // Re-checked at send time: the session may have been deleted or
            // unpaired since the target was set.
            let target = match resolve_im_target(self.channel_repo.as_ref(), &item.user_id, &channel_id).await {
                Ok(target) => target,
                Err(error) => {
                    tracing::warn!(user_id = %item.user_id, channel_id, %error, "device push: IM target unavailable");
                    continue;
                }
            };
            match self
                .sender
                .send_message(
                    &item.user_id,
                    &target.plugin_id,
                    &target.chat_id,
                    im_text_message(item.im_text()),
                )
                .await
            {
                Ok(_) => sent += 1,
                Err(error) => {
                    tracing::warn!(user_id = %item.user_id, plugin_id = %target.plugin_id, %error, "device push: IM delivery failed");
                }
            }
        }
        sent
    }
}

impl InboxPushHook for ChannelDevicePush {
    fn item_raised(&self, item: &InboxItemRow) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let this = self.clone();
        let item = DevicePushItem::from_row(item);
        runtime.spawn(async move {
            this.deliver(&item).await;
        });
    }
}

/// `PUT /api/auth/devices/{id}/push` accepts only the user's own, deliverable
/// IM sessions.
pub(crate) struct ChannelPushTargetValidator {
    channel_repo: Arc<dyn IChannelRepository>,
}

impl ChannelPushTargetValidator {
    pub(crate) fn new(channel_repo: Arc<dyn IChannelRepository>) -> Self {
        Self { channel_repo }
    }
}

#[async_trait]
impl DevicePushTargetValidator for ChannelPushTargetValidator {
    async fn validate_im_target(&self, user_id: &str, target: &str) -> Result<(), String> {
        resolve_im_target(self.channel_repo.as_ref(), user_id, target)
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use aionui_channel::error::ChannelError;
    use aionui_channel::types::UnifiedOutgoingMessage;
    use aionui_db::models::{AssistantSessionRow, AssistantUserRow};
    use aionui_db::{CreateUserDeviceParams, SqliteChannelRepository, SqliteUserDeviceRepository};

    use super::*;

    const USER: &str = "system_default_user";

    #[derive(Default)]
    struct RecordingSender {
        sends: Mutex<Vec<(String, String, String, Option<String>)>>,
    }

    #[async_trait]
    impl ChannelSender for RecordingSender {
        async fn send_message(
            &self,
            owner_user_id: &str,
            plugin_id: &str,
            chat_id: &str,
            message: UnifiedOutgoingMessage,
        ) -> Result<String, ChannelError> {
            self.sends.lock().unwrap().push((
                owner_user_id.to_owned(),
                plugin_id.to_owned(),
                chat_id.to_owned(),
                message.text,
            ));
            Ok("m".into())
        }

        async fn edit_message(
            &self,
            _owner_user_id: &str,
            _plugin_id: &str,
            _chat_id: &str,
            _message_id: &str,
            _message: UnifiedOutgoingMessage,
        ) -> Result<(), ChannelError> {
            Ok(())
        }
    }

    struct Fixture {
        _db: aionui_db::Database,
        push: ChannelDevicePush,
        validator: ChannelPushTargetValidator,
        devices: Arc<SqliteUserDeviceRepository>,
        sender: Arc<RecordingSender>,
    }

    async fn fixture() -> Fixture {
        let db = aionui_db::init_database_memory().await.unwrap();
        let pool = db.pool().clone();
        let channel_repo = Arc::new(SqliteChannelRepository::new(pool.clone()));
        channel_repo
            .create_user(
                USER,
                &AssistantUserRow {
                    id: "ch-user-1".into(),
                    owner_user_id: USER.into(),
                    platform_user_id: "tg-42".into(),
                    platform_type: "telegram".into(),
                    display_name: None,
                    authorized_at: 1,
                    last_active: None,
                    session_id: None,
                },
            )
            .await
            .unwrap();
        channel_repo
            .get_or_create_session(
                USER,
                "ch-user-1",
                "chat-9",
                &AssistantSessionRow {
                    id: "sess-1".into(),
                    user_id: "ch-user-1".into(),
                    agent_type: "aionrs".into(),
                    conversation_id: None,
                    workspace: None,
                    chat_id: Some("chat-9".into()),
                    created_at: 1,
                    last_activity: 1,
                },
            )
            .await
            .unwrap();
        let devices = Arc::new(SqliteUserDeviceRepository::new(pool));
        let sender = Arc::new(RecordingSender::default());
        Fixture {
            _db: db,
            push: ChannelDevicePush::new(devices.clone(), channel_repo.clone(), sender.clone()),
            validator: ChannelPushTargetValidator::new(channel_repo),
            devices,
            sender,
        }
    }

    async fn device(f: &Fixture, hash: &str, created_at: i64) -> String {
        f.devices
            .create_within_limit(
                &CreateUserDeviceParams {
                    user_id: USER,
                    name: "phone",
                    platform: "ios",
                    token_hash: hash,
                    scopes: r#"["mobile_default"]"#,
                    created_at,
                },
                5,
            )
            .await
            .unwrap()
            .unwrap()
            .id
    }

    fn item(kind: &str, title: &str) -> DevicePushItem {
        DevicePushItem {
            user_id: USER.into(),
            item_id: "inbox_1".into(),
            kind: kind.into(),
            title: title.into(),
        }
    }

    #[test]
    fn im_text_is_type_title_and_deep_link_only() {
        assert_eq!(
            item("approval", "Approval needed").im_text(),
            "CSBU WorkMate · Approval needed\ncsbu-workmate://inbox/inbox_1"
        );
        assert_eq!(
            item("blocked", &"x".repeat(200)).im_text(),
            format!(
                "CSBU WorkMate · Blocked: {}\ncsbu-workmate://inbox/inbox_1",
                "x".repeat(80)
            )
        );
    }

    #[tokio::test]
    async fn only_im_devices_get_one_message_per_chat() {
        let f = fixture().await;
        let now = aionui_common::now_ms();
        let a = device(&f, "ha", now).await;
        let b = device(&f, "hb", now).await;
        let _none = device(&f, "hc", now).await;
        let revoked = device(&f, "hd", now).await;
        let idle = device(&f, "he", now - DEVICE_INACTIVITY_TTL_MS - 1).await;
        for id in [&a, &b, &revoked, &idle] {
            assert!(f.devices.set_push(USER, id, "im", Some("sess-1")).await.unwrap());
        }
        assert!(f.devices.revoke(USER, &revoked, now, "user").await.unwrap());

        assert_eq!(f.push.deliver(&item("approval", "Approval needed")).await, 1);
        let sends = f.sender.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 1);
        assert_eq!(
            (sends[0].0.as_str(), sends[0].1.as_str(), sends[0].2.as_str()),
            (USER, "telegram", "chat-9")
        );
        assert!(
            sends[0]
                .3
                .as_deref()
                .unwrap()
                .ends_with("csbu-workmate://inbox/inbox_1")
        );
    }

    #[tokio::test]
    async fn stale_target_is_skipped_and_other_users_get_nothing() {
        let f = fixture().await;
        let now = aionui_common::now_ms();
        let a = device(&f, "ha", now).await;
        assert!(f.devices.set_push(USER, &a, "im", Some("gone")).await.unwrap());
        assert_eq!(f.push.deliver(&item("blocked", "Fix build")).await, 0);

        let mut other = item("approval", "Approval needed");
        other.user_id = "u2".into();
        assert_eq!(f.push.deliver(&other).await, 0);
        assert!(f.sender.sends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn validator_accepts_only_own_sessions() {
        let f = fixture().await;
        f.validator.validate_im_target(USER, "sess-1").await.unwrap();
        assert!(f.validator.validate_im_target(USER, "missing").await.is_err());
        assert!(f.validator.validate_im_target("u2", "sess-1").await.is_err());
    }
}
