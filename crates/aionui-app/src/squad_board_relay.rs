//! M8 E7-6: push the board card of a work item when its linked Squad
//! instance changes run state.
//!
//! The team crate and the conversation (board) crate do not depend on each
//! other, so this composition-layer task listens to the shared event bus for
//! team task/run events, recomputes the linked work item's `squad_run`
//! through the team service, and — only when the `(team_id, state)` pair
//! changed — asks the conversation service to publish `task.board.updated`
//! with `change: "squad_run"` (reusing the E6 board WS event).

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use aionui_api_types::{SquadRunPayload, SquadRunState, WebSocketMessage};
use aionui_conversation::ConversationService;
use aionui_realtime::BroadcastEventBus;
use aionui_team::TeamSessionService;
use aionui_team::events::{
    TEAM_RUN_ACCEPTED_EVENT, TEAM_RUN_CANCELLED_EVENT, TEAM_RUN_COMPLETED_EVENT, TEAM_RUN_FAILED_EVENT,
    TEAM_RUN_STARTED_EVENT, TEAM_RUN_UPDATED_EVENT, TEAM_TASK_CHANGED_EVENT,
};
use tokio::sync::broadcast::error::RecvError;

/// Team events that can change a Squad's derived run state.
const TRIGGERS: [&str; 7] = [
    TEAM_TASK_CHANGED_EVENT,
    TEAM_RUN_ACCEPTED_EVENT,
    TEAM_RUN_STARTED_EVENT,
    TEAM_RUN_UPDATED_EVENT,
    TEAM_RUN_COMPLETED_EVENT,
    TEAM_RUN_CANCELLED_EVENT,
    TEAM_RUN_FAILED_EVENT,
];

/// `(user_id, team_id)` of a team event that may change a Squad's run state.
pub(crate) fn trigger_scope(event: &WebSocketMessage<serde_json::Value>) -> Option<(String, String)> {
    if !TRIGGERS.contains(&event.name.as_str()) {
        return None;
    }
    let user_id = event.data.get("user_id")?.as_str()?.to_owned();
    let team_id = event.data.get("team_id")?.as_str()?.to_owned();
    Some((user_id, team_id))
}

type Seen = Option<(String, SquadRunState)>;

/// Remembers the last pushed `(team_id, state)` per work item so repeated
/// team events (e.g. every `team.runUpdated`) do not spam the board.
#[derive(Default)]
pub(crate) struct LastPushed(HashMap<(String, String), Seen>);

impl LastPushed {
    /// `true` when `run` differs from what was last pushed for the item
    /// (first sighting always pushes).
    pub(crate) fn changed(&mut self, user_id: &str, work_item_id: &str, run: Option<&SquadRunPayload>) -> bool {
        let next: Seen = run.map(|r| (r.team_id.clone(), r.state));
        let key = (user_id.to_owned(), work_item_id.to_owned());
        if self.0.get(&key) == Some(&next) {
            return false;
        }
        self.0.insert(key, next);
        true
    }
}

pub(crate) fn spawn(bus: &BroadcastEventBus, teams: Weak<TeamSessionService>, conversations: ConversationService) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        let mut last = LastPushed::default();
        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "squad board relay lagged");
                    continue;
                }
                Err(RecvError::Closed) => break,
            };
            let Some((user_id, team_id)) = trigger_scope(&event) else {
                continue;
            };
            let Some(teams) = teams.upgrade() else { break };
            relay_one(&teams, &conversations, &mut last, &user_id, &team_id).await;
        }
    });
}

async fn relay_one(
    teams: &Arc<TeamSessionService>,
    conversations: &ConversationService,
    last: &mut LastPushed,
    user_id: &str,
    team_id: &str,
) {
    let (work_item_id, run) = match teams.linked_work_item_squad_run(user_id, team_id).await {
        Ok(Some(linked)) => linked,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(team_id, %error, "squad board relay: squad_run lookup failed");
            return;
        }
    };
    // Only the newest linked instance drives the card.
    if run.as_ref().is_some_and(|r| r.team_id != team_id) {
        return;
    }
    if !last.changed(user_id, &work_item_id, run.as_ref()) {
        return;
    }
    if let Err(error) = conversations
        .publish_squad_run_changed(user_id, &work_item_id, run)
        .await
    {
        tracing::warn!(%work_item_id, %error, "squad board relay: publish failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(team: &str, state: SquadRunState) -> SquadRunPayload {
        SquadRunPayload {
            team_id: team.into(),
            state,
            leader_conversation_id: None,
            leader_mode: None,
            updated_at: 1,
        }
    }

    #[test]
    fn only_team_task_and_run_events_with_scope_trigger() {
        let ev = |name: &str, data| WebSocketMessage::new(name, data);
        assert_eq!(
            trigger_scope(&ev("team.runUpdated", json!({"user_id": "u", "team_id": "t"}))),
            Some(("u".into(), "t".into()))
        );
        assert_eq!(
            trigger_scope(&ev("team.taskChanged", json!({"user_id": "u", "team_id": "t"}))),
            Some(("u".into(), "t".into()))
        );
        assert_eq!(
            trigger_scope(&ev("team.agentSpawned", json!({"user_id": "u", "team_id": "t"}))),
            None
        );
        assert_eq!(trigger_scope(&ev("task.board.updated", json!({"user_id": "u"}))), None);
        assert_eq!(trigger_scope(&ev("team.runFailed", json!({"team_id": "t"}))), None);
    }

    #[test]
    fn pushes_only_on_state_or_instance_change() {
        let mut last = LastPushed::default();
        let running = run("t1", SquadRunState::Running);
        assert!(last.changed("u", "w", Some(&running)));
        assert!(!last.changed("u", "w", Some(&running)), "same state is deduped");
        assert!(last.changed("u", "w", Some(&run("t1", SquadRunState::Finished))));
        assert!(
            last.changed("u", "w", Some(&run("t2", SquadRunState::Finished))),
            "new instance"
        );
        assert!(last.changed("u", "w", None), "cleared");
        assert!(!last.changed("u", "w", None));
        assert!(last.changed("other", "w", None), "per user");
    }
}
