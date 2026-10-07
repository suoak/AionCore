//! Codex-managed ChatGPT account service.
//!
//! This process talks only to official app-server account RPCs. It never reads
//! Codex credential files and intentionally has no token-injection method.
//! Wire evidence: Codex CLI 0.160.1 generated schemas under
//! `~/aion/protocols/samples/codex-cli/0.160.1/schema-full/`.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aionui_api_types::{
    CodexAccountErrorCode, CodexAccountUsageSummary, CodexAccountUsageView, CodexAccountView, CodexAccountWarning,
    CodexAuthState, CodexCreditsSnapshot, CodexDailyUsageBucket, CodexDiagnosticCheck, CodexDiagnosticStatus,
    CodexDiagnosticsView, CodexLoginStartResponse, CodexRateLimitBucket, CodexRateLimitWindow, CodexRateLimitsView,
    CodexResetCredit, CodexResetCreditSummary, CodexSnapshotFreshness, CodexSnapshotSource, CodexSpendControlSnapshot,
    CodexUsageAvailability, WebSocketMessage,
};
use aionui_common::{CommandSpec, now_ms};
use aionui_process::{ManagedProcess, Spawner};
use aionui_realtime::EventBroadcaster;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc, oneshot};

use super::codex_account_state::CodexAccountStateMachine;
use crate::AgentError;

const RPC_TIMEOUT: Duration = Duration::from_secs(20);
const NOTIFICATION_DEBOUNCE: Duration = Duration::from_millis(75);
pub const CODEX_ACCOUNT_UPDATED_EVENT: &str = "codex.accountUpdated";

#[derive(Debug, Clone)]
struct RpcNotification {
    method: String,
    params: Value,
}

#[derive(Debug, thiserror::Error)]
enum CodexRpcError {
    #[error("Codex runtime unavailable")]
    RuntimeUnavailable,
    #[error("Codex account protocol is unsupported")]
    Unsupported,
    #[error("Codex account request failed: {0}")]
    Request(String),
}

#[async_trait]
trait CodexAccountRpc: Send + Sync {
    async fn call(&self, method: &'static str, params: Value) -> Result<Value, CodexRpcError>;
    fn subscribe(&self) -> broadcast::Receiver<RpcNotification>;
}

struct RpcCall {
    method: &'static str,
    params: Value,
    response: oneshot::Sender<Result<Value, CodexRpcError>>,
}

#[derive(Clone)]
struct AppServerRpc {
    _process: Arc<ManagedProcess>,
    calls: mpsc::Sender<RpcCall>,
    notifications: broadcast::Sender<RpcNotification>,
}

impl AppServerRpc {
    async fn spawn(spawner: Arc<dyn Spawner>, program: PathBuf) -> Result<Arc<Self>, CodexRpcError> {
        let process = spawner
            .spawn(
                CommandSpec {
                    command: program,
                    args: vec!["app-server".into(), "--stdio".into()],
                    env: Vec::new(),
                    cwd: None,
                },
                &[],
                "codex-account",
            )
            .await
            .map_err(|_| CodexRpcError::RuntimeUnavailable)?;
        let (stdin, stdout) = process.take_stdio().await.ok_or(CodexRpcError::RuntimeUnavailable)?;
        let (call_tx, call_rx) = mpsc::channel(32);
        let (notification_tx, _) = broadcast::channel(64);
        tokio::spawn(run_rpc_actor(stdin, stdout, call_rx, notification_tx.clone()));
        let client = Arc::new(Self {
            _process: process,
            calls: call_tx,
            notifications: notification_tx,
        });
        client
            .call(
                "initialize",
                json!({
                    "clientInfo": {"name": "workmate-codex-account", "version": env!("CARGO_PKG_VERSION")},
                    "capabilities": {"experimentalApi": false, "requestAttestation": false}
                }),
            )
            .await?;
        Ok(client)
    }
}

#[async_trait]
impl CodexAccountRpc for AppServerRpc {
    async fn call(&self, method: &'static str, params: Value) -> Result<Value, CodexRpcError> {
        let (tx, rx) = oneshot::channel();
        self.calls
            .send(RpcCall {
                method,
                params,
                response: tx,
            })
            .await
            .map_err(|_| CodexRpcError::RuntimeUnavailable)?;
        tokio::time::timeout(RPC_TIMEOUT, rx)
            .await
            .map_err(|_| CodexRpcError::Request("request timed out".into()))?
            .map_err(|_| CodexRpcError::RuntimeUnavailable)?
    }

    fn subscribe(&self) -> broadcast::Receiver<RpcNotification> {
        self.notifications.subscribe()
    }
}

async fn run_rpc_actor(
    mut stdin: aionui_process::BoxedStdin,
    stdout: aionui_process::BoxedStdout,
    mut calls: mpsc::Receiver<RpcCall>,
    notifications: broadcast::Sender<RpcNotification>,
) {
    let mut lines = BufReader::new(stdout).lines();
    let mut next_id = 1_u64;
    let mut pending: HashMap<u64, oneshot::Sender<Result<Value, CodexRpcError>>> = HashMap::new();

    loop {
        tokio::select! {
            command = calls.recv() => {
                let Some(command) = command else { break };
                let id = next_id;
                next_id = next_id.saturating_add(1);
                let frame = json!({
                    "jsonrpc": "2.0", "id": id, "method": command.method, "params": command.params
                });
                let write_result = async {
                    let mut bytes = serde_json::to_vec(&frame).map_err(|_| ())?;
                    bytes.push(b'\n');
                    stdin.write_all(&bytes).await.map_err(|_| ())?;
                    stdin.flush().await.map_err(|_| ())
                }.await;
                if write_result.is_err() {
                    let _ = command.response.send(Err(CodexRpcError::RuntimeUnavailable));
                    break;
                }
                pending.insert(id, command.response);
            }
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let Ok(frame) = serde_json::from_str::<Value>(&line) else { continue };
                if let Some(id) = frame.get("id").and_then(Value::as_u64) {
                    if frame.get("method").is_some() {
                        // Account service owns no host tokens and exposes no reverse
                        // auth seam. Replying prevents an unknown request deadlock.
                        let response = json!({
                            "jsonrpc":"2.0", "id":id,
                            "error":{"code":-32601,"message":"unsupported by WorkMate account service"}
                        });
                        if let Ok(mut bytes) = serde_json::to_vec(&response) {
                            bytes.push(b'\n');
                            let _ = stdin.write_all(&bytes).await;
                            let _ = stdin.flush().await;
                        }
                    } else if let Some(reply) = pending.remove(&id) {
                        let result = if let Some(error) = frame.get("error") {
                            let code = error.get("code").and_then(Value::as_i64).unwrap_or_default();
                            if code == -32601 {
                                Err(CodexRpcError::Unsupported)
                            } else {
                                let message = error.get("message").and_then(Value::as_str).unwrap_or("request failed");
                                Err(CodexRpcError::Request(redact_auth_error(message)))
                            }
                        } else {
                            Ok(frame.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = reply.send(result);
                    }
                } else if let Some(method) = frame.get("method").and_then(Value::as_str) {
                    let _ = notifications.send(RpcNotification {
                        method: method.to_owned(),
                        params: frame.get("params").cloned().unwrap_or(Value::Null),
                    });
                }
            }
        }
    }
    for (_, reply) in pending {
        let _ = reply.send(Err(CodexRpcError::RuntimeUnavailable));
    }
}

pub struct CodexAccountService {
    spawner: Arc<dyn Spawner>,
    program: PathBuf,
    broadcaster: Arc<dyn EventBroadcaster>,
    rpc: Mutex<Option<Arc<dyn CodexAccountRpc>>>,
    state: Mutex<CodexAccountStateMachine>,
    raw_rate_limits: RwLock<Option<Value>>,
    rate_limits: RwLock<Option<CodexRateLimitsView>>,
    usage: RwLock<Option<CodexAccountUsageView>>,
    diagnostics: RwLock<CodexDiagnosticsView>,
    installed_version: RwLock<Option<String>>,
    warnings: RwLock<Vec<CodexAccountWarning>>,
    refresh_gate: Mutex<()>,
    invalidation_generation: AtomicU64,
    identity_changed: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl CodexAccountService {
    pub fn new(
        spawner: Arc<dyn Spawner>,
        program: PathBuf,
        broadcaster: Arc<dyn EventBroadcaster>,
        identity_changed: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            spawner,
            program,
            broadcaster,
            rpc: Mutex::new(None),
            state: Mutex::new(CodexAccountStateMachine::default()),
            raw_rate_limits: RwLock::new(None),
            rate_limits: RwLock::new(None),
            usage: RwLock::new(None),
            diagnostics: RwLock::new(empty_diagnostics()),
            installed_version: RwLock::new(None),
            warnings: RwLock::new(Vec::new()),
            refresh_gate: Mutex::new(()),
            invalidation_generation: AtomicU64::new(0),
            identity_changed,
        })
    }

    async fn rpc(self: &Arc<Self>) -> Result<Arc<dyn CodexAccountRpc>, AgentError> {
        let mut slot = self.rpc.lock().await;
        if let Some(rpc) = slot.as_ref() {
            return Ok(rpc.clone());
        }
        *self.installed_version.write().await = probe_codex_version(&self.program).await;
        let rpc: Arc<dyn CodexAccountRpc> = AppServerRpc::spawn(self.spawner.clone(), self.program.clone())
            .await
            .map_err(map_rpc_error)?;
        let mut notifications = rpc.subscribe();
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            while let Ok(notification) = notifications.recv().await {
                let Some(service) = weak.upgrade() else { break };
                service.handle_notification(notification).await;
            }
        });
        *slot = Some(rpc.clone());
        Ok(rpc)
    }

    /// Startup hook: initialize app-server then authoritatively read account.
    pub async fn initialize(self: &Arc<Self>) -> Result<CodexAccountView, AgentError> {
        self.refresh().await
    }

    pub async fn view(self: &Arc<Self>) -> Result<CodexAccountView, AgentError> {
        if self.state.lock().await.snapshot().auth_state == CodexAuthState::Unknown {
            return self.refresh().await;
        }
        Ok(self.current_view().await)
    }

    pub async fn refresh(self: &Arc<Self>) -> Result<CodexAccountView, AgentError> {
        let _single_flight = self.refresh_gate.lock().await;
        let rpc = self.rpc().await?;
        let response = match rpc.call("account/read", json!({"refreshToken": false})).await {
            Ok(response) => response,
            Err(error) => {
                self.state.lock().await.account_read_failed(now_ms());
                return Err(map_account_read_error(error));
            }
        };
        let identity_changed = self.state.lock().await.apply_account_read(&response, now_ms());
        if identity_changed && let Some(callback) = &self.identity_changed {
            callback();
        }

        // Account identity is the cache generation boundary. Never show the
        // previous account's optional snapshots if a new account's read fails.
        if identity_changed {
            *self.raw_rate_limits.write().await = None;
            *self.rate_limits.write().await = None;
            *self.usage.write().await = None;
        }

        let signed_in = self.state.lock().await.snapshot().auth_state == CodexAuthState::SignedIn;
        self.warnings.write().await.clear();
        if signed_in {
            let (rates, usage, diagnostics) = tokio::join!(
                rpc.call("account/rateLimits/read", Value::Null),
                rpc.call("account/usage/read", Value::Null),
                rpc.call("server/diagnostics", json!({})),
            );
            self.apply_optional_snapshot(rates, true).await;
            self.apply_optional_snapshot(usage, false).await;
            self.apply_diagnostics(diagnostics).await;
        } else {
            *self.raw_rate_limits.write().await = None;
            *self.rate_limits.write().await = None;
            *self.usage.write().await = None;
            self.apply_diagnostics(rpc.call("server/diagnostics", json!({})).await)
                .await;
        }
        let view = self.current_view().await;
        self.broadcast(&view);
        Ok(view)
    }

    pub async fn start_chatgpt_login(self: &Arc<Self>) -> Result<CodexLoginStartResponse, AgentError> {
        if self.state.lock().await.snapshot().active_login.is_some() {
            return Err(AgentError::bad_request("A Codex ChatGPT login is already active"));
        }
        let response = self
            .rpc()
            .await?
            .call("account/login/start", json!({"type":"chatgpt"}))
            .await
            .map_err(map_rpc_error)?;
        if response.get("type").and_then(Value::as_str) != Some("chatgpt") {
            return Err(AgentError::bad_gateway("Codex returned an unsupported login mode"));
        }
        let login_id = required_string(&response, "loginId")?;
        let authorization_url = required_string(&response, "authUrl")?;
        self.state.lock().await.begin_login(login_id.clone(), now_ms());
        tracing::info!(login_id = %login_id, "codex account login started");
        let account = self.state.lock().await.snapshot();
        self.broadcast(&self.current_view().await);
        Ok(CodexLoginStartResponse {
            account,
            authorization_url,
        })
    }

    pub async fn cancel_login(self: &Arc<Self>) -> Result<CodexAccountView, AgentError> {
        let login_id = self
            .state
            .lock()
            .await
            .snapshot()
            .active_login
            .map(|attempt| attempt.login_id)
            .ok_or_else(|| AgentError::bad_request("No Codex login is active"))?;
        self.state.lock().await.begin_cancel(&login_id, now_ms());
        let result = self
            .rpc()
            .await?
            .call("account/login/cancel", json!({"loginId": login_id}))
            .await;
        let refreshed = self.refresh().await;
        result.map_err(map_rpc_error)?;
        refreshed
    }

    pub async fn logout(self: &Arc<Self>) -> Result<CodexAccountView, AgentError> {
        self.rpc()
            .await?
            .call("account/logout", json!({}))
            .await
            .map_err(|_| AgentError::bad_gateway("LOGOUT_FAILED: Codex logout failed"))?;
        *self.rate_limits.write().await = None;
        *self.raw_rate_limits.write().await = None;
        *self.usage.write().await = None;
        self.refresh().await
    }

    async fn apply_optional_snapshot(&self, result: Result<Value, CodexRpcError>, rate_limits: bool) {
        match result {
            Ok(value) => {
                if rate_limits {
                    *self.raw_rate_limits.write().await = Some(value.clone());
                    *self.rate_limits.write().await = Some(project_rate_limits(
                        &value,
                        now_ms(),
                        CodexSnapshotSource::Read,
                        CodexSnapshotFreshness::Fresh,
                    ));
                } else {
                    *self.usage.write().await = Some(project_account_usage(&value, now_ms()));
                }
            }
            Err(_) => {
                let (code, message) = if rate_limits {
                    (
                        CodexAccountErrorCode::RateLimitReadFailed,
                        "Codex rate limits are unavailable",
                    )
                } else {
                    (
                        CodexAccountErrorCode::UsageReadFailed,
                        "Codex token usage is unavailable",
                    )
                };
                self.warnings.write().await.push(CodexAccountWarning {
                    code,
                    message: message.into(),
                });
                if rate_limits {
                    let mut snapshot = self.rate_limits.write().await;
                    if snapshot.is_some() {
                        mark_rate_freshness(&mut snapshot, CodexSnapshotFreshness::Stale);
                    } else {
                        *snapshot = Some(CodexRateLimitsView {
                            ordinary_usage_allowed: None,
                            availability: CodexUsageAvailability::Unknown,
                            buckets: Vec::new(),
                            reset_credits: None,
                            fetched_at: now_ms(),
                            source: CodexSnapshotSource::Read,
                            freshness: CodexSnapshotFreshness::Unavailable,
                        });
                    }
                } else {
                    let mut snapshot = self.usage.write().await;
                    if snapshot.is_some() {
                        mark_usage_freshness(&mut snapshot, CodexSnapshotFreshness::Stale);
                    } else {
                        *snapshot = Some(CodexAccountUsageView {
                            summary: CodexAccountUsageSummary::default(),
                            daily_buckets: Vec::new(),
                            fetched_at: now_ms(),
                            source: CodexSnapshotSource::Read,
                            freshness: CodexSnapshotFreshness::Unavailable,
                        });
                    }
                }
            }
        }
    }

    async fn apply_diagnostics(&self, result: Result<Value, CodexRpcError>) {
        let now = now_ms();
        let account = self.state.lock().await.snapshot();
        let warnings = self.warnings.read().await.clone();
        let mut checks = vec![CodexDiagnosticCheck {
            id: "runtime.version".into(),
            status: if self.installed_version.read().await.is_some() {
                CodexDiagnosticStatus::Pass
            } else {
                CodexDiagnosticStatus::Warn
            },
            summary: "Codex runtime version probe".into(),
            remediation: self
                .installed_version
                .read()
                .await
                .is_none()
                .then(|| "Verify that the configured Codex executable is runnable".into()),
        }];
        checks.push(CodexDiagnosticCheck {
            id: "account.authentication".into(),
            status: match account.auth_state {
                CodexAuthState::SignedIn => CodexDiagnosticStatus::Pass,
                CodexAuthState::SignedOut => CodexDiagnosticStatus::NotApplicable,
                CodexAuthState::Authenticating | CodexAuthState::Unknown => CodexDiagnosticStatus::Warn,
                CodexAuthState::Error => CodexDiagnosticStatus::Fail,
            },
            summary: "Codex-managed account authentication".into(),
            remediation: (account.auth_state == CodexAuthState::Error)
                .then(|| "Refresh the account or sign in again through Codex".into()),
        });
        for (id, warning_code, summary) in [
            (
                "account.rate_limits",
                CodexAccountErrorCode::RateLimitReadFailed,
                "Rate-limit snapshot",
            ),
            (
                "account.usage",
                CodexAccountErrorCode::UsageReadFailed,
                "Account usage snapshot",
            ),
        ] {
            let failed = warnings.iter().any(|warning| warning.code == warning_code);
            checks.push(CodexDiagnosticCheck {
                id: id.into(),
                status: if account.auth_state != CodexAuthState::SignedIn {
                    CodexDiagnosticStatus::NotApplicable
                } else if failed {
                    CodexDiagnosticStatus::Warn
                } else {
                    CodexDiagnosticStatus::Pass
                },
                summary: summary.into(),
                remediation: failed.then(|| "Refresh after checking account connectivity".into()),
            });
        }
        let (server_status, remediation) = match result {
            Ok(value)
                if value
                    .get("process")
                    .and_then(|v| v.get("id"))
                    .and_then(Value::as_u64)
                    .is_some() =>
            {
                (CodexDiagnosticStatus::Pass, None)
            }
            _ => (
                CodexDiagnosticStatus::Warn,
                Some("Update Codex or retry the read-only diagnostic".into()),
            ),
        };
        checks.push(CodexDiagnosticCheck {
            id: "server.process".into(),
            status: server_status,
            summary: "Content-free app-server diagnostics".into(),
            remediation,
        });
        if server_status != CodexDiagnosticStatus::Pass {
            self.warnings.write().await.push(CodexAccountWarning {
                code: CodexAccountErrorCode::DiagnosticFailed,
                message: "Codex diagnostics are unavailable".into(),
            });
        }
        *self.diagnostics.write().await = CodexDiagnosticsView {
            checks,
            fetched_at: now,
            freshness: if server_status == CodexDiagnosticStatus::Pass {
                CodexSnapshotFreshness::Fresh
            } else {
                CodexSnapshotFreshness::Unavailable
            },
        };
    }

    async fn handle_notification(self: Arc<Self>, notification: RpcNotification) {
        match notification.method.as_str() {
            "account/login/completed" => {
                let login_id = notification.params.get("loginId").and_then(Value::as_str);
                let success = notification
                    .params
                    .get("success")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let accepted = self.state.lock().await.complete_login(login_id, success, now_ms());
                if accepted {
                    if !success {
                        self.warnings.write().await.push(CodexAccountWarning {
                            code: CodexAccountErrorCode::LoginFailed,
                            message: "ChatGPT login did not complete".into(),
                        });
                    }
                    self.schedule_refresh();
                } else {
                    tracing::debug!(login_id, "ignoring stale Codex login completion");
                }
            }
            "account/updated" => self.schedule_refresh(),
            "account/rateLimits/updated" => {
                self.merge_rate_limit_notification(&notification.params).await;
                self.schedule_refresh();
            }
            _ => {}
        }
    }

    fn schedule_refresh(self: &Arc<Self>) {
        let weak_refresh = Arc::downgrade(self);
        tokio::spawn(async move {
            if let Some(service) = weak_refresh.upgrade() {
                {
                    let mut snapshot = service.rate_limits.write().await;
                    mark_rate_freshness(&mut snapshot, CodexSnapshotFreshness::Refreshing);
                }
                {
                    let mut snapshot = service.usage.write().await;
                    mark_usage_freshness(&mut snapshot, CodexSnapshotFreshness::Refreshing);
                }
                let view = service.current_view().await;
                service.broadcast(&view);
            }
        });
        let generation = self.invalidation_generation.fetch_add(1, Ordering::AcqRel) + 1;
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(NOTIFICATION_DEBOUNCE).await;
            let Some(service) = weak.upgrade() else { return };
            if service.invalidation_generation.load(Ordering::Acquire) != generation {
                return;
            }
            if let Err(error) = service.refresh().await {
                tracing::warn!(error = %error, "codex account authoritative refresh failed");
            }
        });
    }

    async fn merge_rate_limit_notification(&self, params: &Value) {
        let Some(update) = params.get("rateLimits").and_then(Value::as_object) else {
            return;
        };
        let mut raw = self.raw_rate_limits.write().await;
        let Some(root) = raw.as_mut().and_then(Value::as_object_mut) else {
            return;
        };
        let bucket = root.entry("rateLimits").or_insert_with(|| json!({}));
        merge_sparse_json(bucket, &Value::Object(update.clone()));
        let merged = Value::Object(root.clone());
        *self.rate_limits.write().await = Some(project_rate_limits(
            &merged,
            now_ms(),
            CodexSnapshotSource::NotificationMerge,
            CodexSnapshotFreshness::Refreshing,
        ));
    }

    async fn current_view(&self) -> CodexAccountView {
        CodexAccountView {
            account: self.state.lock().await.snapshot(),
            rate_limits: self.rate_limits.read().await.clone(),
            usage: self.usage.read().await.clone(),
            diagnostics: self.diagnostics.read().await.clone(),
            warnings: self.warnings.read().await.clone(),
            installed_version: self.installed_version.read().await.clone(),
            required_version: aionui_session::VERIFIED_CODEX_VERSION.into(),
        }
    }

    fn broadcast(&self, view: &CodexAccountView) {
        if let Ok(payload) = serde_json::to_value(view) {
            self.broadcaster
                .broadcast(WebSocketMessage::new(CODEX_ACCOUNT_UPDATED_EVENT, payload));
        }
    }
}

fn empty_diagnostics() -> CodexDiagnosticsView {
    CodexDiagnosticsView {
        checks: Vec::new(),
        fetched_at: 0,
        freshness: CodexSnapshotFreshness::Unavailable,
    }
}

fn merge_sparse_json(target: &mut Value, update: &Value) {
    if let (Some(target), Some(update)) = (target.as_object_mut(), update.as_object()) {
        for (key, value) in update {
            if value.is_null() {
                continue;
            }
            match target.get_mut(key) {
                Some(existing) if existing.is_object() && value.is_object() => merge_sparse_json(existing, value),
                _ => {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
    } else if !update.is_null() {
        *target = update.clone();
    }
}

fn mark_rate_freshness(snapshot: &mut Option<CodexRateLimitsView>, freshness: CodexSnapshotFreshness) {
    if let Some(snapshot) = snapshot {
        snapshot.freshness = freshness;
    }
}

fn mark_usage_freshness(snapshot: &mut Option<CodexAccountUsageView>, freshness: CodexSnapshotFreshness) {
    if let Some(snapshot) = snapshot {
        snapshot.freshness = freshness;
    }
}

fn project_rate_limits(
    value: &Value,
    fetched_at: i64,
    source: CodexSnapshotSource,
    freshness: CodexSnapshotFreshness,
) -> CodexRateLimitsView {
    let ordinary_usage_allowed = value.get("ordinaryUsageAllowed").and_then(Value::as_bool);
    let mut buckets = BTreeMap::<String, CodexRateLimitBucket>::new();
    if let Some(by_id) = value.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (key, raw) in by_id {
            let bucket = project_rate_bucket(key, raw);
            buckets.insert(key.clone(), bucket);
        }
    }
    if let Some(raw) = value.get("rateLimits").filter(|value| value.is_object()) {
        let inferred_key = raw
            .get("limitId")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty())
            .unwrap_or("default");
        buckets
            .entry(inferred_key.to_owned())
            .or_insert_with(|| project_rate_bucket(inferred_key, raw));
    }
    let buckets: Vec<_> = buckets.into_values().collect();
    let explicitly_blocked = buckets
        .iter()
        .any(|bucket| bucket.rate_limit_reached_type.is_some() || bucket.spend_control_reached == Some(true));
    let saturated = buckets.iter().any(|bucket| {
        bucket.primary.as_ref().is_some_and(|window| window.used_percent >= 100)
            || bucket
                .secondary
                .as_ref()
                .is_some_and(|window| window.used_percent >= 100)
    });
    let availability = if ordinary_usage_allowed == Some(false) || explicitly_blocked {
        CodexUsageAvailability::Blocked
    } else if ordinary_usage_allowed == Some(true) && saturated {
        CodexUsageAvailability::Limited
    } else if ordinary_usage_allowed == Some(true) {
        CodexUsageAvailability::Available
    } else {
        CodexUsageAvailability::Unknown
    };
    CodexRateLimitsView {
        ordinary_usage_allowed,
        availability,
        buckets,
        reset_credits: value.get("rateLimitResetCredits").and_then(project_reset_credits),
        fetched_at,
        source,
        freshness,
    }
}

fn project_rate_bucket(key: &str, value: &Value) -> CodexRateLimitBucket {
    CodexRateLimitBucket {
        key: key.to_owned(),
        limit_id: string_field(value, "limitId"),
        limit_name: string_field(value, "limitName"),
        normal_model_slug: string_field(value, "normalModelSlug"),
        plan_type: string_field(value, "planType"),
        primary: value.get("primary").and_then(project_rate_window),
        secondary: value.get("secondary").and_then(project_rate_window),
        credits: value.get("credits").and_then(|credits| {
            Some(CodexCreditsSnapshot {
                has_credits: credits.get("hasCredits")?.as_bool()?,
                unlimited: credits.get("unlimited")?.as_bool()?,
                balance: string_field(credits, "balance"),
            })
        }),
        individual_limit: value.get("individualLimit").and_then(|limit| {
            Some(CodexSpendControlSnapshot {
                limit: limit.get("limit")?.as_str()?.to_owned(),
                used: limit.get("used")?.as_str()?.to_owned(),
                remaining_percent: limit.get("remainingPercent")?.as_i64()?,
                resets_at: limit.get("resetsAt")?.as_i64()?,
            })
        }),
        rate_limit_reached_type: string_field(value, "rateLimitReachedType"),
        spend_control_reached: value.get("spendControlReached").and_then(Value::as_bool),
    }
}

fn project_rate_window(value: &Value) -> Option<CodexRateLimitWindow> {
    Some(CodexRateLimitWindow {
        used_percent: value.get("usedPercent")?.as_i64()?,
        resets_at: value.get("resetsAt").and_then(Value::as_i64),
        window_duration_mins: value.get("windowDurationMins").and_then(Value::as_i64),
    })
}

fn project_reset_credits(value: &Value) -> Option<CodexResetCreditSummary> {
    let credits = value
        .get("credits")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|credit| {
            Some(CodexResetCredit {
                id: credit.get("id")?.as_str()?.to_owned(),
                status: credit.get("status")?.as_str()?.to_owned(),
                reset_type: credit.get("resetType")?.as_str()?.to_owned(),
                granted_at: credit.get("grantedAt")?.as_i64()?,
                expires_at: credit.get("expiresAt").and_then(Value::as_i64),
                title: string_field(credit, "title"),
                description: string_field(credit, "description"),
            })
        })
        .collect();
    Some(CodexResetCreditSummary {
        available_count: value.get("availableCount")?.as_i64()?,
        credits,
    })
}

fn project_account_usage(value: &Value, fetched_at: i64) -> CodexAccountUsageView {
    let summary = value.get("summary").unwrap_or(&Value::Null);
    let daily_buckets = value
        .get("dailyUsageBuckets")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|bucket| {
            Some(CodexDailyUsageBucket {
                start_date: bucket.get("startDate")?.as_str()?.to_owned(),
                tokens: bucket.get("tokens")?.as_i64()?,
            })
        })
        .collect();
    CodexAccountUsageView {
        summary: CodexAccountUsageSummary {
            lifetime_tokens: summary.get("lifetimeTokens").and_then(Value::as_i64),
            peak_daily_tokens: summary.get("peakDailyTokens").and_then(Value::as_i64),
            longest_running_turn_sec: summary.get("longestRunningTurnSec").and_then(Value::as_i64),
            current_streak_days: summary.get("currentStreakDays").and_then(Value::as_i64),
            longest_streak_days: summary.get("longestStreakDays").and_then(Value::as_i64),
        },
        daily_buckets,
        fetched_at,
        source: CodexSnapshotSource::Read,
        freshness: CodexSnapshotFreshness::Fresh,
    }
}

fn string_field(value: &Value, field: &str) -> Option<String> {
    value.get(field).and_then(Value::as_str).map(ToOwned::to_owned)
}

fn required_string(value: &Value, field: &str) -> Result<String, AgentError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| AgentError::bad_gateway(format!("Codex response omitted {field}")))
}

async fn probe_codex_version(program: &std::path::Path) -> Option<String> {
    let mut command = aionui_runtime::Builder::clean_cli(program);
    command.arg("--version");
    let output = tokio::time::timeout(Duration::from_secs(5), command.output())
        .await
        .ok()?
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.split_whitespace().last().map(ToOwned::to_owned))
}

fn map_rpc_error(error: CodexRpcError) -> AgentError {
    match error {
        CodexRpcError::RuntimeUnavailable => AgentError::bad_gateway("RUNTIME_UNAVAILABLE: Codex is unavailable"),
        CodexRpcError::Unsupported => {
            AgentError::bad_gateway("PROTOCOL_UNSUPPORTED: installed Codex does not support WorkMate ChatGPT login")
        }
        CodexRpcError::Request(message) => AgentError::bad_gateway(format!("LOGIN_FAILED: {message}")),
    }
}

fn map_account_read_error(error: CodexRpcError) -> AgentError {
    match error {
        CodexRpcError::Unsupported => {
            AgentError::bad_gateway("PROTOCOL_UNSUPPORTED: installed Codex does not support account/read")
        }
        _ => AgentError::bad_gateway("ACCOUNT_READ_FAILED: unable to read Codex account"),
    }
}

fn redact_auth_error(raw: &str) -> String {
    // Error strings cross into UI/logging, so retain only a generic class when
    // likely secret-bearing material is present. Full auth URLs are never kept.
    let lower = raw.to_ascii_lowercase();
    if lower.contains("bearer ")
        || lower.contains("access_token")
        || lower.contains("refresh_token")
        || lower.contains("id_token")
        || lower.contains("authorization:")
        || lower.contains("code=")
        || (lower.contains("http") && lower.contains("state="))
    {
        "authentication request failed (sensitive details redacted)".into()
    } else {
        raw.chars().take(240).collect()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, VecDeque};

    use aionui_api_types::CodexAuthState;
    use aionui_process::{ManagedProcess, ProcessError};

    use super::*;

    struct FakeRpc {
        responses: Mutex<HashMap<&'static str, VecDeque<Result<Value, CodexRpcError>>>>,
        calls: Mutex<Vec<(&'static str, Value)>>,
        notifications: broadcast::Sender<RpcNotification>,
    }

    impl FakeRpc {
        fn new() -> Arc<Self> {
            let (notifications, _) = broadcast::channel(16);
            Arc::new(Self {
                responses: Mutex::new(HashMap::new()),
                calls: Mutex::new(Vec::new()),
                notifications,
            })
        }

        async fn enqueue(&self, method: &'static str, response: Result<Value, CodexRpcError>) {
            self.responses
                .lock()
                .await
                .entry(method)
                .or_default()
                .push_back(response);
        }

        fn notify(&self, method: &str, params: Value) {
            let _ = self.notifications.send(RpcNotification {
                method: method.into(),
                params,
            });
        }
    }

    #[async_trait]
    impl CodexAccountRpc for FakeRpc {
        async fn call(&self, method: &'static str, params: Value) -> Result<Value, CodexRpcError> {
            self.calls.lock().await.push((method, params));
            self.responses
                .lock()
                .await
                .get_mut(method)
                .and_then(VecDeque::pop_front)
                .unwrap_or(Err(CodexRpcError::Unsupported))
        }

        fn subscribe(&self) -> broadcast::Receiver<RpcNotification> {
            self.notifications.subscribe()
        }
    }

    struct NeverSpawner;

    #[async_trait]
    impl Spawner for NeverSpawner {
        async fn spawn(
            &self,
            _spec: CommandSpec,
            _extra_env: &[(String, String)],
            _opaque_owner_tag: &str,
        ) -> Result<Arc<ManagedProcess>, ProcessError> {
            panic!("preloaded fake RPC must not spawn")
        }
    }

    struct NoopBroadcaster;

    impl EventBroadcaster for NoopBroadcaster {
        fn broadcast(&self, _event: WebSocketMessage<Value>) {}
    }

    fn service(fake: Arc<FakeRpc>) -> Arc<CodexAccountService> {
        Arc::new(CodexAccountService {
            spawner: Arc::new(NeverSpawner),
            program: "codex".into(),
            broadcaster: Arc::new(NoopBroadcaster),
            rpc: Mutex::new(Some(fake)),
            state: Mutex::new(CodexAccountStateMachine::default()),
            raw_rate_limits: RwLock::new(None),
            rate_limits: RwLock::new(None),
            usage: RwLock::new(None),
            diagnostics: RwLock::new(empty_diagnostics()),
            installed_version: RwLock::new(Some("0.160.1".into())),
            warnings: RwLock::new(Vec::new()),
            refresh_gate: Mutex::new(()),
            invalidation_generation: AtomicU64::new(0),
            identity_changed: None,
        })
    }

    #[test]
    fn redacts_tokens_headers_codes_and_authorization_urls() {
        for secret in [
            "Bearer SECRET",
            "access_token=SECRET",
            "refresh_token=SECRET",
            "id_token=SECRET",
            "Authorization: SECRET",
            "https://auth.example/callback?code=SECRET&state=SECRET",
        ] {
            let redacted = redact_auth_error(secret);
            assert!(!redacted.contains("SECRET"), "leaked from {secret}");
        }
    }

    #[tokio::test]
    async fn login_start_uses_managed_chatgpt_and_tracks_login_id() {
        let fake = FakeRpc::new();
        fake.enqueue(
            "account/login/start",
            Ok(json!({"type":"chatgpt","loginId":"login-B","authUrl":"https://auth.example/start?state=x"})),
        )
        .await;
        let service = service(fake.clone());
        let started = service.start_chatgpt_login().await.unwrap();
        assert_eq!(started.account.auth_state, CodexAuthState::Authenticating);
        assert_eq!(started.account.active_login.unwrap().login_id, "login-B");
        let calls = fake.calls.lock().await;
        assert_eq!(calls[0], ("account/login/start", json!({"type":"chatgpt"})));
    }

    #[tokio::test]
    async fn cancel_and_logout_are_followed_by_authoritative_account_read() {
        let fake = FakeRpc::new();
        let service = service(fake.clone());
        service.state.lock().await.begin_login("login-B".into(), 1);
        fake.enqueue("account/login/cancel", Ok(json!({}))).await;
        fake.enqueue("account/read", Ok(json!({"account":null,"requiresOpenaiAuth":true})))
            .await;
        assert_eq!(
            service.cancel_login().await.unwrap().account.auth_state,
            CodexAuthState::SignedOut
        );

        *service.raw_rate_limits.write().await = Some(json!({"rateLimits":{}}));
        *service.rate_limits.write().await = Some(project_rate_limits(
            &json!({"rateLimits":{}}),
            1,
            CodexSnapshotSource::Read,
            CodexSnapshotFreshness::Fresh,
        ));
        *service.usage.write().await = Some(project_account_usage(&json!({"summary":{}}), 1));
        fake.enqueue("account/logout", Ok(json!({}))).await;
        fake.enqueue("account/read", Ok(json!({"account":null,"requiresOpenaiAuth":true})))
            .await;
        let view = service.logout().await.unwrap();
        assert_eq!(view.account.auth_state, CodexAuthState::SignedOut);
        assert!(view.rate_limits.is_none() && view.usage.is_none());
    }

    #[tokio::test]
    async fn optional_usage_failure_does_not_kill_signed_in_account() {
        let fake = FakeRpc::new();
        fake.enqueue(
            "account/read",
            Ok(json!({"account":{"type":"chatgpt","email":"a@example.com","planType":"plus"}})),
        )
        .await;
        let view = service(fake).refresh().await.unwrap();
        assert_eq!(view.account.auth_state, CodexAuthState::SignedIn);
        assert!(
            view.warnings
                .iter()
                .any(|warning| warning.code == CodexAccountErrorCode::RateLimitReadFailed)
        );
        assert!(
            view.warnings
                .iter()
                .any(|warning| warning.code == CodexAccountErrorCode::UsageReadFailed)
        );
    }

    #[tokio::test]
    async fn account_updated_notification_invalidates_then_reads_authoritative_snapshot() {
        let fake = FakeRpc::new();
        fake.enqueue(
            "account/read",
            Ok(json!({"account":{"type":"chatgpt","email":null,"planType":"team"}})),
        )
        .await;
        let service = service(fake.clone());
        // Attach the same notification loop used by the production constructor.
        let mut notifications = fake.subscribe();
        let weak = Arc::downgrade(&service);
        tokio::spawn(async move {
            if let Ok(notification) = notifications.recv().await
                && let Some(service) = weak.upgrade()
            {
                service.handle_notification(notification).await;
            }
        });
        fake.notify("account/updated", json!({"account":null}));
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            service.view().await.unwrap().account.auth_state,
            CodexAuthState::SignedIn
        );
    }

    #[tokio::test]
    async fn required_account_read_failure_enters_error_state() {
        let fake = FakeRpc::new();
        fake.enqueue("account/read", Err(CodexRpcError::Request("network".into())))
            .await;
        let service = service(fake);
        assert!(service.refresh().await.is_err());
        assert_eq!(service.view().await.unwrap().account.auth_state, CodexAuthState::Error);
    }

    #[test]
    fn rate_limit_projection_supports_multiple_buckets_and_preserves_unknown_permission() {
        let projected = project_rate_limits(
            &json!({
                "ordinaryUsageAllowed": null,
                "rateLimits": {"limitId":"codex", "primary":{"usedPercent":10}},
                "rateLimitsByLimitId": {
                    "codex": {"limitId":"codex", "primary":{"usedPercent":10}},
                    "luna": {"limitId":"luna", "secondary":{"usedPercent":100}}
                },
                "rateLimitResetCredits": {"availableCount":1,"credits":[{
                    "id":"credit-1","status":"available","resetType":"codexRateLimits","grantedAt":7
                }]},
                "rateLimitUpsell": {"unknown_future_field":"ignored"}
            }),
            42,
            CodexSnapshotSource::Read,
            CodexSnapshotFreshness::Fresh,
        );
        assert_eq!(projected.ordinary_usage_allowed, None);
        assert_eq!(projected.availability, CodexUsageAvailability::Unknown);
        assert_eq!(projected.buckets.len(), 2);
        assert_eq!(projected.reset_credits.unwrap().available_count, 1);
        let json = serde_json::to_value(projected).unwrap().to_string();
        assert!(!json.contains("rateLimitUpsell") && !json.contains("unknown_future_field"));
    }

    #[test]
    fn explicit_provider_permission_controls_availability_without_percentage_inference() {
        for (allowed, expected) in [
            (Some(false), CodexUsageAvailability::Blocked),
            (Some(true), CodexUsageAvailability::Limited),
            (None, CodexUsageAvailability::Unknown),
        ] {
            let mut raw = json!({"rateLimits":{"primary":{"usedPercent":100}}});
            raw["ordinaryUsageAllowed"] = allowed.map(Value::Bool).unwrap_or(Value::Null);
            assert_eq!(
                project_rate_limits(&raw, 1, CodexSnapshotSource::Read, CodexSnapshotFreshness::Fresh).availability,
                expected
            );
        }
    }

    #[tokio::test]
    async fn sparse_notification_merges_known_fields_then_debounced_read_is_bounded() {
        let fake = FakeRpc::new();
        fake.enqueue(
            "account/read",
            Ok(json!({"account":{"type":"chatgpt","email":"a@example.com","planType":"plus"}})),
        )
        .await;
        fake.enqueue(
            "account/rateLimits/read",
            Ok(json!({"ordinaryUsageAllowed":true,"rateLimits":{"limitId":"codex","limitName":"Codex","primary":{"usedPercent":12}}})),
        )
        .await;
        fake.enqueue("account/usage/read", Ok(json!({"summary":{}}))).await;
        fake.enqueue("server/diagnostics", Ok(json!({"process":{"id":1},"gauges":[]})))
            .await;
        let service = service(fake.clone());
        service.refresh().await.unwrap();

        for used in 20..40 {
            service
                .clone()
                .handle_notification(RpcNotification {
                    method: "account/rateLimits/updated".into(),
                    params: json!({"rateLimits":{"limitName":null,"primary":{"usedPercent":used}}}),
                })
                .await;
        }
        let merged = service.rate_limits.read().await.clone().unwrap();
        assert_eq!(merged.source, CodexSnapshotSource::NotificationMerge);
        assert_eq!(merged.buckets[0].limit_name.as_deref(), Some("Codex"));

        fake.enqueue(
            "account/read",
            Ok(json!({"account":{"type":"chatgpt","email":"a@example.com","planType":"plus"}})),
        )
        .await;
        fake.enqueue(
            "account/rateLimits/read",
            Ok(json!({"ordinaryUsageAllowed":true,"rateLimits":{"limitId":"codex","primary":{"usedPercent":39}}})),
        )
        .await;
        fake.enqueue("account/usage/read", Ok(json!({"summary":{}}))).await;
        fake.enqueue("server/diagnostics", Ok(json!({"process":{"id":1},"gauges":[]})))
            .await;
        tokio::time::sleep(Duration::from_millis(180)).await;
        let calls = fake.calls.lock().await;
        assert_eq!(calls.iter().filter(|(method, _)| *method == "account/read").count(), 2);
    }

    #[tokio::test]
    async fn account_switch_invalidates_optional_snapshots_before_failed_reads() {
        let fake = FakeRpc::new();
        for email in ["a@example.com", "b@example.com"] {
            fake.enqueue(
                "account/read",
                Ok(json!({"account":{"type":"chatgpt","email":email,"planType":"plus"}})),
            )
            .await;
            if email.starts_with('a') {
                fake.enqueue(
                    "account/rateLimits/read",
                    Ok(json!({"ordinaryUsageAllowed":true,"rateLimits":{}})),
                )
                .await;
                fake.enqueue("account/usage/read", Ok(json!({"summary":{"lifetimeTokens":9}})))
                    .await;
            }
            fake.enqueue("server/diagnostics", Ok(json!({"process":{"id":1},"gauges":[]})))
                .await;
        }
        let service = service(fake);
        assert!(service.refresh().await.unwrap().rate_limits.is_some());
        let switched = service.refresh().await.unwrap();
        assert_eq!(
            switched.rate_limits.unwrap().freshness,
            CodexSnapshotFreshness::Unavailable
        );
        assert_eq!(switched.usage.unwrap().freshness, CodexSnapshotFreshness::Unavailable);
        assert_eq!(switched.account.auth_state, CodexAuthState::SignedIn);
    }
}
