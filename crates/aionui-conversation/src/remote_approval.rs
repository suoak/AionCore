//! 3.0 C9 (plan B §5.4, D8): remote approve-to-execute.
//!
//! A Plan/Goal approval or an adopt can be answered from a paired phone or a
//! bound IM identity, and an approval executes at once (no desktop
//! re-confirmation). The rules, each refused with an audit event:
//!
//! - **Bound**: every request stores the digest the approver must echo — the
//!   plan artifact's sha256 or the adopt diff digest (base, branch head,
//!   target and its head, strategy, normalized worktree diff). Core
//!   recomputes it before acting and again inside the adopt; a changed plan,
//!   new commits or a moved target all reject (`diff_changed`).
//! - **One-shot**: `pending → approved|rejected` is a conditional update; a
//!   repeated, replayed or concurrent second decision is 409 and runs nothing.
//! - **Expiring**: 30 minutes by default (setting, 1 minute … 24 hours).
//! - **Trusted identity**: an active paired device, or an IM identity the
//!   user bound as an approver that is still paired in the channel; and the
//!   user must pass the E12 launch check for the task.
//! - **Nothing else changes**: the decision carries only approve/reject and
//!   the digest. Plan content, agent, permission mode, strategy and target
//!   come from the request.

use std::sync::{Arc, RwLock};

use aionui_api_types::{
    ApprovalRequestKind, ApprovalRequestResponse, ApprovalRequestStatus, DEFAULT_APPROVAL_TTL_MINUTES,
    IssueApprovalRequest, MAX_APPROVAL_TTL_MINUTES, MIN_IM_DIGEST_PREFIX, RemoteApprovalDecision,
    RemoteApprovalDecisionRequest, RemoteApprovalSettings, RemoteApproverRequest, RemoteApproverResponse,
    WebSocketMessage,
};
use aionui_common::now_ms;
use aionui_db::models::TaskApprovalRow;
use aionui_db::{
    ApprovalRequestRow, AuditActorKind, AuditSink, AuditVia, IApprovalRequestRepository, NewApprovalRequest,
    NewAuditEvent, ResolveTaskApprovalParams,
};
use async_trait::async_trait;

use crate::error::ConversationError;
use crate::inbox::TaskApprovalInvalidation;
use crate::service::ConversationService;

/// WebSocket event: a remote approval request was issued or changed state.
pub(crate) const EVENT_APPROVAL_REQUEST_CHANGED: &str = "approval.request.changed";

/// Who answered remotely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteDecider {
    /// A paired device (already authenticated and unrevoked by the
    /// device-token middleware).
    Device(String),
    /// An IM identity, as reported by the channel for the button press.
    Im { platform: String, identity: String },
}

impl RemoteDecider {
    pub fn label(&self) -> String {
        match self {
            Self::Device(id) => format!("device:{id}"),
            Self::Im { platform, identity } => format!("im:{platform}:{identity}"),
        }
    }

    fn actor_kind(&self) -> AuditActorKind {
        match self {
            Self::Device(_) => AuditActorKind::Device,
            Self::Im { .. } => AuditActorKind::User,
        }
    }
}

/// What the approver echoed: a phone sends the full digest, an IM button a
/// prefix of at least [`MIN_IM_DIGEST_PREFIX`] hex characters (button
/// payloads are small). Either way Core compares against its stored digest
/// and recomputes the current one.
enum DigestEcho<'a> {
    Full(&'a str),
    Prefix(&'a str),
}

impl DigestEcho<'_> {
    fn matches(&self, stored: &str) -> bool {
        match self {
            Self::Full(value) => constant_time_eq(value.as_bytes(), stored.as_bytes()),
            Self::Prefix(value) => {
                value.len() >= MIN_IM_DIGEST_PREFIX
                    && value.len() <= stored.len()
                    && value.bytes().all(|b| b.is_ascii_hexdigit())
                    && constant_time_eq(value.as_bytes(), &stored.as_bytes()[..value.len()])
            }
        }
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The adopt digest and where it points.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdoptDigestInfo {
    pub digest: String,
    pub target_branch: Option<String>,
}

/// Implemented by the app over the fan-out service.
#[async_trait]
pub trait AdoptApprovalPort: Send + Sync {
    async fn digest(
        &self,
        user_id: &str,
        group_id: &str,
        slot: &str,
        strategy: &str,
        target_branch: Option<&str>,
    ) -> Result<AdoptDigestInfo, ConversationError>;

    /// Adopt only if the digest still matches (checked under the group lock).
    /// Returns the result commit (`None` for branch-only).
    async fn adopt(
        &self,
        user_id: &str,
        group_id: &str,
        slot: &str,
        strategy: &str,
        target_branch: Option<&str>,
        expected_digest: &str,
    ) -> Result<Option<String>, ConversationError>;
}

/// Sends an issued request to the user's bound IM identities (best effort).
#[async_trait]
pub trait RemoteApprovalNotifier: Send + Sync {
    async fn request_issued(&self, user_id: &str, request: &ApprovalRequestResponse, task_title: &str);
}

/// Whether an IM identity is still paired in the channel for this user.
#[async_trait]
pub trait ImIdentityPort: Send + Sync {
    async fn is_paired(&self, user_id: &str, platform: &str, identity: &str) -> bool;
}

#[derive(Default)]
pub struct RemoteApprovalSlots {
    repo: RwLock<Option<Arc<dyn IApprovalRequestRepository>>>,
    adopt: RwLock<Option<Arc<dyn AdoptApprovalPort>>>,
    notifier: RwLock<Option<Arc<dyn RemoteApprovalNotifier>>>,
    identities: RwLock<Option<Arc<dyn ImIdentityPort>>>,
    audit: RwLock<Option<AuditSink>>,
}

fn read<T: Clone>(slot: &RwLock<Option<T>>) -> Option<T> {
    slot.read().ok().and_then(|guard| guard.clone())
}

fn write<T>(slot: &RwLock<Option<T>>, value: T) {
    if let Ok(mut guard) = slot.write() {
        *guard = Some(value);
    }
}

fn conflict(code: &str, reason: impl std::fmt::Display) -> ConversationError {
    ConversationError::Busy {
        reason: format!("{code}: {reason}"),
    }
}

fn forbidden(code: &str, reason: impl std::fmt::Display) -> ConversationError {
    ConversationError::Forbidden {
        reason: format!("{code}: {reason}"),
    }
}

pub(crate) fn request_response(row: ApprovalRequestRow) -> Result<ApprovalRequestResponse, ConversationError> {
    Ok(ApprovalRequestResponse {
        kind: ApprovalRequestKind::parse(&row.kind)
            .ok_or_else(|| ConversationError::internal(format!("unknown approval request kind {}", row.kind)))?,
        status: ApprovalRequestStatus::parse(&row.status)
            .ok_or_else(|| ConversationError::internal(format!("unknown approval request status {}", row.status)))?,
        id: row.id,
        task_session_id: row.task_session_id,
        digest: row.digest,
        approval_id: row.approval_id,
        artifact_id: row.artifact_id,
        fanout_group_id: row.fanout_group_id,
        slot: row.slot,
        target_branch: row.target_branch,
        strategy: row.strategy,
        requested_at: row.requested_at,
        expires_at: row.expires_at,
        decided_at: row.decided_at,
        decided_by: row.decided_by,
        reason: row.reason,
        result_ref: row.result_ref,
        error: row.error,
    })
}

/// 16 random hex characters (64 bits): short enough for a Telegram button
/// payload next to the digest prefix, and never the only check.
fn new_request_id() -> String {
    let u = uuid::Uuid::new_v4().simple().to_string();
    // Skip the version (12) and variant (16) nibbles.
    format!("{}{}", &u[..12], &u[20..24])
}

const ADOPT_STRATEGIES: &[&str] = &["squash", "no_ff", "cherry_pick", "branch_only"];

impl ConversationService {
    pub fn with_approval_request_repo(&self, repo: Arc<dyn IApprovalRequestRepository>) {
        write(&self.remote_approval.repo, repo);
    }

    pub fn with_adopt_approval_port(&self, port: Arc<dyn AdoptApprovalPort>) {
        write(&self.remote_approval.adopt, port);
    }

    pub fn with_remote_approval_notifier(&self, notifier: Arc<dyn RemoteApprovalNotifier>) {
        write(&self.remote_approval.notifier, notifier);
    }

    pub fn with_im_identity_port(&self, port: Arc<dyn ImIdentityPort>) {
        write(&self.remote_approval.identities, port);
    }

    pub fn with_remote_approval_audit(&self, audit: AuditSink) {
        write(&self.remote_approval.audit, audit);
    }

    fn approval_request_repo_optional(&self) -> Option<Arc<dyn IApprovalRequestRepository>> {
        read(&self.remote_approval.repo)
    }

    fn approval_request_repo(&self) -> Result<Arc<dyn IApprovalRequestRepository>, ConversationError> {
        self.approval_request_repo_optional()
            .ok_or_else(|| ConversationError::internal("Remote approval is not configured"))
    }

    /// Append one `remote_approval.*` event. A failed write is logged: the
    /// decision itself is already atomic in the requests table.
    async fn audit_remote(
        &self,
        user_id: &str,
        action: &str,
        actor: Option<&RemoteDecider>,
        row: Option<&ApprovalRequestRow>,
        request_id: &str,
        detail: serde_json::Value,
    ) {
        let Some(audit) = read(&self.remote_approval.audit) else {
            return;
        };
        let (kind, actor_id, via) = match actor {
            Some(decider) => (decider.actor_kind(), Some(decider.label()), AuditVia::Mobile),
            None => (AuditActorKind::System, None, AuditVia::System),
        };
        let mut event = NewAuditEvent::new(format!("remote_approval.{action}"), kind, via)
            .with_user(user_id)
            .with_target("approval_request", request_id)
            .with_approval(request_id, actor.map(RemoteDecider::label));
        if let Some(actor_id) = actor_id {
            event = event.with_actor(actor_id);
        }
        let mut diff = serde_json::json!({ "detail": detail });
        if let Some(row) = row {
            event = event
                .with_request_fingerprint(row.digest.clone())
                .with_summary(format!("{} approval request on task {}", row.kind, row.task_session_id));
            diff["task_session_id"] = row.task_session_id.clone().into();
            diff["kind"] = row.kind.clone().into();
            diff["digest"] = row.digest.clone().into();
            diff["status"] = row.status.clone().into();
            diff["decided_by"] = row.decided_by.clone().into();
            diff["result_ref"] = row.result_ref.clone().into();
        }
        audit.record(event.with_diff(diff)).await;
    }

    fn broadcast_approval_request(&self, user_id: &str, row: &ApprovalRequestRow) {
        self.broadcaster().broadcast(WebSocketMessage::new(
            EVENT_APPROVAL_REQUEST_CHANGED,
            serde_json::json!({
                "user_id": user_id,
                "request_id": row.id,
                "task_session_id": row.task_session_id,
                "kind": row.kind,
                "status": row.status,
                "approval_id": row.approval_id,
                "expires_at": row.expires_at,
                "decided_by": row.decided_by,
                "reason": row.reason,
                "result_ref": row.result_ref,
                "error": row.error,
            }),
        ));
    }

    async fn approval_ttl_ms(&self, user_id: &str, repo: &Arc<dyn IApprovalRequestRepository>) -> i64 {
        let minutes = repo
            .ttl_minutes(user_id)
            .await
            .ok()
            .flatten()
            .unwrap_or(DEFAULT_APPROVAL_TTL_MINUTES)
            .clamp(1, MAX_APPROVAL_TTL_MINUTES);
        minutes * 60_000
    }

    async fn store_request(
        &self,
        repo: &Arc<dyn IApprovalRequestRepository>,
        new: NewApprovalRequest<'_>,
        task_title: &str,
    ) -> Result<ApprovalRequestRow, ConversationError> {
        let superseded = repo.create(&new).await?;
        for old in &superseded {
            self.broadcast_approval_request(new.user_id, old);
            self.audit_remote(new.user_id, "invalidate", None, Some(old), &old.id, "superseded".into())
                .await;
        }
        let row = repo
            .get(new.user_id, new.id)
            .await?
            .ok_or_else(|| ConversationError::internal("approval request vanished"))?;
        self.broadcast_approval_request(new.user_id, &row);
        self.audit_remote(
            new.user_id,
            "request",
            None,
            Some(&row),
            &row.id,
            serde_json::Value::Null,
        )
        .await;
        if let Some(notifier) = read(&self.remote_approval.notifier)
            && let Ok(response) = request_response(row.clone())
        {
            notifier.request_issued(new.user_id, &response, task_title).await;
        }
        Ok(row)
    }

    /// A Plan/Goal approval became pending: bind a remote request to it.
    /// Best effort — without one the approval is desktop-only.
    pub(crate) async fn issue_plan_approval_request(
        &self,
        user_id: &str,
        task_title: &str,
        approval: &TaskApprovalRow,
    ) {
        if let Err(error) = self.issue_for_approval(user_id, task_title, approval).await {
            tracing::warn!(%error, approval_id = %approval.id, "issuing the remote approval request failed");
        }
    }

    async fn issue_for_approval(
        &self,
        user_id: &str,
        task_title: &str,
        approval: &TaskApprovalRow,
    ) -> Result<ApprovalRequestRow, ConversationError> {
        let repo = self.approval_request_repo()?;
        let now = now_ms();
        let id = new_request_id();
        let ttl = self.approval_ttl_ms(user_id, &repo).await;
        self.store_request(
            &repo,
            NewApprovalRequest {
                id: &id,
                user_id,
                task_session_id: &approval.task_session_id,
                kind: &approval.approval_type,
                approval_id: Some(&approval.id),
                artifact_id: Some(&approval.artifact_id),
                artifact_hash: Some(&approval.artifact_hash),
                digest: &approval.artifact_hash,
                requested_at: now,
                expires_at: now + ttl,
                ..Default::default()
            },
            task_title,
        )
        .await
    }

    /// Desktop: issue (or re-issue after expiry) a remote request.
    pub async fn issue_approval_request(
        &self,
        user_id: &str,
        task_session_id: &str,
        request: IssueApprovalRequest,
    ) -> Result<ApprovalRequestResponse, ConversationError> {
        let task = self
            .task_session_repo()?
            .get(user_id, task_session_id)
            .await?
            .ok_or_else(|| {
                ConversationError::not_found_reason(format!("Task session '{task_session_id}' not found"))
            })?;
        let row = match request.kind {
            ApprovalRequestKind::Plan | ApprovalRequestKind::Goal => {
                if request.strategy.is_some() || request.target_branch.is_some() {
                    return Err(ConversationError::bad_request(
                        "strategy and target_branch apply to adopt requests only",
                    ));
                }
                let kind = request.kind.as_str();
                let approval = self
                    .task_session_repo()?
                    .list_approvals(user_id, task_session_id)
                    .await?
                    .into_iter()
                    .find(|row| row.status == "pending" && row.approval_type == kind)
                    .ok_or_else(|| conflict("approval_not_pending", format!("no pending {kind} approval")))?;
                self.issue_for_approval(user_id, &task.title, &approval).await?
            }
            ApprovalRequestKind::Adopt => {
                let strategy = request.strategy.as_deref().unwrap_or("squash");
                if !ADOPT_STRATEGIES.contains(&strategy) {
                    return Err(ConversationError::bad_request(format!(
                        "unknown adopt strategy '{strategy}'"
                    )));
                }
                // 3.0 C8: review-only tasks are never adopted.
                self.ensure_task_deliverable(user_id, task_session_id).await?;
                let flow = self
                    .task_board_repo()?
                    .get_task_flow(user_id, task_session_id)
                    .await?
                    .ok_or_else(|| {
                        ConversationError::not_found_reason(format!("Task session '{task_session_id}' not found"))
                    })?;
                let group_id = flow
                    .fanout_group_id
                    .clone()
                    .ok_or_else(|| conflict("adopt_unavailable", "the task has no worktree to adopt"))?;
                let item = self.board_item(user_id, task_session_id).await?;
                if item.board_status != "in_review" {
                    return Err(conflict(
                        "adopt_unavailable",
                        "only a task waiting for review can be adopted",
                    ));
                }
                let port = read(&self.remote_approval.adopt)
                    .ok_or_else(|| ConversationError::internal("Remote adopt is not configured"))?;
                let slot = "A";
                let info = port
                    .digest(user_id, &group_id, slot, strategy, request.target_branch.as_deref())
                    .await?;
                let repo = self.approval_request_repo()?;
                let now = now_ms();
                let id = new_request_id();
                let ttl = self.approval_ttl_ms(user_id, &repo).await;
                self.store_request(
                    &repo,
                    NewApprovalRequest {
                        id: &id,
                        user_id,
                        task_session_id,
                        kind: "adopt",
                        fanout_group_id: Some(&group_id),
                        slot: Some(slot),
                        diff_digest: Some(&info.digest),
                        target_branch: info.target_branch.as_deref(),
                        strategy: Some(strategy),
                        digest: &info.digest,
                        requested_at: now,
                        expires_at: now + ttl,
                        ..Default::default()
                    },
                    &task.title,
                )
                .await?
            }
        };
        request_response(row)
    }

    pub async fn list_approval_requests(
        &self,
        user_id: &str,
        task_session_id: Option<&str>,
    ) -> Result<Vec<ApprovalRequestResponse>, ConversationError> {
        let repo = self.approval_request_repo()?;
        let rows = match task_session_id {
            Some(task) => repo.list_for_task(user_id, task).await?,
            None => repo.list_pending(user_id).await?,
        };
        rows.into_iter().map(request_response).collect()
    }

    pub async fn get_approval_request(
        &self,
        user_id: &str,
        id: &str,
    ) -> Result<ApprovalRequestResponse, ConversationError> {
        let row = self
            .approval_request_repo()?
            .get(user_id, id)
            .await?
            .ok_or_else(|| ConversationError::not_found_reason(format!("Approval request '{id}' not found")))?;
        request_response(row)
    }

    /// A paired device answers `POST /api/approval-requests/{id}/decision`.
    pub async fn decide_approval_request_remotely(
        &self,
        user_id: &str,
        request_id: &str,
        request: RemoteApprovalDecisionRequest,
        decider: RemoteDecider,
    ) -> Result<ApprovalRequestResponse, ConversationError> {
        let approve = request.decision == RemoteApprovalDecision::Approve;
        self.decide_remote(
            user_id,
            request_id,
            DigestEcho::Full(&request.digest),
            approve,
            request.comment.as_deref(),
            &decider,
        )
        .await
    }

    /// The pre-C9 phone route (`/api/task-sessions/{id}/approvals/{aid}/decision`
    /// with a device token) runs under the same rules: it must match the
    /// approval's pending request, with the artifact hash as the digest.
    pub(crate) async fn decide_task_approval_from_device(
        &self,
        user_id: &str,
        task_session_id: &str,
        approval_id: &str,
        request: aionui_api_types::DecideTaskApprovalRequest,
        device_id: &str,
    ) -> Result<aionui_api_types::TaskApprovalResponse, ConversationError> {
        let decider = RemoteDecider::Device(device_id.to_owned());
        let row = self
            .approval_request_repo()?
            .find_for_approval(user_id, approval_id)
            .await?
            .filter(|row| row.task_session_id == task_session_id)
            .ok_or_else(|| ConversationError::not_found_reason(format!("Approval '{approval_id}' not found")))?;
        if row.artifact_id.as_deref() != Some(request.artifact_id.as_str()) {
            self.audit_remote(
                user_id,
                "digest_mismatch",
                Some(&decider),
                Some(&row),
                &row.id,
                "artifact_id".into(),
            )
            .await;
            return Err(conflict(
                "approval_digest_mismatch",
                "the approval is for another artifact",
            ));
        }
        let approve = request.decision == aionui_api_types::TaskApprovalDecision::Approve;
        self.decide_remote(
            user_id,
            &row.id,
            DigestEcho::Full(&request.artifact_hash),
            approve,
            request.comment.as_deref(),
            &decider,
        )
        .await?;
        let approval = self
            .task_session_repo()?
            .list_approvals(user_id, task_session_id)
            .await?
            .into_iter()
            .find(|a| a.id == approval_id)
            .ok_or_else(|| ConversationError::not_found_reason(format!("Approval '{approval_id}' not found")))?;
        crate::service::task_approval_response(approval)
    }

    /// An IM button press. `owner_user_id` is the channel owner; `identity`
    /// the platform user who pressed it (not the message author).
    pub async fn decide_approval_request_from_im(
        &self,
        owner_user_id: &str,
        platform: &str,
        identity: &str,
        request_id: &str,
        digest_prefix: &str,
        approve: bool,
    ) -> Result<ApprovalRequestResponse, ConversationError> {
        let decider = RemoteDecider::Im {
            platform: platform.to_owned(),
            identity: identity.to_owned(),
        };
        self.decide_remote(
            owner_user_id,
            request_id,
            DigestEcho::Prefix(digest_prefix),
            approve,
            None,
            &decider,
        )
        .await
    }

    async fn check_identity(&self, user_id: &str, decider: &RemoteDecider) -> Result<(), ConversationError> {
        match decider {
            // The device-token middleware already refused unknown, revoked
            // and inactive devices and bound the request to their owner.
            RemoteDecider::Device(id) if !id.trim().is_empty() => Ok(()),
            RemoteDecider::Device(_) => Err(forbidden("approval_identity_untrusted", "unknown device")),
            RemoteDecider::Im { platform, identity } => {
                let repo = self.approval_request_repo()?;
                if !repo.is_approver(user_id, platform, identity).await? {
                    return Err(forbidden(
                        "approval_identity_untrusted",
                        "this IM account is not bound as an approver",
                    ));
                }
                let paired = match read(&self.remote_approval.identities) {
                    Some(port) => port.is_paired(user_id, platform, identity).await,
                    None => false,
                };
                if !paired {
                    return Err(forbidden(
                        "approval_identity_untrusted",
                        "this IM account is no longer paired",
                    ));
                }
                Ok(())
            }
        }
    }

    async fn decide_remote(
        &self,
        user_id: &str,
        request_id: &str,
        echo: DigestEcho<'_>,
        approve: bool,
        comment: Option<&str>,
        decider: &RemoteDecider,
    ) -> Result<ApprovalRequestResponse, ConversationError> {
        if comment.is_some_and(|c| c.chars().count() > 2_000) {
            return Err(ConversationError::bad_request(
                "Approval comment must contain at most 2000 characters",
            ));
        }
        let repo = self.approval_request_repo()?;
        let Some(row) = repo.get(user_id, request_id).await? else {
            self.audit_remote(
                user_id,
                "identity_denied",
                Some(decider),
                None,
                request_id,
                "unknown request".into(),
            )
            .await;
            return Err(ConversationError::not_found_reason(format!(
                "Approval request '{request_id}' not found"
            )));
        };
        if let Err(error) = self.check_identity(user_id, decider).await {
            self.audit_remote(
                user_id,
                "identity_denied",
                Some(decider),
                Some(&row),
                &row.id,
                error.to_string().into(),
            )
            .await;
            return Err(error);
        }
        // E12: the user must still be allowed to run this task's agent.
        let flow = self
            .task_board_repo()?
            .get_task_flow(user_id, &row.task_session_id)
            .await?;
        let mut facts = aionui_db::LaunchFacts::new(aionui_db::LaunchSurface::Task);
        facts.target = Some(("task_session", &row.task_session_id));
        facts.assistant_id = flow.as_ref().and_then(|f| f.assistant_id.as_deref());
        if let Err(error) = self.enforce_launch_policy(user_id, &facts).await {
            self.audit_remote(
                user_id,
                "identity_denied",
                Some(decider),
                Some(&row),
                &row.id,
                error.to_string().into(),
            )
            .await;
            return Err(error);
        }

        let now = now_ms();
        if row.status != "pending" {
            self.audit_remote(
                user_id,
                "replay",
                Some(decider),
                Some(&row),
                &row.id,
                serde_json::Value::Null,
            )
            .await;
            return Err(conflict(
                "approval_already_decided",
                format!("the request is {}", row.status),
            ));
        }
        if row.expires_at <= now {
            self.expire_due_approval_requests().await;
            self.audit_remote(
                user_id,
                "expired_attempt",
                Some(decider),
                Some(&row),
                &row.id,
                TaskApprovalInvalidation::Expired.as_str().into(),
            )
            .await;
            return Err(conflict("approval_expired", "the request expired; issue a new one"));
        }
        if !echo.matches(&row.digest) {
            self.audit_remote(
                user_id,
                "digest_mismatch",
                Some(decider),
                Some(&row),
                &row.id,
                "echoed digest".into(),
            )
            .await;
            return Err(conflict(
                "approval_digest_mismatch",
                "the digest does not match this request",
            ));
        }
        // Recompute what the request is bound to; a change voids it.
        if approve && let Err(reason) = self.current_digest_matches(user_id, &row).await {
            if repo
                .invalidate(user_id, &row.id, TaskApprovalInvalidation::DiffChanged.as_str(), now)
                .await?
            {
                let changed = repo.get(user_id, &row.id).await?.unwrap_or(row.clone());
                self.broadcast_approval_request(user_id, &changed);
            }
            self.audit_remote(
                user_id,
                "digest_mismatch",
                Some(decider),
                Some(&row),
                &row.id,
                reason.clone().into(),
            )
            .await;
            return Err(conflict("approval_digest_mismatch", reason));
        }

        let status = if approve { "approved" } else { "rejected" };
        if !repo.claim(user_id, &row.id, status, &decider.label(), now).await? {
            let latest = repo.get(user_id, &row.id).await?;
            self.audit_remote(
                user_id,
                "replay",
                Some(decider),
                latest.as_ref(),
                &row.id,
                "lost the race".into(),
            )
            .await;
            return Err(conflict("approval_already_decided", "the request was already decided"));
        }
        let claimed = repo.get(user_id, &row.id).await?.unwrap_or(row);
        tracing::info!(
            event = "remote_approval.decided",
            request_id = %claimed.id,
            task_session_id = %claimed.task_session_id,
            kind = %claimed.kind,
            status,
            decided_by = %decider.label(),
            "remote approval decided"
        );
        let finished = self
            .execute_remote_decision(user_id, &repo, claimed, approve, comment, decider)
            .await;
        self.broadcast_approval_request(user_id, &finished);
        let action = if approve { "approve" } else { "reject" };
        self.audit_remote(
            user_id,
            action,
            Some(decider),
            Some(&finished),
            &finished.id,
            serde_json::Value::Null,
        )
        .await;
        request_response(finished)
    }

    async fn current_digest_matches(&self, user_id: &str, row: &ApprovalRequestRow) -> Result<(), String> {
        match row.kind.as_str() {
            "plan" | "goal" => {
                let approval_id = row.approval_id.as_deref().unwrap_or_default();
                let approvals = self
                    .task_session_repo()
                    .map_err(|e| e.to_string())?
                    .list_approvals(user_id, &row.task_session_id)
                    .await
                    .map_err(|e| e.to_string())?;
                match approvals.iter().find(|a| a.id == approval_id) {
                    Some(a) if a.status == "pending" && a.artifact_hash == row.digest => Ok(()),
                    Some(a) if a.status == "pending" => Err("the plan changed".into()),
                    _ => Err("the approval is no longer pending".into()),
                }
            }
            "adopt" => {
                let port = read(&self.remote_approval.adopt).ok_or("remote adopt is not configured")?;
                let info = port
                    .digest(
                        user_id,
                        row.fanout_group_id.as_deref().unwrap_or_default(),
                        row.slot.as_deref().unwrap_or("A"),
                        row.strategy.as_deref().unwrap_or("squash"),
                        row.target_branch.as_deref(),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                if info.digest == row.digest && info.target_branch == row.target_branch {
                    Ok(())
                } else {
                    Err("the diff, branch or target changed".into())
                }
            }
            other => Err(format!("unknown kind {other}")),
        }
    }

    /// Run what the claimed decision means. Failures are recorded on the
    /// request (`error`) — the decision itself stays decided (one-shot).
    async fn execute_remote_decision(
        &self,
        user_id: &str,
        repo: &Arc<dyn IApprovalRequestRepository>,
        row: ApprovalRequestRow,
        approve: bool,
        comment: Option<&str>,
        decider: &RemoteDecider,
    ) -> ApprovalRequestRow {
        let outcome: Result<Option<String>, ConversationError> = match row.kind.as_str() {
            "plan" | "goal" => self.resolve_remote_plan(user_id, &row, approve, comment, decider).await,
            "adopt" if approve => match read(&self.remote_approval.adopt) {
                Some(port) => port
                    .adopt(
                        user_id,
                        row.fanout_group_id.as_deref().unwrap_or_default(),
                        row.slot.as_deref().unwrap_or("A"),
                        row.strategy.as_deref().unwrap_or("squash"),
                        row.target_branch.as_deref(),
                        &row.digest,
                    )
                    .await
                    .map(|commit| commit.map(|c| format!("commit:{c}"))),
                None => Err(ConversationError::internal("Remote adopt is not configured")),
            },
            _ => Ok(None),
        };
        let (result_ref, error) = match outcome {
            Ok(result_ref) => (result_ref, None),
            Err(error) => {
                tracing::warn!(%error, request_id = %row.id, "remote approval did not execute");
                (None, Some(error.to_string()))
            }
        };
        if result_ref.is_some() || error.is_some() {
            if let Err(e) = repo
                .record_result(&row.id, result_ref.as_deref(), error.as_deref())
                .await
            {
                tracing::warn!(error = %e, request_id = %row.id, "recording the remote approval result failed");
            }
            if error.is_some() {
                self.audit_remote(
                    user_id,
                    "execute_failed",
                    Some(decider),
                    Some(&row),
                    &row.id,
                    error.clone().into(),
                )
                .await;
            }
        }
        repo.get(user_id, &row.id).await.ok().flatten().unwrap_or(row)
    }

    async fn resolve_remote_plan(
        &self,
        user_id: &str,
        row: &ApprovalRequestRow,
        approve: bool,
        comment: Option<&str>,
        decider: &RemoteDecider,
    ) -> Result<Option<String>, ConversationError> {
        let status = if approve { "approved" } else { "rejected" };
        let approval = self
            .task_session_repo()?
            .resolve_approval(&ResolveTaskApprovalParams {
                user_id,
                task_session_id: &row.task_session_id,
                approval_id: row.approval_id.as_deref().unwrap_or_default(),
                artifact_id: row.artifact_id.as_deref().unwrap_or_default(),
                artifact_hash: &row.digest,
                status,
                resolved_by: user_id,
                comment,
                resolved_at: now_ms(),
            })
            .await?;
        let label = decider.label();
        let via = match decider {
            RemoteDecider::Device(id) => crate::TaskApprovalDecider::Device(id),
            RemoteDecider::Im { .. } => crate::TaskApprovalDecider::Im(&label),
        };
        self.task_approval_changed(user_id, None, &approval, Some(via)).await;
        // D8: an approved plan runs at once; the run is recorded on the request.
        if approve && approval.approval_type == "plan" {
            tokio::spawn(crate::board::plan_first::remote_execute_job(
                self.clone(),
                user_id.to_owned(),
                approval,
                row.id.clone(),
            ));
        }
        Ok(None)
    }

    /// Called by the remote execute job once the approved plan ran.
    pub(crate) async fn record_remote_run(
        &self,
        user_id: &str,
        request_id: &str,
        run_id: Option<&str>,
        error: Option<&str>,
    ) {
        let Some(repo) = self.approval_request_repo_optional() else {
            return;
        };
        let result_ref = run_id.map(|id| format!("run:{id}"));
        if let Err(e) = repo.record_result(request_id, result_ref.as_deref(), error).await {
            tracing::warn!(error = %e, request_id, "recording the remote run failed");
            return;
        }
        if let Ok(Some(row)) = repo.get(user_id, request_id).await {
            self.broadcast_approval_request(user_id, &row);
            let action = if error.is_some() { "execute_failed" } else { "executed" };
            self.audit_remote(user_id, action, None, Some(&row), request_id, serde_json::Value::Null)
                .await;
        }
    }

    /// The desktop decided the approval itself: its remote request is done.
    pub(crate) async fn settle_approval_request_from_desktop(&self, user_id: &str, approval: &TaskApprovalRow) {
        let Some(repo) = self.approval_request_repo_optional() else {
            return;
        };
        let Ok(Some(row)) = repo.find_for_approval(user_id, &approval.id).await else {
            return;
        };
        let status = if approval.status == "approved" {
            "approved"
        } else {
            "rejected"
        };
        if matches!(
            repo.claim(user_id, &row.id, status, "desktop", now_ms()).await,
            Ok(true)
        ) && let Ok(Some(row)) = repo.get(user_id, &row.id).await
        {
            self.broadcast_approval_request(user_id, &row);
        }
    }

    /// Plan/Goal approvals that were superseded or whose task ended.
    pub(crate) async fn invalidate_approval_requests_for(
        &self,
        user_id: &str,
        approvals: &[TaskApprovalRow],
        reason: &str,
    ) {
        let Some(repo) = self.approval_request_repo_optional() else {
            return;
        };
        for approval in approvals {
            if let Ok(Some(row)) = repo.find_for_approval(user_id, &approval.id).await
                && matches!(repo.invalidate(user_id, &row.id, reason, now_ms()).await, Ok(true))
                && let Ok(Some(row)) = repo.get(user_id, &row.id).await
            {
                self.broadcast_approval_request(user_id, &row);
                self.audit_remote(user_id, "invalidate", None, Some(&row), &row.id, reason.into())
                    .await;
            }
        }
    }

    /// A task left review or ended: its adopt request no longer applies.
    pub(crate) async fn invalidate_adopt_requests(&self, user_id: &str, task_session_id: &str, reason: &str) {
        let Some(repo) = self.approval_request_repo_optional() else {
            return;
        };
        if let Ok(rows) = repo
            .invalidate_pending(user_id, task_session_id, Some("adopt"), reason, now_ms())
            .await
        {
            for row in rows {
                self.broadcast_approval_request(user_id, &row);
                self.audit_remote(user_id, "invalidate", None, Some(&row), &row.id, reason.into())
                    .await;
            }
        }
    }

    /// `pending → expired` for every overdue request (run by a timer and on
    /// demand). The task stays in "needs you"; the desktop can re-issue.
    pub async fn expire_due_approval_requests(&self) -> usize {
        let Some(repo) = self.approval_request_repo_optional() else {
            return 0;
        };
        match repo.expire_due(now_ms()).await {
            Ok(rows) => {
                for row in &rows {
                    self.broadcast_approval_request(&row.user_id, row);
                    self.audit_remote(
                        &row.user_id,
                        "expire",
                        None,
                        Some(row),
                        &row.id,
                        serde_json::Value::Null,
                    )
                    .await;
                }
                rows.len()
            }
            Err(error) => {
                tracing::warn!(%error, "expiring approval requests failed");
                0
            }
        }
    }

    pub async fn remote_approval_settings(&self, user_id: &str) -> Result<RemoteApprovalSettings, ConversationError> {
        let ttl_minutes = self
            .approval_request_repo()?
            .ttl_minutes(user_id)
            .await?
            .unwrap_or(DEFAULT_APPROVAL_TTL_MINUTES);
        Ok(RemoteApprovalSettings { ttl_minutes })
    }

    pub async fn set_remote_approval_settings(
        &self,
        user_id: &str,
        settings: RemoteApprovalSettings,
    ) -> Result<RemoteApprovalSettings, ConversationError> {
        if !(1..=MAX_APPROVAL_TTL_MINUTES).contains(&settings.ttl_minutes) {
            return Err(ConversationError::bad_request(format!(
                "ttl_minutes must be between 1 and {MAX_APPROVAL_TTL_MINUTES}"
            )));
        }
        self.approval_request_repo()?
            .set_ttl_minutes(user_id, settings.ttl_minutes, now_ms())
            .await?;
        Ok(settings)
    }

    pub async fn list_remote_approvers(&self, user_id: &str) -> Result<Vec<RemoteApproverResponse>, ConversationError> {
        Ok(self
            .approval_request_repo()?
            .list_approvers(user_id)
            .await?
            .into_iter()
            .map(|row| RemoteApproverResponse {
                platform_type: row.platform_type,
                platform_user_id: row.platform_user_id,
                created_at: row.created_at,
            })
            .collect())
    }

    /// Bind an IM identity as "me". It must already be paired in the channel.
    pub async fn add_remote_approver(
        &self,
        user_id: &str,
        request: RemoteApproverRequest,
    ) -> Result<RemoteApproverResponse, ConversationError> {
        let platform = request.platform_type.trim();
        let identity = request.platform_user_id.trim();
        if platform.is_empty() || identity.is_empty() {
            return Err(ConversationError::bad_request(
                "platform_type and platform_user_id are required",
            ));
        }
        let paired = match read(&self.remote_approval.identities) {
            Some(port) => port.is_paired(user_id, platform, identity).await,
            None => false,
        };
        if !paired {
            return Err(ConversationError::bad_request(
                "Only an IM account already paired in the channel can be bound as an approver",
            ));
        }
        let now = now_ms();
        self.approval_request_repo()?
            .add_approver(user_id, platform, identity, now)
            .await?;
        Ok(RemoteApproverResponse {
            platform_type: platform.to_owned(),
            platform_user_id: identity.to_owned(),
            created_at: now,
        })
    }

    pub async fn remove_remote_approver(
        &self,
        user_id: &str,
        platform: &str,
        identity: &str,
    ) -> Result<(), ConversationError> {
        if !self
            .approval_request_repo()?
            .remove_approver(user_id, platform, identity)
            .await?
        {
            return Err(ConversationError::not_found_reason("approver not found"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_echo_needs_the_full_digest_or_a_long_hex_prefix() {
        let stored = "a".repeat(40) + &"b".repeat(24);
        assert!(DigestEcho::Full(&stored).matches(&stored));
        assert!(!DigestEcho::Full(&stored[..63]).matches(&stored));
        assert!(!DigestEcho::Full("").matches(&stored));
        assert!(DigestEcho::Prefix(&stored[..24]).matches(&stored));
        assert!(DigestEcho::Prefix(&stored).matches(&stored));
        assert!(!DigestEcho::Prefix(&stored[..23]).matches(&stored), "too short");
        assert!(!DigestEcho::Prefix(&"c".repeat(24)).matches(&stored));
        let not_hex = "z".repeat(24);
        assert!(!DigestEcho::Prefix(&not_hex).matches(&("z".repeat(64))));
        assert!(!DigestEcho::Prefix(&(stored.clone() + "a")).matches(&stored));
    }

    #[test]
    fn decider_labels() {
        assert_eq!(RemoteDecider::Device("d1".into()).label(), "device:d1");
        assert_eq!(
            RemoteDecider::Im {
                platform: "telegram".into(),
                identity: "42".into()
            }
            .label(),
            "im:telegram:42"
        );
    }
}
