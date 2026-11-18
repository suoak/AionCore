//! Agent preflight aggregation.
//!
//! Pure projection of an [`AgentManagementRow`] (stored availability snapshot +
//! enable/install/runtime state) into an [`AgentPreflight`] report. It never
//! probes an agent itself; a stale or missing snapshot is reported as a
//! `connection` warning whose fix is a health check.

use aionui_api_types::{
    AgentManagementRow, AgentManagementStatus, AgentPreflight, ManagedRuntimeState, PreflightCheck, PreflightCheckKey,
    PreflightFix, PreflightFixAction, PreflightLevel,
};

/// Route the user to model provider settings to fix `no_provider`.
pub const PROVIDER_SETTINGS_TARGET: &str = "settings/model";

const CODE_AUTH_REQUIRED: &str = "auth_required";
const CODE_NO_PROVIDER: &str = "no_provider";
const CODE_RUNTIME_RETIRED: &str = "runtime_retired";
const VERSION_DRIFT_PREFIX: &str = "version_drift_";

fn check(key: PreflightCheckKey, level: PreflightLevel, message: impl Into<String>) -> PreflightCheck {
    PreflightCheck {
        key,
        level,
        message: message.into(),
        code: None,
        fix: None,
    }
}

fn with_fix(mut c: PreflightCheck, action: PreflightFixAction, target: impl Into<String>) -> PreflightCheck {
    c.fix = Some(PreflightFix {
        action,
        target: target.into(),
    });
    c
}

fn with_code(mut c: PreflightCheck, code: Option<&str>) -> PreflightCheck {
    c.code = code.map(str::to_owned);
    c
}

fn error_text(row: &AgentManagementRow, fallback: &str) -> String {
    row.last_check_error_message
        .as_deref()
        .filter(|m| !m.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

fn runtime_check(row: &AgentManagementRow) -> Option<PreflightCheck> {
    let runtime = row.runtime.as_ref()?;
    let c = match runtime.state {
        ManagedRuntimeState::Ready => check(PreflightCheckKey::Runtime, PreflightLevel::Pass, "Runtime is ready"),
        ManagedRuntimeState::Installing => {
            let message = match runtime.progress {
                Some(p) => format!("Runtime is installing ({p}%)"),
                None => "Runtime is installing".to_owned(),
            };
            check(PreflightCheckKey::Runtime, PreflightLevel::Warn, message)
        }
        ManagedRuntimeState::NotInstalled => with_fix(
            check(
                PreflightCheckKey::Runtime,
                PreflightLevel::Fail,
                "Runtime is not installed",
            ),
            PreflightFixAction::PrepareRuntime,
            &row.id,
        ),
        ManagedRuntimeState::Failed => with_fix(
            with_code(
                check(
                    PreflightCheckKey::Runtime,
                    PreflightLevel::Fail,
                    runtime
                        .error_message
                        .clone()
                        .unwrap_or_else(|| "Runtime installation failed".to_owned()),
                ),
                runtime.error_code.as_deref(),
            ),
            PreflightFixAction::PrepareRuntime,
            &row.id,
        ),
    };
    Some(c)
}

fn connection_checks(row: &AgentManagementRow) -> Vec<PreflightCheck> {
    let code = row.last_check_error_code.as_deref();
    match row.status {
        // The installed check already reports a missing agent.
        AgentManagementStatus::Missing => Vec::new(),
        AgentManagementStatus::Unchecked => vec![with_fix(
            check(
                PreflightCheckKey::Connection,
                PreflightLevel::Warn,
                "Agent has not been checked yet",
            ),
            PreflightFixAction::HealthCheck,
            &row.id,
        )],
        AgentManagementStatus::Online => {
            let mut out = vec![check(
                PreflightCheckKey::Connection,
                PreflightLevel::Pass,
                "Agent is reachable",
            )];
            if let Some(drift) = code.filter(|c| c.starts_with(VERSION_DRIFT_PREFIX)) {
                let message = row
                    .last_check_guidance
                    .clone()
                    .unwrap_or_else(|| error_text(row, "CLI version is outside the tested range"));
                out.push(with_fix(
                    with_code(
                        check(PreflightCheckKey::Version, PreflightLevel::Warn, message),
                        Some(drift),
                    ),
                    PreflightFixAction::UpdateCli,
                    &row.id,
                ));
            }
            out
        }
        AgentManagementStatus::Offline => {
            let c = match code {
                Some(CODE_AUTH_REQUIRED) => with_fix(
                    check(
                        PreflightCheckKey::Auth,
                        PreflightLevel::Fail,
                        error_text(row, "Log in to use this agent"),
                    ),
                    PreflightFixAction::Login,
                    &row.id,
                ),
                Some(CODE_NO_PROVIDER) => with_fix(
                    check(
                        PreflightCheckKey::Provider,
                        PreflightLevel::Fail,
                        error_text(row, "No model provider is configured"),
                    ),
                    PreflightFixAction::ConfigureProvider,
                    PROVIDER_SETTINGS_TARGET,
                ),
                // Retired backends cannot be repaired from the UI.
                Some(CODE_RUNTIME_RETIRED) => check(
                    PreflightCheckKey::Connection,
                    PreflightLevel::Fail,
                    error_text(row, "This agent has been retired"),
                ),
                _ => with_fix(
                    check(
                        PreflightCheckKey::Connection,
                        PreflightLevel::Fail,
                        error_text(row, "Agent could not be reached"),
                    ),
                    PreflightFixAction::HealthCheck,
                    &row.id,
                ),
            };
            vec![with_code(c, code)]
        }
    }
}

/// Build the preflight report for one management row.
pub fn preflight_for_row(row: &AgentManagementRow) -> AgentPreflight {
    let mut checks = Vec::new();

    checks.push(if row.enabled {
        check(PreflightCheckKey::Enabled, PreflightLevel::Pass, "Agent is enabled")
    } else {
        with_fix(
            check(PreflightCheckKey::Enabled, PreflightLevel::Warn, "Agent is disabled"),
            PreflightFixAction::Enable,
            &row.id,
        )
    });

    let installed = row.installed && row.status != AgentManagementStatus::Missing;
    checks.push(if installed {
        check(PreflightCheckKey::Installed, PreflightLevel::Pass, "Agent is installed")
    } else {
        let target = row
            .agent_source_info
            .hub_package_id
            .clone()
            .unwrap_or_else(|| row.id.clone());
        with_code(
            with_fix(
                check(
                    PreflightCheckKey::Installed,
                    PreflightLevel::Fail,
                    "Agent is not installed",
                ),
                PreflightFixAction::Install,
                target,
            ),
            row.last_check_error_code.as_deref(),
        )
    });

    if let Some(c) = runtime_check(row) {
        checks.push(c);
    }
    checks.extend(connection_checks(row));

    let level = checks.iter().map(|c| c.level).max().unwrap_or(PreflightLevel::Pass);
    AgentPreflight {
        id: row.id.clone(),
        name: row.name.clone(),
        backend: row.backend.clone(),
        level,
        ready: row.enabled && level != PreflightLevel::Fail,
        checks,
        checked_at: row.last_check_at,
    }
}

#[cfg(test)]
#[path = "preflight_tests.rs"]
mod tests;
