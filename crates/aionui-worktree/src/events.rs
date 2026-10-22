//! `worktree.progress` / `worktree.changed` realtime events (same envelope as
//! `conversation.listChanged`; routed per user via `user_id`).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aionui_api_types::WebSocketMessage;
use aionui_realtime::EventBroadcaster;
use serde_json::json;

pub const EVENT_PROGRESS: &str = "worktree.progress";
pub const EVENT_CHANGED: &str = "worktree.changed";
pub const PROGRESS_THROTTLE: Duration = Duration::from_millis(250);

#[derive(Clone, Default)]
pub struct WorktreeEvents {
    broadcaster: Option<Arc<dyn EventBroadcaster>>,
    last_progress: Arc<Mutex<HashMap<String, (Instant, String)>>>,
}

impl WorktreeEvents {
    pub fn new(broadcaster: Arc<dyn EventBroadcaster>) -> Self {
        Self {
            broadcaster: Some(broadcaster),
            last_progress: Arc::default(),
        }
    }

    /// Silent emitter (tests, headless tools).
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Throttled to one event per 250ms per worktree, except that a phase
    /// change or 100% always goes out.
    pub fn progress(&self, user_id: &str, worktree_id: &str, phase: &str, percent: Option<u8>, message: &str) {
        {
            let mut last = self.last_progress.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            if let Some((at, last_phase)) = last.get(worktree_id)
                && last_phase == phase
                && percent != Some(100)
                && now.duration_since(*at) < PROGRESS_THROTTLE
            {
                return;
            }
            last.insert(worktree_id.to_owned(), (now, phase.to_owned()));
        }
        self.send(
            EVENT_PROGRESS,
            json!({
                "user_id": user_id,
                "worktree_id": worktree_id,
                "phase": phase,
                "percent": percent,
                "message": message,
            }),
        );
    }

    pub fn changed(&self, user_id: &str, worktree_id: &str, status: &str) {
        self.last_progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(worktree_id);
        self.send(
            EVENT_CHANGED,
            json!({ "user_id": user_id, "worktree_id": worktree_id, "status": status }),
        );
    }

    fn send(&self, name: &str, payload: serde_json::Value) {
        if let Some(b) = &self.broadcaster {
            b.broadcast(WebSocketMessage::new(name, payload));
        }
    }
}
