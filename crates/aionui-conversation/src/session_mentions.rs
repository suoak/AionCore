//! Resolve `@@` conversation references at the send boundary.
//!
//! Mirrors `[[AION_FILES]]` (`aionui-project/src/chat_files.rs`): the block is
//! appended to the message content and persisted verbatim, so persistence,
//! broadcast, and agent input all see the same bytes.

use aionui_api_types::TeamSessionBinding;
use aionui_common::constants::{AIONUI_SESSIONS_END_MARKER, AIONUI_SESSIONS_MARKER};

use crate::error::ConversationError;

/// One `@@` target, resolved from its id by the backend.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMentionTargetInfo {
    pub id: String,
    pub name: String,
    /// Absolute workspace path, when the conversation row records one.
    pub workspace: Option<String>,
}

/// The `workspace:` field value for one target.
///
/// One field with a conditional value rather than `workspace` +
/// `same_workspace`: when the workspaces match, the absolute path carries no
/// information (the agent knows its own cwd). When they differ, the constraint
/// travels inside the value so it does not depend on the skill being read.
///
/// An unknown target workspace must render as `unknown`, never `same`:
/// collapsing it to `same` would tell the agent relative paths are safe when
/// we do not know that, which is the exact silent-misread failure this field
/// exists to prevent.
pub fn workspace_field_value(sender_workspace: Option<&str>, target_workspace: Option<&str>) -> String {
    match (sender_workspace, target_workspace) {
        (Some(sender), Some(target)) if sender == target => "same".to_owned(),
        (_, Some(target)) => format!("{target} (differs from yours)"),
        (_, None) => "unknown (differs from yours)".to_owned(),
    }
}

/// The one routing hint the block carries: it names the `session-message` skill
/// to deliver with, and adds an UNCONDITIONAL `session capabilities` fallback for
/// when that skill is unavailable (the user can uncheck the auto-inject skill in
/// the assistant editor). It still carries no `send-message` payload command
/// template — the skill body stays the single source of that payload shape, so
/// the block and the skill cannot drift. `session capabilities` is different: it
/// is a self-describing, descriptor-generated discovery command (not a payload
/// template), so pointing at it introduces nothing that can drift.
///
/// It is emitted as the FIRST in-marker line on purpose. The frontend's
/// `parseSessionsBlock` (`sessionMarkers.ts`) renders only the text OUTSIDE the
/// markers and turns each in-marker line into a chip only when it splits into
/// `name \t id \t workspace`. This line has no tab, so it is dropped from the
/// sender's bubble AND skipped by the chip parser — while every agent backend
/// still receives it verbatim in the message content.
///
/// English on purpose. Everything between the markers is agent-facing: the
/// frontend strips this whole block from the displayed text and re-renders the
/// human-facing chips/labels separately (via i18n), so the block's own bytes are
/// read only by the agent. The block is therefore written entirely in English —
/// including the `workspace:` warning below — to match the `session-message`
/// skill it routes to.
const SESSIONS_BLOCK_SKILL_HINT: &str = "To deliver to the conversations below, use the session-message skill (address by conversation id); if it is unavailable, run `\"$AIONUI_HELPER_BIN\" session capabilities` for the delivery contract.";

/// Build the sender-side block.
///
/// The block is agent-facing and written in English. Spec §8.3 originally kept it
/// instruction-free and left sending to the auto-inject skill. That relied on the
/// agent loading the skill's body before choosing how to deliver, which is not
/// guaranteed: a competing built-in cross-session tool can be reached first, in
/// `Injected` skill-delivery mode the body is lazy-loaded, and the user can
/// uncheck the skill entirely. So the block now carries ONE routing hint
/// ([`SESSIONS_BLOCK_SKILL_HINT`]) that names the `session-message` skill and
/// adds an unconditional `session capabilities` fallback for when it is
/// unavailable — but still no `send-message` payload command template, keeping
/// §8.3's real invariant (the payload shape lives only in the skill body) intact.
pub fn build_sessions_block(sender_workspace: Option<&str>, targets: &[SessionMentionTargetInfo]) -> String {
    let mut block = String::from(AIONUI_SESSIONS_MARKER);
    block.push('\n');
    block.push_str(SESSIONS_BLOCK_SKILL_HINT);
    for target in targets {
        block.push('\n');
        block.push_str(&target.name);
        block.push('\t');
        block.push_str(&target.id);
        block.push_str("\tworkspace: ");
        block.push_str(&workspace_field_value(sender_workspace, target.workspace.as_deref()));
    }
    block.push('\n');
    block.push_str(AIONUI_SESSIONS_END_MARKER);
    block
}

/// Read `extra.workspace` out of a conversation row's raw `extra` JSON string.
pub fn workspace_from_extra(extra: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(extra).ok()?;
    value
        .get("workspace")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

/// `Some(team_id)` when the row is team-owned. Key is `teamId` (camelCase).
///
/// `pub` rather than `pub(crate)`: `aionui-session-message` reuses it so the
/// team hard-filter and the delivery-time team check cannot drift from the
/// send boundary's.
pub fn team_id_from_extra_str(extra: &str) -> Option<String> {
    TeamSessionBinding::team_id_marker_from_extra_str(extra)
}

/// Reject a reference that must not be usable as a `@@` target.
///
/// Pass an empty `sender_conversation_id` to skip the self-reference check —
/// the `@@` picker already excludes the current conversation (spec §5.3), and
/// the CLI side re-checks it as `target_is_self`.
pub fn reject_unusable_target(
    sender_conversation_id: &str,
    target_id: &str,
    target_extra: &str,
) -> Result<(), ConversationError> {
    if !sender_conversation_id.is_empty() && target_id == sender_conversation_id {
        return Err(ConversationError::BadRequest {
            reason: format!("@@ reference targets the current conversation: {target_id}"),
        });
    }
    if team_id_from_extra_str(target_extra).is_some() {
        return Err(ConversationError::Forbidden {
            reason: format!("@@ reference targets a team-owned conversation: {target_id}"),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "session_mentions_test.rs"]
mod session_mentions_test;

/// Most targets one conversation may hold grants for; older grants beyond this
/// are dropped first. Bounds memory for a conversation that `@@`s a lot.
const MAX_GRANTS_PER_CONVERSATION: usize = 256;

/// Which conversations an agent may deliver to from a given conversation.
///
/// Cross-session delivery (`session send-message`) is agent-initiated, so a
/// prompt-injected agent could otherwise message ANY of the user's
/// conversations and start a turn there. A grant is created only by:
///
/// - the user's own send path, for every conversation they picked with `@@`
///   in that conversation (`ConversationService::send_message`);
/// - a successful delivery A → B, which grants B the reply edge B → A (the
///   user authorized that pair; it never extends to third conversations).
///
/// Persistence (E1 tail ③): when a store is attached
/// ([`SessionMentionGrants::set_store`]), grants are written through to the
/// `session_mention_grants` table and reloaded at startup
/// ([`SessionMentionGrants::load_persisted`]), so a restart no longer forces
/// the user to re-select targets. Only these server-side records are trusted;
/// `@@` text in history is never re-parsed into grants. Without a store
/// (tests, or a failed load) grants stay in memory and fail closed on restart.
#[derive(Debug, Default)]
pub struct SessionMentionGrants {
    inner: std::sync::Mutex<std::collections::HashMap<(String, String), Vec<String>>>,
    store: std::sync::OnceLock<sqlx::SqlitePool>,
}

impl SessionMentionGrants {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<(String, String), Vec<String>>> {
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Attach the database used to persist grants. Later calls are ignored.
    pub fn set_store(&self, pool: sqlx::SqlitePool) {
        let _ = self.store.set(pool);
    }

    /// Load persisted grants into memory (oldest first, so the per-conversation
    /// cap keeps the newest). Call once at startup after [`Self::set_store`].
    pub async fn load_persisted(&self) -> Result<usize, sqlx::Error> {
        let Some(pool) = self.store.get() else {
            return Ok(0);
        };
        let rows: Vec<(String, String, String)> = sqlx::query_as(
            "SELECT user_id, from_conversation_id, to_conversation_id FROM session_mention_grants \
             ORDER BY created_at ASC, rowid ASC",
        )
        .fetch_all(pool)
        .await?;
        let count = rows.len();
        for (user_id, from, to) in rows {
            self.grant_in_memory(&user_id, &from, &to);
        }
        Ok(count)
    }

    /// Run a persistence statement off the caller's path; failures are logged
    /// and leave the in-memory grant intact (it then simply won't survive a
    /// restart, which fails closed).
    fn persist(&self, what: &'static str, op: impl FnOnce(sqlx::SqlitePool) -> PersistFuture + Send + 'static) {
        let Some(pool) = self.store.get().cloned() else {
            return;
        };
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            if let Err(error) = op(pool).await {
                tracing::warn!(%error, what, "persisting @@ session grant failed");
            }
        });
    }

    /// Update memory only; returns targets evicted by the per-conversation cap.
    fn grant_in_memory(&self, user_id: &str, from: &str, to: &str) -> Vec<String> {
        let mut map = self.lock();
        let targets = map.entry((user_id.to_owned(), from.to_owned())).or_default();
        targets.retain(|existing| existing != to);
        targets.push(to.to_owned());
        if targets.len() > MAX_GRANTS_PER_CONVERSATION {
            let excess = targets.len() - MAX_GRANTS_PER_CONVERSATION;
            return targets.drain(..excess).collect();
        }
        Vec::new()
    }

    /// Allow `from` to deliver to `to` for `user_id`.
    pub fn grant(&self, user_id: &str, from: &str, to: &str) {
        if from == to || from.is_empty() || to.is_empty() {
            return;
        }
        let evicted = self.grant_in_memory(user_id, from, to);
        let (user_id, from, to) = (user_id.to_owned(), from.to_owned(), to.to_owned());
        self.persist("grant", move |pool| {
            Box::pin(async move {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or_default();
                let mut tx = pool.begin().await?;
                sqlx::query(
                    "INSERT INTO session_mention_grants (user_id, from_conversation_id, to_conversation_id, created_at) \
                     VALUES (?, ?, ?, ?) \
                     ON CONFLICT (user_id, from_conversation_id, to_conversation_id) DO UPDATE SET created_at = excluded.created_at",
                )
                .bind(&user_id)
                .bind(&from)
                .bind(&to)
                .bind(now)
                .execute(&mut *tx)
                .await?;
                for target in evicted {
                    sqlx::query(
                        "DELETE FROM session_mention_grants \
                         WHERE user_id = ? AND from_conversation_id = ? AND to_conversation_id = ?",
                    )
                    .bind(&user_id)
                    .bind(&from)
                    .bind(&target)
                    .execute(&mut *tx)
                    .await?;
                }
                tx.commit().await
            })
        });
    }

    pub fn is_granted(&self, user_id: &str, from: &str, to: &str) -> bool {
        self.lock()
            .get(&(user_id.to_owned(), from.to_owned()))
            .is_some_and(|targets| targets.iter().any(|target| target == to))
    }

    /// Drop every grant from or to a deleted conversation.
    pub fn forget_conversation(&self, user_id: &str, conversation_id: &str) {
        {
            let mut map = self.lock();
            map.remove(&(user_id.to_owned(), conversation_id.to_owned()));
            for ((owner, _), targets) in map.iter_mut() {
                if owner == user_id {
                    targets.retain(|target| target != conversation_id);
                }
            }
        }
        let (user_id, conversation_id) = (user_id.to_owned(), conversation_id.to_owned());
        self.persist("forget", move |pool| {
            Box::pin(async move {
                sqlx::query(
                    "DELETE FROM session_mention_grants \
                     WHERE user_id = ? AND (from_conversation_id = ? OR to_conversation_id = ?)",
                )
                .bind(&user_id)
                .bind(&conversation_id)
                .bind(&conversation_id)
                .execute(&pool)
                .await
                .map(|_| ())
            })
        });
    }
}

type PersistFuture = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), sqlx::Error>> + Send>>;

#[cfg(test)]
mod grant_tests {
    use super::SessionMentionGrants;

    #[test]
    fn grants_are_directional_and_scoped_per_user() {
        let grants = SessionMentionGrants::new();
        grants.grant("u1", "a", "b");
        assert!(grants.is_granted("u1", "a", "b"));
        assert!(!grants.is_granted("u1", "b", "a"));
        assert!(!grants.is_granted("u2", "a", "b"));
        assert!(!grants.is_granted("u1", "a", "c"));
    }

    #[test]
    fn self_grants_are_ignored_and_deletion_revokes() {
        let grants = SessionMentionGrants::new();
        grants.grant("u1", "a", "a");
        assert!(!grants.is_granted("u1", "a", "a"));
        grants.grant("u1", "a", "b");
        grants.grant("u1", "c", "b");
        grants.forget_conversation("u1", "b");
        assert!(!grants.is_granted("u1", "a", "b"));
        assert!(!grants.is_granted("u1", "c", "b"));
    }

    #[test]
    fn grants_are_bounded_per_conversation() {
        let grants = SessionMentionGrants::new();
        for index in 0..(super::MAX_GRANTS_PER_CONVERSATION + 5) {
            grants.grant("u1", "a", &format!("t{index}"));
        }
        assert!(!grants.is_granted("u1", "a", "t0"));
        assert!(grants.is_granted("u1", "a", &format!("t{}", super::MAX_GRANTS_PER_CONVERSATION + 4)));
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::SessionMentionGrants;

    async fn wait_for_rows(pool: &sqlx::SqlitePool, expected: i64) {
        for _ in 0..200 {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_mention_grants")
                .fetch_one(pool)
                .await
                .unwrap();
            if n == expected {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("session_mention_grants never reached {expected} rows");
    }

    #[tokio::test]
    async fn grants_survive_a_restart_and_forget_removes_them() {
        let db = aionui_db::init_database_memory().await.unwrap();
        let pool = db.pool().clone();

        let before = SessionMentionGrants::new();
        before.set_store(pool.clone());
        before.grant("u1", "a", "b");
        before.grant("u1", "a", "c");
        before.grant("u2", "x", "y");
        wait_for_rows(&pool, 3).await;

        // "Restart": a fresh instance reloads from the table.
        let after = SessionMentionGrants::new();
        after.set_store(pool.clone());
        assert_eq!(after.load_persisted().await.unwrap(), 3);
        assert!(after.is_granted("u1", "a", "b"));
        assert!(after.is_granted("u1", "a", "c"));
        assert!(after.is_granted("u2", "x", "y"));
        assert!(!after.is_granted("u1", "b", "a"));
        assert!(!after.is_granted("u2", "a", "b"));

        after.forget_conversation("u1", "c");
        wait_for_rows(&pool, 2).await;
        let again = SessionMentionGrants::new();
        again.set_store(pool);
        again.load_persisted().await.unwrap();
        assert!(again.is_granted("u1", "a", "b"));
        assert!(!again.is_granted("u1", "a", "c"));
    }

    #[tokio::test]
    async fn without_a_store_grants_stay_in_memory() {
        let grants = SessionMentionGrants::new();
        assert_eq!(grants.load_persisted().await.unwrap(), 0);
        grants.grant("u1", "a", "b");
        assert!(grants.is_granted("u1", "a", "b"));
    }
}
