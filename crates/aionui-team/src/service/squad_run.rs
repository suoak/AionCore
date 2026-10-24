//! M8 E7-5: `GET /api/teams/{id}/squad-run`. The state is derived only from
//! data Core already has — the team task board and the in-memory team-run
//! manager — so nothing new is persisted.

use aionui_api_types::{SquadRunPayload, SquadRunState, TeamRunStatus};

use super::*;
use crate::types::{TaskStatus, TeammateRole};

/// Pure derivation (unit-tested). `latest_run` is the status of the session's
/// current/most recent team run, `None` when there is no session or no run.
/// Returns `None` when the Squad has neither tasks nor a run (nothing to show).
pub fn derive_squad_run_state(tasks: &[TaskStatus], latest_run: Option<TeamRunStatus>) -> Option<SquadRunState> {
    if matches!(
        latest_run,
        Some(TeamRunStatus::Accepted | TeamRunStatus::Running | TeamRunStatus::Cancelling)
    ) {
        return Some(SquadRunState::Running);
    }
    let live: Vec<TaskStatus> = tasks.iter().copied().filter(|s| *s != TaskStatus::Deleted).collect();
    if latest_run == Some(TeamRunStatus::Failed) {
        return Some(SquadRunState::Failed);
    }
    if live.is_empty() {
        // A finished/cancelled run without tasks still counts as a run.
        return match latest_run {
            Some(TeamRunStatus::Completed) => Some(SquadRunState::Finished),
            Some(TeamRunStatus::Cancelled) => Some(SquadRunState::Paused),
            _ => None,
        };
    }
    if live.iter().all(|s| *s == TaskStatus::Completed) {
        return Some(SquadRunState::Finished);
    }
    let any_started = live
        .iter()
        .any(|s| matches!(s, TaskStatus::InProgress | TaskStatus::Completed));
    if !any_started && latest_run.is_none() {
        return Some(SquadRunState::WaitingBacklog);
    }
    Some(SquadRunState::Paused)
}

impl TeamSessionService {
    pub async fn get_squad_run(&self, user_id: &str, team_id: &str) -> Result<Option<SquadRunPayload>, TeamError> {
        let team = self.load_owned_team(user_id, team_id).await?;
        if self.repo.team_squad_template_id(team_id).await?.is_none() {
            return Err(TeamError::TeamNotSquad(team_id.to_owned()));
        }
        let leader_mode = self.repo.team_squad_leader_mode(team_id).await?;
        self.squad_run_for_team(user_id, &team, leader_mode).await
    }

    /// Shared by `GET /squad-run` and the embedded `TeamResponse.squad_run`.
    /// Caller has already checked that `team` is a Squad instance owned by
    /// `user_id`.
    pub(crate) async fn squad_run_for_team(
        &self,
        user_id: &str,
        team: &Team,
        leader_mode: Option<String>,
    ) -> Result<Option<SquadRunPayload>, TeamError> {
        let team_id = team.id.as_str();
        let rows = self.repo.list_tasks(user_id, team_id).await?;
        let tasks: Vec<TeamTask> = rows.iter().filter_map(|r| TeamTask::from_row(r).ok()).collect();
        let latest_run = self.sessions.get(team_id).and_then(|entry| {
            let session = Arc::clone(&entry.session);
            let snapshot = session.work_coordinator().snapshot();
            session
                .team_run_manager()
                .current_payload(&snapshot)
                .map(|run| run.status)
        });
        let statuses: Vec<TaskStatus> = tasks.iter().map(|t| t.status).collect();
        let Some(state) = derive_squad_run_state(&statuses, latest_run) else {
            return Ok(None);
        };
        let updated_at = tasks.iter().map(|t| t.updated_at).max().unwrap_or(team.updated_at);
        let leader_conversation_id = team
            .agents
            .iter()
            .find(|a| a.role == TeammateRole::Lead)
            .map(|a| a.conversation_id.clone());
        Ok(Some(SquadRunPayload {
            team_id: team_id.to_owned(),
            state,
            leader_conversation_id,
            leader_mode,
            updated_at,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::derive_squad_run_state as derive;
    use crate::types::TaskStatus::*;
    use aionui_api_types::{SquadRunState as S, TeamRunStatus as R};

    #[test]
    fn active_run_is_running_regardless_of_tasks() {
        for run in [R::Accepted, R::Running, R::Cancelling] {
            assert_eq!(derive(&[Completed], Some(run.clone())), Some(S::Running));
            assert_eq!(derive(&[], Some(run)), Some(S::Running));
        }
    }

    #[test]
    fn nothing_yet_is_none() {
        assert_eq!(derive(&[], None), None);
        assert_eq!(derive(&[Deleted], None), None);
    }

    #[test]
    fn only_pending_without_run_waits_on_backlog() {
        assert_eq!(derive(&[Pending, Pending, Deleted], None), Some(S::WaitingBacklog));
    }

    #[test]
    fn all_completed_is_finished() {
        assert_eq!(derive(&[Completed, Deleted], None), Some(S::Finished));
        assert_eq!(derive(&[Completed], Some(R::Completed)), Some(S::Finished));
        assert_eq!(derive(&[], Some(R::Completed)), Some(S::Finished));
    }

    #[test]
    fn open_work_without_active_run_is_paused() {
        assert_eq!(derive(&[InProgress], None), Some(S::Paused));
        assert_eq!(derive(&[Completed, Pending], None), Some(S::Paused));
        assert_eq!(derive(&[Pending], Some(R::Completed)), Some(S::Paused));
        assert_eq!(derive(&[Pending], Some(R::Cancelled)), Some(S::Paused));
        assert_eq!(derive(&[], Some(R::Cancelled)), Some(S::Paused));
    }

    #[test]
    fn failed_latest_run_is_failed() {
        assert_eq!(derive(&[Pending], Some(R::Failed)), Some(S::Failed));
        assert_eq!(derive(&[Completed], Some(R::Failed)), Some(S::Failed));
    }
}
