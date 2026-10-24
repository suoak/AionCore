use std::time::Instant;

use super::{FINALIZE_DEDUP_WINDOW, TeammateManager};

impl TeammateManager {
    pub fn begin_finalize(&self, conversation_id: &str) -> bool {
        let now = Instant::now();
        let should_proceed = !matches!(
            self.finalized_turns.get(conversation_id),
            Some(entry) if now.duration_since(*entry.value()) < FINALIZE_DEDUP_WINDOW
        );
        if should_proceed {
            self.finalized_turns.insert(conversation_id.to_owned(), now);
            let map = self.finalized_turns.clone();
            let key = conversation_id.to_owned();
            tokio::spawn(async move {
                tokio::time::sleep(FINALIZE_DEDUP_WINDOW).await;
                map.remove(&key);
            });
        }
        should_proceed
    }

    pub fn clear_finalized_turn(&self, conversation_id: &str) {
        self.finalized_turns.remove(conversation_id);
    }

    /// M8 E7-4: hold a dispatch lock for `(work_item, member, task)`.
    /// Returns `false` when that triple is already in progress (caller should
    /// answer "already in progress" and not wake the member again).
    pub fn try_begin_dispatch(&self, work_item_id: &str, member: &str, task_id: &str) -> bool {
        let key = dispatch_key(work_item_id, member, task_id);
        use dashmap::mapref::entry::Entry;
        match self.active_dispatches.entry(key) {
            Entry::Vacant(slot) => {
                slot.insert(());
                true
            }
            Entry::Occupied(_) => false,
        }
    }

    /// M8 E7-4: release the dispatch lock when the subtask finishes or is deleted.
    pub fn end_dispatch(&self, work_item_id: &str, member: &str, task_id: &str) {
        self.active_dispatches
            .remove(&dispatch_key(work_item_id, member, task_id));
    }

    /// Whether a dispatch lock is held (tests + callers that branch on it).
    pub fn is_dispatch_active(&self, work_item_id: &str, member: &str, task_id: &str) -> bool {
        self.active_dispatches
            .contains_key(&dispatch_key(work_item_id, member, task_id))
    }
}

fn dispatch_key(work_item_id: &str, member: &str, task_id: &str) -> String {
    format!("{work_item_id}\0{member}\0{task_id}")
}

/// M8 E7-4: true when a send would target the caller (self-trigger).
pub(crate) fn is_self_trigger(from_slot_id: &str, to_slot_id: &str) -> bool {
    from_slot_id == to_slot_id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_trigger_detects_same_slot() {
        assert!(is_self_trigger("worker-1", "worker-1"));
        assert!(!is_self_trigger("lead-1", "worker-1"));
        assert!(!is_self_trigger("worker-1", "*"));
    }

    #[test]
    fn dispatch_key_joins_triple() {
        assert_eq!(dispatch_key("w", "m", "t"), "w\0m\0t");
        assert_ne!(dispatch_key("w", "m", "t1"), dispatch_key("w", "m", "t2"));
    }
}
