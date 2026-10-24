# ADR 0001: The work-item board and the Team task board stay separate

- Status: Accepted
- Date: 2026-10-24
- Scope: M8 E6 (task board), E7 (Squad)

## Context

M8 E6 adds a kanban board for work items. A work item is a `task_sessions` row
with the board columns added in E6-2: `board_status`, `assignee_kind/id`,
`priority`, `labels`, `board_rank`, and `external_ref`. Its timeline lives in
`task_comments`.

The Team runtime already has its own task list, `team_tasks`. A Team leader
uses it to break a goal into sub-tasks for team members, with an owner,
`blocked_by`, and forward-only status.

The two look alike, so the obvious question is whether to merge them into one
table or one board.

## Decision

1. **The tables are not merged.** Work items stay in `task_sessions`, and
   Team sub-tasks stay in `team_tasks`. Neither schema gains a column for the
   other.
2. **The link is one-way, from Team to board.**
   - When a work item is assigned to a Squad (E7), the leader splits it into
     `team_tasks` inside the team.
   - Each sub-task points back through `team_tasks.metadata.work_item_id`.
   - `task_sessions` holds no list of sub-task ids.
3. **The board shows work items only.** The detail page has a "sub-tasks" tab
   that lists the linked `team_tasks` **read-only**. The board never edits,
   moves, or reviews a Team sub-task.
4. **What is shared is semantics, not storage.** These ideas carry over:
   - an owner (assignee) per item
   - blocking relations
   - status that only moves forward through the agent path
5. **Each side keeps its own state machine.**
   - On the board, the user owns every column change except one: an agent may
     hand an item over from `in_progress` to `in_review`. `done` is reachable
     only through `POST /api/task-sessions/{id}/review`, which needs a user
     credential.
   - Team tasks keep their existing leader-driven rules.

## Consequences

- E6 ships without touching the Team runtime or its migrations, and the Team
  task board keeps working unchanged.
- A work item's progress is not derived automatically from its sub-tasks. When
  the leader thinks the Squad's work is finished, it moves the work item to
  `in_review`. A user still has to approve it.
- To show sub-tasks, the server reads `team_tasks` filtered by
  `metadata.work_item_id`. That query belongs to E7 and is not part of E6-3.
- If both sides later need one view, a read model can join them. The tables
  stay separate.

## Alternatives considered

- **One table for work items and Team sub-tasks.** Rejected for three reasons:
  - It mixes user-owned review rules with leader-owned execution rules.
  - It needs a migration of live Team data.
  - It couples two release lines.
- **A two-way link** (sub-task ids stored on the work item). Rejected because it
  creates two sources of truth that can drift apart. The single back-reference
  is enough.
