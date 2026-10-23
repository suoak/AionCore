//! `scm/commit`, `scm/push` and `scm/remoteInfo` on the actor (M7 E4-4).
//!
//! Commit and push are the first `scm/*` methods that can run for minutes — a
//! repository's pre-commit hook, a slow remote — so they do not run on the
//! actor loop. The loop authorizes and validates synchronously, then hands the
//! work to a task that replies on its own; status refreshes, diffs and other
//! repositories' traffic keep flowing meanwhile. The repository's action lock
//! is deliberately not held across the git process: a status computed mid-commit
//! is merely early, and the refresh after completion publishes the result.
//!
//! One commit-or-push per repository at a time: a second request while one is
//! running is refused (`busy`) rather than queued behind it.
//!
//! Audit: `scm.committed` / `scm.pushed` structured log events. These methods
//! are reachable only over the user's `/ws` connection; an agent runtime token
//! cannot open it, so agents cannot commit or push through WorkMate.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use crate::types::FileOp;

use super::super::commit_types::{CommitRequest, PushRequest, ScmProgress, ScmProgressFn};
use super::super::error::ScmError;
use super::super::provider::IScmProvider;
use super::super::runtime::ScmRuntime;
use super::super::types::RepoRef;
use super::super::wire;
use super::{ScmActor, ScmWirePush};

/// Repositories with a commit or push in flight.
#[derive(Clone, Default)]
pub(super) struct InFlight(Arc<Mutex<HashSet<String>>>);

impl InFlight {
    fn acquire(&self, repo_id: &str) -> Result<InFlightGuard, ScmError> {
        let mut set = self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if !set.insert(repo_id.to_owned()) {
            return Err(ScmError::OperationFailed {
                context: "busy",
                message: "a commit or push is already running for this repository".into(),
            });
        }
        Ok(InFlightGuard {
            set: Arc::clone(&self.0),
            repo_id: repo_id.to_owned(),
        })
    }
}

/// Releases the repository when the task ends, however it ends.
struct InFlightGuard {
    set: Arc<Mutex<HashSet<String>>>,
    repo_id: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.set
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&self.repo_id);
    }
}

impl ScmActor {
    pub(super) async fn start_commit(&self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let prepared = async {
            let repo = Self::repo_of(&params)?;
            let request: CommitRequest =
                serde_json::from_value(params.clone()).map_err(|_| ScmError::InvalidParams { what: "message" })?;
            self.authorize(user_id, &repo, FileOp::Write).await?;
            if self.runtime.provider().commit().is_none() {
                return Err(ScmError::CapabilityUnsupported { capability: "commit" });
            }
            let guard = self.publishing.acquire(&repo.repo_id)?;
            Ok((repo, request, guard))
        }
        .await;
        let (repo, request, guard) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return self.push.push(session, wire::error_from(id, &err)),
        };

        let runtime = Arc::clone(&self.runtime);
        let push = Arc::clone(&self.push);
        let session = session.to_owned();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            let _guard = guard;
            let outcome = match runtime.provider().commit() {
                Some(committer) => committer.commit(&repo, &request).await,
                None => Err(ScmError::CapabilityUnsupported { capability: "commit" }),
            };
            match outcome {
                Ok(done) => {
                    tracing::info!(
                        event = "scm.committed",
                        user_id = %user_id,
                        repo_id = %repo.repo_id,
                        commit_sha = %done.commit_sha,
                        signoff = request.signoff,
                        no_verify = request.no_verify,
                        "scm commit"
                    );
                    push.push(&session, wire::success(id, json!(done)));
                }
                Err(err) => push.push(&session, wire::error_from(id, &err)),
            }
            // A failed hook may still have rewritten files; publish either way.
            refresh_and_broadcast(&runtime, push.as_ref(), &repo).await;
        });
    }

    pub(super) async fn start_push(&self, session: &str, user_id: &str, id: Option<Value>, params: Value) {
        let prepared = async {
            let repo = Self::repo_of(&params)?;
            let request: PushRequest =
                serde_json::from_value(params.clone()).map_err(|_| ScmError::InvalidParams { what: "remote" })?;
            self.authorize(user_id, &repo, FileOp::Write).await?;
            if self.runtime.provider().remote().is_none() {
                return Err(ScmError::CapabilityUnsupported {
                    capability: "remote_ops",
                });
            }
            let guard = self.publishing.acquire(&repo.repo_id)?;
            Ok((repo, request, guard))
        }
        .await;
        let (repo, request, guard) = match prepared {
            Ok(prepared) => prepared,
            Err(err) => return self.push.push(session, wire::error_from(id, &err)),
        };

        let runtime = Arc::clone(&self.runtime);
        let push = Arc::clone(&self.push);
        let session = session.to_owned();
        let user_id = user_id.to_owned();
        tokio::spawn(async move {
            let _guard = guard;
            // Progress goes to the requesting connection only: it is that
            // user's button that is spinning.
            let progress: ScmProgressFn = {
                let push = Arc::clone(&push);
                let session = session.clone();
                let repo_id = repo.repo_id.clone();
                Arc::new(move |step: ScmProgress| {
                    push.push(&session, wire::progress(&repo_id, &step.phase, step.percent));
                })
            };
            let outcome = match runtime.provider().remote() {
                Some(remote) => remote.push(&repo, &request, Some(progress)).await,
                None => Err(ScmError::CapabilityUnsupported {
                    capability: "remote_ops",
                }),
            };
            match outcome {
                Ok(done) => {
                    tracing::info!(
                        event = "scm.pushed",
                        user_id = %user_id,
                        repo_id = %repo.repo_id,
                        remote = %done.remote,
                        branch = %done.branch,
                        upstream = %done.upstream,
                        "scm push"
                    );
                    push.push(&session, wire::success(id, json!(done)));
                    // The branch's ahead / behind changed.
                    refresh_and_broadcast(&runtime, push.as_ref(), &repo).await;
                }
                Err(err) => {
                    tracing::info!(
                        event = "scm.push_failed",
                        user_id = %user_id,
                        repo_id = %repo.repo_id,
                        error = wire::error_name(&err),
                        "scm push failed"
                    );
                    push.push(&session, wire::error_from(id, &err));
                }
            }
        });
    }

    pub(super) async fn remote_info(&self, user_id: &str, params: Value) -> Result<Value, ScmError> {
        let repo = Self::repo_of(&params)?;
        self.authorize(user_id, &repo, FileOp::Read).await?;
        let remote = self
            .runtime
            .provider()
            .remote()
            .ok_or(ScmError::CapabilityUnsupported {
                capability: "remote_ops",
            })?;
        let info = remote.remote_info(&repo).await?;
        Ok(serde_json::to_value(&info).unwrap_or_else(|_| json!({})))
    }
}

/// Recompute a repository and push the frame to its subscribers — the actor's
/// `broadcast`, usable from a task that does not hold the actor.
async fn refresh_and_broadcast(runtime: &ScmRuntime, push: &dyn ScmWirePush, repo: &RepoRef) {
    match runtime.refresh(repo).await {
        Ok(status) => {
            let frame = wire::notification(
                "scm/statusChanged",
                serde_json::to_value(&status).unwrap_or_else(|_| json!({})),
            );
            for session in runtime.subscribers_of(&repo.repo_id).await {
                push.push(&session, frame.clone());
            }
        }
        Err(err) => tracing::debug!(repo_id = %repo.repo_id, error = %err, "scm refresh after publish failed"),
    }
}
