use super::*;
use aionui_api_types::AgentManagementRow;
use serde_json::{Value, json};

fn row(overrides: Value) -> AgentManagementRow {
    let mut base = json!({
        "id": "agent-1",
        "name": "Claude Code",
        "backend": "claude",
        "agent_type": "acp",
        "agent_source": "builtin",
        "enabled": true,
        "installed": true,
        "sort_order": 0,
        "status": "online",
    });
    for (k, v) in overrides.as_object().unwrap() {
        base[k] = v.clone();
    }
    serde_json::from_value(base).expect("valid management row")
}

fn find(report: &AgentPreflight, key: PreflightCheckKey) -> &PreflightCheck {
    report
        .checks
        .iter()
        .find(|c| c.key == key)
        .unwrap_or_else(|| panic!("missing check {key:?}"))
}

#[test]
fn online_enabled_installed_agent_is_ready() {
    let report = preflight_for_row(&row(json!({ "last_check_at": 1700000000000_i64 })));
    assert!(report.ready);
    assert_eq!(report.level, PreflightLevel::Pass);
    assert_eq!(report.id, "agent-1");
    assert_eq!(report.backend.as_deref(), Some("claude"));
    assert_eq!(report.checked_at, Some(1700000000000));
    let keys: Vec<_> = report.checks.iter().map(|c| c.key).collect();
    assert_eq!(
        keys,
        vec![
            PreflightCheckKey::Enabled,
            PreflightCheckKey::Installed,
            PreflightCheckKey::Connection
        ]
    );
    assert!(report.checks.iter().all(|c| c.fix.is_none()));
}

#[test]
fn disabled_agent_warns_with_enable_fix_and_is_not_ready() {
    let report = preflight_for_row(&row(json!({ "enabled": false })));
    assert!(!report.ready);
    assert_eq!(report.level, PreflightLevel::Warn);
    let enabled = find(&report, PreflightCheckKey::Enabled);
    assert_eq!(enabled.level, PreflightLevel::Warn);
    assert_eq!(
        enabled.fix,
        Some(PreflightFix {
            action: PreflightFixAction::Enable,
            target: "agent-1".to_owned()
        })
    );
}

#[test]
fn missing_agent_fails_install_with_hub_package_target() {
    let report = preflight_for_row(&row(json!({
        "installed": false,
        "status": "missing",
        "last_check_error_code": "command_not_found",
        "agent_source_info": { "hub_package_id": "hub.claude" },
    })));
    assert!(!report.ready);
    assert_eq!(report.level, PreflightLevel::Fail);
    let installed = find(&report, PreflightCheckKey::Installed);
    assert_eq!(installed.code.as_deref(), Some("command_not_found"));
    assert_eq!(installed.fix.as_ref().unwrap().action, PreflightFixAction::Install);
    assert_eq!(installed.fix.as_ref().unwrap().target, "hub.claude");
    // A missing agent does not also get a connection check.
    assert!(!report.checks.iter().any(|c| c.key == PreflightCheckKey::Connection));
}

#[test]
fn unchecked_agent_warns_with_health_check_fix() {
    let report = preflight_for_row(&row(json!({ "status": "unchecked" })));
    assert!(report.ready, "warnings do not block");
    let conn = find(&report, PreflightCheckKey::Connection);
    assert_eq!(conn.level, PreflightLevel::Warn);
    assert_eq!(conn.fix.as_ref().unwrap().action, PreflightFixAction::HealthCheck);
}

#[test]
fn auth_required_maps_to_auth_check_with_login_fix() {
    let report = preflight_for_row(&row(json!({
        "status": "offline",
        "last_check_error_code": "auth_required",
        "last_check_error_message": "Please run `claude login`",
    })));
    assert!(!report.ready);
    let auth = find(&report, PreflightCheckKey::Auth);
    assert_eq!(auth.level, PreflightLevel::Fail);
    assert_eq!(auth.code.as_deref(), Some("auth_required"));
    assert_eq!(auth.message, "Please run `claude login`");
    assert_eq!(auth.fix.as_ref().unwrap().action, PreflightFixAction::Login);
}

#[test]
fn no_provider_maps_to_provider_settings_fix() {
    let report = preflight_for_row(&row(json!({
        "status": "offline",
        "agent_type": "aionrs",
        "last_check_error_code": "no_provider",
    })));
    let provider = find(&report, PreflightCheckKey::Provider);
    assert_eq!(provider.message, "No model provider is configured");
    assert_eq!(
        provider.fix,
        Some(PreflightFix {
            action: PreflightFixAction::ConfigureProvider,
            target: PROVIDER_SETTINGS_TARGET.to_owned()
        })
    );
}

#[test]
fn generic_offline_failure_offers_health_check_and_keeps_code() {
    let report = preflight_for_row(&row(json!({
        "status": "offline",
        "last_check_error_code": "acp_init_failed",
        "last_check_error_message": "   ",
    })));
    let conn = find(&report, PreflightCheckKey::Connection);
    assert_eq!(conn.level, PreflightLevel::Fail);
    assert_eq!(conn.code.as_deref(), Some("acp_init_failed"));
    assert_eq!(conn.message, "Agent could not be reached", "blank message falls back");
    assert_eq!(conn.fix.as_ref().unwrap().action, PreflightFixAction::HealthCheck);
}

#[test]
fn retired_runtime_fails_without_fix() {
    let report = preflight_for_row(&row(json!({
        "status": "offline",
        "last_check_error_code": "runtime_retired",
    })));
    let conn = find(&report, PreflightCheckKey::Connection);
    assert_eq!(conn.level, PreflightLevel::Fail);
    assert!(conn.fix.is_none());
}

#[test]
fn version_drift_on_online_agent_is_a_warning() {
    let report = preflight_for_row(&row(json!({
        "status": "online",
        "last_check_error_code": "version_drift_older",
        "last_check_guidance": "Update Claude Code to 2.x",
    })));
    assert!(report.ready);
    assert_eq!(report.level, PreflightLevel::Warn);
    let version = find(&report, PreflightCheckKey::Version);
    assert_eq!(version.code.as_deref(), Some("version_drift_older"));
    assert_eq!(version.message, "Update Claude Code to 2.x");
    assert_eq!(version.fix.as_ref().unwrap().action, PreflightFixAction::UpdateCli);
    assert_eq!(find(&report, PreflightCheckKey::Connection).level, PreflightLevel::Pass);
}

#[test]
fn managed_runtime_states_map_to_levels() {
    let runtime = |state: &str| {
        row(json!({
            "runtime": {
                "runtime_id": "rt",
                "release": "1",
                "state": state,
                "progress": 40,
                "error_message": "disk full",
            }
        }))
    };
    let ready = preflight_for_row(&runtime("ready"));
    assert_eq!(find(&ready, PreflightCheckKey::Runtime).level, PreflightLevel::Pass);

    let installing = preflight_for_row(&runtime("installing"));
    let c = find(&installing, PreflightCheckKey::Runtime);
    assert_eq!(c.level, PreflightLevel::Warn);
    assert_eq!(c.message, "Runtime is installing (40%)");

    for state in ["not_installed", "failed"] {
        let report = preflight_for_row(&runtime(state));
        let c = find(&report, PreflightCheckKey::Runtime);
        assert_eq!(c.level, PreflightLevel::Fail, "{state}");
        assert_eq!(c.fix.as_ref().unwrap().action, PreflightFixAction::PrepareRuntime);
        assert!(!report.ready);
    }
    let failed = preflight_for_row(&runtime("failed"));
    assert_eq!(find(&failed, PreflightCheckKey::Runtime).message, "disk full");
}
