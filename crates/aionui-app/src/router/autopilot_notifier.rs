//! E8-6 Autopilot result delivery (D8-7): IM summary through the existing
//! channel sender, failures also raised in the Inbox (deduped).

use std::sync::Arc;

use aionui_channel::stream_relay::ChannelSender;
use aionui_channel::types::{OutgoingMessageType, UnifiedOutgoingMessage};
use aionui_conversation::ConversationService;
use aionui_cron::notify::{AutopilotNotifier, AutopilotOutcome, AutopilotRunReport};
use aionui_db::{IChannelRepository, UpsertInboxItemParams};
use async_trait::async_trait;

const INBOX_SUMMARY_MAX_CHARS: usize = 500;

/// Inbox dedupe key for failures that are not tied to a conversation.
pub(crate) fn cron_job_inbox_key(job_id: &str) -> String {
    format!("cron:{job_id}")
}

pub(crate) struct ChannelAutopilotNotifier {
    channel_repo: Arc<dyn IChannelRepository>,
    sender: Arc<dyn ChannelSender>,
    conversation: ConversationService,
}

/// Resolved IM destination.
#[derive(Debug, PartialEq, Eq)]
struct ImTarget {
    plugin_id: String,
    chat_id: String,
}

impl ChannelAutopilotNotifier {
    pub(crate) fn new(
        channel_repo: Arc<dyn IChannelRepository>,
        sender: Arc<dyn ChannelSender>,
        conversation: ConversationService,
    ) -> Self {
        Self {
            channel_repo,
            sender,
            conversation,
        }
    }

    /// `notify_channel_id` is an `assistant_sessions.id` owned by the user;
    /// the plugin id is the platform type of the paired channel user.
    async fn resolve_target(&self, user_id: &str, channel_id: &str) -> Result<ImTarget, String> {
        let session = self
            .channel_repo
            .get_session(user_id, channel_id)
            .await
            .map_err(|e| format!("failed to load notify channel: {e}"))?
            .ok_or_else(|| format!("notify channel '{channel_id}' not found"))?;
        let chat_id = session
            .chat_id
            .filter(|chat| !chat.trim().is_empty())
            .ok_or_else(|| format!("notify channel '{channel_id}' has no chat to deliver to"))?;
        let channel_user = self
            .channel_repo
            .get_all_users(user_id)
            .await
            .map_err(|e| format!("failed to load notify channel user: {e}"))?
            .into_iter()
            .find(|user| user.id == session.user_id)
            .ok_or_else(|| format!("notify channel '{channel_id}' has no paired user"))?;
        Ok(ImTarget {
            plugin_id: channel_user.platform_type,
            chat_id,
        })
    }

    async fn deliver_im(&self, report: &AutopilotRunReport, channel_id: &str) {
        let target = match self.resolve_target(&report.user_id, channel_id).await {
            Ok(target) => target,
            Err(error) => {
                tracing::warn!(job_id = %report.job_id, channel_id, %error, "autopilot IM target unavailable");
                return;
            }
        };
        let message = UnifiedOutgoingMessage {
            message_type: OutgoingMessageType::Text,
            text: Some(report.summary_text()),
            parse_mode: None,
            buttons: None,
            keyboard: None,
            image_url: None,
            file_url: None,
            file_name: None,
            media_actions: None,
            reply_to_message_id: None,
            silent: None,
        };
        if let Err(error) = self
            .sender
            .send_message(&report.user_id, &target.plugin_id, &target.chat_id, message)
            .await
        {
            tracing::warn!(job_id = %report.job_id, plugin_id = %target.plugin_id, %error, "autopilot IM delivery failed");
        }
    }

    async fn deliver_inbox(&self, report: &AutopilotRunReport) {
        match report.outcome {
            AutopilotOutcome::Error => {
                // Share the turn-level key when the run had a conversation so
                // one failure yields one Inbox item, not two.
                let dedupe_key = match report.conversation_id.as_deref() {
                    Some(conversation_id) => ConversationService::conversation_turn_inbox_key(conversation_id),
                    None => cron_job_inbox_key(&report.job_id),
                };
                let title = format!("{}{}", aionui_api_types::AUTOPILOT_INBOX_TITLE_PREFIX, report.job_name);
                let summary = report
                    .error
                    .as_deref()
                    .map(|error| error.chars().take(INBOX_SUMMARY_MAX_CHARS).collect::<String>());
                self.conversation
                    .raise_inbox_item(&UpsertInboxItemParams {
                        user_id: &report.user_id,
                        kind: "failed",
                        source_kind: "conversation",
                        source_id: report.conversation_id.as_deref().unwrap_or(&report.job_id),
                        conversation_id: report.conversation_id.as_deref(),
                        task_session_id: None,
                        run_id: None,
                        title: &title,
                        summary: summary.as_deref(),
                        dedupe_key: &dedupe_key,
                        created_at: aionui_common::now_ms(),
                    })
                    .await;
            }
            AutopilotOutcome::Ok => {
                // A later success settles an earlier conversation-less failure.
                self.conversation
                    .resolve_inbox_item(&report.user_id, &cron_job_inbox_key(&report.job_id))
                    .await;
            }
        }
    }
}

#[async_trait]
impl AutopilotNotifier for ChannelAutopilotNotifier {
    async fn validate_target(&self, user_id: &str, channel_id: &str) -> Result<(), String> {
        self.resolve_target(user_id, channel_id).await.map(|_| ())
    }

    async fn notify(&self, report: AutopilotRunReport) {
        if let Some(channel_id) = report.notify_channel_id.as_deref() {
            self.deliver_im(&report, channel_id).await;
        }
        self.deliver_inbox(&report).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use aionui_channel::error::ChannelError;
    use aionui_db::models::{AssistantSessionRow, AssistantUserRow};
    use aionui_db::{IInboxRepository, InboxFilter, InboxItemRow, SqliteChannelRepository, SqliteInboxRepository};

    use super::*;
    use crate::config::AppConfig;
    use crate::services::AppServices;

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
            Ok("msg-1".into())
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
        _tmp: tempfile::TempDir,
        notifier: ChannelAutopilotNotifier,
        sender: Arc<RecordingSender>,
        inbox: SqliteInboxRepository,
        session_id: String,
    }

    async fn fixture() -> Fixture {
        let tmp = tempfile::TempDir::new().unwrap();
        let config = AppConfig {
            data_dir: tmp.path().join("data"),
            work_dir: tmp.path().join("work"),
            ..Default::default()
        };
        let db = aionui_db::init_database_memory().await.unwrap();
        let services = AppServices::from_config(db, &config).await.unwrap();
        let pool = services.database.pool().clone();

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
        let session = channel_repo
            .get_or_create_session(
                USER,
                "ch-user-1",
                "chat-9",
                &AssistantSessionRow {
                    id: "ch-session-1".into(),
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

        let sender = Arc::new(RecordingSender::default());
        let notifier =
            ChannelAutopilotNotifier::new(channel_repo, sender.clone(), services.conversation_service.clone());
        Fixture {
            _tmp: tmp,
            notifier,
            sender,
            inbox: SqliteInboxRepository::new(pool),
            session_id: session.id,
        }
    }

    fn report(outcome: AutopilotOutcome, channel: Option<&str>, conversation: Option<&str>) -> AutopilotRunReport {
        AutopilotRunReport {
            user_id: USER.into(),
            job_id: "cron_1".into(),
            job_name: "Nightly".into(),
            notify_channel_id: channel.map(ToOwned::to_owned),
            outcome,
            conversation_id: conversation.map(ToOwned::to_owned),
            error: (outcome == AutopilotOutcome::Error).then(|| "agent crashed".to_owned()),
        }
    }

    async fn open_items(inbox: &SqliteInboxRepository) -> Vec<InboxItemRow> {
        inbox
            .list(
                USER,
                &InboxFilter {
                    unread_only: true,
                    before: None,
                    limit: 50,
                },
            )
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn validate_target_accepts_owned_session_and_rejects_others() {
        let f = fixture().await;
        f.notifier.validate_target(USER, &f.session_id).await.unwrap();
        assert!(f.notifier.validate_target(USER, "missing").await.is_err());
        assert!(f.notifier.validate_target("other_user", &f.session_id).await.is_err());
    }

    #[tokio::test]
    async fn failure_sends_im_and_raises_one_deduped_inbox_item() {
        let f = fixture().await;
        let failed = report(AutopilotOutcome::Error, Some(&f.session_id), None);
        f.notifier.notify(failed.clone()).await;
        f.notifier.notify(failed).await;

        let sends = f.sender.sends.lock().unwrap().clone();
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].0, USER);
        assert_eq!(sends[0].1, "telegram");
        assert_eq!(sends[0].2, "chat-9");
        assert_eq!(
            sends[0].3.as_deref(),
            Some("\u{274c} Autopilot \"Nightly\" failed: agent crashed")
        );

        let items = open_items(&f.inbox).await;
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].kind, "failed");
        assert_eq!(items[0].dedupe_key, "cron:cron_1");
        assert_eq!(items[0].summary.as_deref(), Some("agent crashed"));

        // A later success settles the conversation-less failure and only
        // goes to IM.
        f.notifier
            .notify(report(AutopilotOutcome::Ok, Some(&f.session_id), Some("conv-1")))
            .await;
        assert!(open_items(&f.inbox).await.is_empty());
        assert_eq!(f.sender.sends.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn failure_with_conversation_shares_the_turn_item_key() {
        let f = fixture().await;
        f.notifier
            .notify(report(AutopilotOutcome::Error, None, Some("conv-1")))
            .await;
        let items = open_items(&f.inbox).await;
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].dedupe_key,
            ConversationService::conversation_turn_inbox_key("conv-1")
        );
        assert_eq!(items[0].conversation_id.as_deref(), Some("conv-1"));
        // No channel configured: Inbox only.
        assert!(f.sender.sends.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unresolvable_channel_does_not_block_inbox() {
        let f = fixture().await;
        f.notifier
            .notify(report(AutopilotOutcome::Error, Some("gone"), None))
            .await;
        assert!(f.sender.sends.lock().unwrap().is_empty());
        assert_eq!(open_items(&f.inbox).await.len(), 1);
    }
}
