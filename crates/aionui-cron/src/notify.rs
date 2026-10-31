//! E8-6 Autopilot result delivery (guardrail 6, D8-7).
//!
//! The cron crate only decides *when* to report; the app wires the actual
//! delivery (IM via aionui-channel, failures also into the Inbox).

use async_trait::async_trait;

/// How an unattended Autopilot run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutopilotOutcome {
    Ok,
    Error,
}

impl AutopilotOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }
}

/// One finished scheduled run of an Autopilot job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutopilotRunReport {
    pub user_id: String,
    pub job_id: String,
    pub job_name: String,
    /// `assistant_sessions.id` to receive the IM summary, if configured.
    pub notify_channel_id: Option<String>,
    pub outcome: AutopilotOutcome,
    /// Conversation the run used, when known.
    pub conversation_id: Option<String>,
    pub error: Option<String>,
}

const SUMMARY_ERROR_MAX_CHARS: usize = 300;

impl AutopilotRunReport {
    /// Plain-text IM summary.
    pub fn summary_text(&self) -> String {
        match self.outcome {
            AutopilotOutcome::Ok => format!("\u{2705} Autopilot \"{}\" finished.", self.job_name),
            AutopilotOutcome::Error => {
                let error = self.error.as_deref().map(str::trim).filter(|e| !e.is_empty());
                match error {
                    Some(error) => format!(
                        "\u{274c} Autopilot \"{}\" failed: {}",
                        self.job_name,
                        truncate_chars(error, SUMMARY_ERROR_MAX_CHARS)
                    ),
                    None => format!("\u{274c} Autopilot \"{}\" failed.", self.job_name),
                }
            }
        }
    }
}

pub(crate) fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out: String = text.chars().take(max_chars).collect();
    if text.chars().count() > max_chars {
        out.push('\u{2026}');
    }
    out
}

/// Delivery port implemented by the app.
#[async_trait]
pub trait AutopilotNotifier: Send + Sync {
    /// Check that `channel_id` is an IM session owned by `user_id` that can
    /// receive messages. `Err` carries a user-facing reason.
    async fn validate_target(&self, user_id: &str, channel_id: &str) -> Result<(), String>;

    /// Deliver one run report. Must not fail the run; log and swallow errors.
    async fn notify(&self, report: AutopilotRunReport);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(outcome: AutopilotOutcome, error: Option<&str>) -> AutopilotRunReport {
        AutopilotRunReport {
            user_id: "u1".into(),
            job_id: "cron_1".into(),
            job_name: "Nightly".into(),
            notify_channel_id: None,
            outcome,
            conversation_id: None,
            error: error.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn summary_text_variants() {
        assert_eq!(
            report(AutopilotOutcome::Ok, None).summary_text(),
            "\u{2705} Autopilot \"Nightly\" finished."
        );
        assert_eq!(
            report(AutopilotOutcome::Error, Some(" boom ")).summary_text(),
            "\u{274c} Autopilot \"Nightly\" failed: boom"
        );
        assert_eq!(
            report(AutopilotOutcome::Error, Some("  ")).summary_text(),
            "\u{274c} Autopilot \"Nightly\" failed."
        );
        let long = "x".repeat(SUMMARY_ERROR_MAX_CHARS + 5);
        let text = report(AutopilotOutcome::Error, Some(&long)).summary_text();
        assert!(text.ends_with('\u{2026}'));
    }
}
