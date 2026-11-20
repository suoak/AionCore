//! 3.0 C9: IM side of remote approvals — identity checks, approval cards and
//! button presses, over the channel tables and plugins.

use std::sync::Arc;

use aionui_api_types::{ApprovalRequestKind, ApprovalRequestResponse};
use async_trait::async_trait;

/// An IM identity is trusted only while it is an authorized channel user of
/// the owner (`assistant_users`, i.e. its pairing was approved).
pub(crate) struct ImIdentityAdapter {
    pub(crate) repo: Arc<dyn aionui_db::IChannelRepository>,
}

#[async_trait]
impl aionui_conversation::remote_approval::ImIdentityPort for ImIdentityAdapter {
    async fn is_paired(&self, user_id: &str, platform: &str, identity: &str) -> bool {
        matches!(
            self.repo.get_user_by_platform(user_id, identity, platform).await,
            Ok(Some(_))
        )
    }
}

/// Sends an approval card to each bound IM approver's chat with the bot.
pub(crate) struct ImApprovalNotifier {
    pub(crate) repo: Arc<dyn aionui_db::IChannelRepository>,
    pub(crate) manager: Arc<aionui_channel::manager::ChannelManager>,
    pub(crate) approvers: Arc<dyn aionui_db::IApprovalRequestRepository>,
}

fn card_text(request: &ApprovalRequestResponse, task_title: &str) -> String {
    let what = match request.kind {
        ApprovalRequestKind::Plan => "Approve this plan? It runs as soon as you approve.".to_owned(),
        ApprovalRequestKind::Goal => "Approve this goal?".to_owned(),
        ApprovalRequestKind::Adopt => format!(
            "Adopt this change? It is merged into {} ({}) as soon as you approve.",
            request.target_branch.as_deref().unwrap_or("its branch"),
            request.strategy.as_deref().unwrap_or("squash"),
        ),
    };
    let minutes = ((request.expires_at - request.requested_at) / 60_000).max(1);
    format!(
        "WorkMate · {task_title}\n{what}\nDigest {} · expires in {minutes} min",
        request.digest.chars().take(12).collect::<String>()
    )
}

#[async_trait]
impl aionui_conversation::remote_approval::RemoteApprovalNotifier for ImApprovalNotifier {
    async fn request_issued(&self, user_id: &str, request: &ApprovalRequestResponse, task_title: &str) {
        let Ok(approvers) = self.approvers.list_approvers(user_id).await else {
            return;
        };
        if approvers.is_empty() {
            return;
        }
        let plugins = self.repo.get_all_plugins(user_id).await.unwrap_or_default();
        for approver in approvers {
            let Ok(Some(user)) = self
                .repo
                .get_user_by_platform(user_id, &approver.platform_user_id, &approver.platform_type)
                .await
            else {
                continue;
            };
            let Some(session_id) = user.session_id.as_deref() else {
                continue;
            };
            let Ok(Some(session)) = self.repo.get_session(user_id, session_id).await else {
                continue;
            };
            let Some(chat_id) = session.chat_id.as_deref() else {
                continue;
            };
            let Some(plugin) = plugins.iter().find(|p| p.enabled && p.r#type == approver.platform_type) else {
                continue;
            };
            let message = aionui_channel::types::UnifiedOutgoingMessage {
                message_type: aionui_channel::types::OutgoingMessageType::Buttons,
                text: Some(card_text(request, task_title)),
                parse_mode: None,
                buttons: Some(aionui_channel::remote_approval::approval_buttons(
                    &request.id,
                    &request.digest,
                )),
                keyboard: None,
                image_url: None,
                file_url: None,
                file_name: None,
                media_actions: None,
                reply_to_message_id: None,
                silent: None,
            };
            if let Err(error) = self.manager.send_message(user_id, &plugin.id, chat_id, message).await {
                tracing::warn!(%error, platform = %approver.platform_type, "approval card was not delivered");
            }
        }
    }
}

/// Button presses on approval cards.
pub(crate) struct ChannelApprovalAdapter {
    pub(crate) conversations: aionui_conversation::ConversationService,
}

#[async_trait]
impl aionui_channel::remote_approval::ChannelApprovalPort for ChannelApprovalAdapter {
    async fn decide(
        &self,
        owner_user_id: &str,
        platform: &str,
        identity: &str,
        request_id: &str,
        digest_prefix: &str,
        approve: bool,
    ) -> Result<String, String> {
        match self
            .conversations
            .decide_approval_request_from_im(owner_user_id, platform, identity, request_id, digest_prefix, approve)
            .await
        {
            Ok(request) => Ok(match (approve, request.error.as_deref()) {
                (false, _) => "Rejected.".to_owned(),
                (true, Some(error)) => format!("Approved, but it did not run: {error}"),
                (true, None) => match request.kind {
                    ApprovalRequestKind::Adopt => "Approved and adopted.".to_owned(),
                    ApprovalRequestKind::Plan => "Approved — the plan is running.".to_owned(),
                    ApprovalRequestKind::Goal => "Approved.".to_owned(),
                },
            }),
            Err(error) => Err(format!("Not accepted: {error}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_card_names_what_runs_and_when_it_expires() {
        let request = ApprovalRequestResponse {
            id: "r1".into(),
            task_session_id: "t1".into(),
            kind: ApprovalRequestKind::Adopt,
            status: aionui_api_types::ApprovalRequestStatus::Pending,
            digest: "abcdef0123456789".repeat(4),
            approval_id: None,
            artifact_id: None,
            fanout_group_id: Some("g1".into()),
            slot: Some("A".into()),
            target_branch: Some("main".into()),
            strategy: Some("squash".into()),
            requested_at: 0,
            expires_at: 30 * 60_000,
            decided_at: None,
            decided_by: None,
            reason: None,
            result_ref: None,
            error: None,
        };
        let text = card_text(&request, "Fix login");
        assert!(text.contains("Fix login"), "{text}");
        assert!(text.contains("into main (squash)"), "{text}");
        assert!(text.contains("abcdef012345"), "{text}");
        assert!(text.contains("30 min"), "{text}");
    }
}
