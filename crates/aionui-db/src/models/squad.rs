//! Squad templates (M8 E7-1, migration 074).

use aionui_common::TimestampMs;
use serde::{Deserialize, Serialize};

pub const SQUAD_LEADER_MODES: &[&str] = &["route_only", "route_and_work"];
pub const SQUAD_VISIBILITIES: &[&str] = &["private", "team"];

/// Row mapping for `squad_templates`. `members` is the raw JSON array text.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct SquadTemplateRow {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub description: String,
    pub leader_assistant_id: String,
    pub leader_mode: String,
    pub members: String,
    pub visibility: String,
    pub team_id: Option<String>,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
    pub deleted_at: Option<TimestampMs>,
}
