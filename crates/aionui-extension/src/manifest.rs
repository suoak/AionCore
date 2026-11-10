use crate::asset_paths::canonical_relative_path;
use crate::constants::{EXTENSION_MANIFEST_FILE, RESERVED_NAME_PREFIXES};
use crate::error::ExtensionError;
use crate::hub::is_canonical_sha512_sri;
use crate::types::ExtensionManifest;
use serde_json::{Map, Value};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// Validate an extension manifest for required fields, name format, and version format.
pub fn validate_manifest(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    validate_name(&manifest.name)?;
    validate_version(&manifest.version)?;
    validate_agent_tool_handler_digests(manifest)?;
    validate_panel_entry_point_digests(manifest)?;
    validate_work_panel_view_entry_point_digests(manifest)?;
    validate_background_service_entry_point_digests(manifest)?;
    validate_manifest_files(manifest)?;
    Ok(())
}

/// Upper bound on top-level `files` entries (parse-time; Host install budgets are separate).
pub const MAX_MANIFEST_FILES: usize = 1024;

/// Fail closed on a malformed top-level `files` list, then enforce the contribution rules.
///
/// List rules (M9 E13 slice A): every `path` must be an exact canonical relative path and every
/// `integrity` a canonical sha512 SRI. Duplicates (also case-folded), directory-prefix clashes
/// (also case-folded), the manifest file itself and more than [`MAX_MANIFEST_FILES`] entries are
/// refused.
///
/// Contribution rules (M9 E13 integrity inheritance): when `files` is non-empty, every
/// contribution entry that names a package file — panel / workPanelView / backgroundService
/// `entryPoint` and agentTool `handler` — must be a canonical path listed in `files`, and its
/// integrity field must carry **exactly** the listed SRI. `parse_manifest` fills a missing field
/// from `files` (see [`inherit_integrity_from_files`]), so on a parsed manifest every downstream
/// binder and gate reads inherited integrity from the very same field as declared integrity. A typed
/// manifest that was not inherited (field still `None`) is refused here rather than treated as
/// undeclared. Manifests without `files` keep the existing per-entry behaviour unchanged.
/// Declaration consistency only: it reads no file, proves no bytes, and an omitted list never
/// means unlisted extras are verified.
fn validate_manifest_files(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let Some(declared) = validate_files_list(manifest)? else {
        return Ok(());
    };
    for (path, integrity) in contribution_file_entries(manifest) {
        let canonical = canonical_relative_path(path).ok_or_else(|| {
            ExtensionError::ManifestValidation(
                "contribution entry point must be a canonical relative path when files is declared".into(),
            )
        })?;
        let Some(listed) = declared.get(canonical.as_str()) else {
            return Err(ExtensionError::ManifestValidation(
                "contribution entry point is missing from the files list".into(),
            ));
        };
        match integrity {
            Some(i) if i == *listed => {}
            Some(_) => {
                return Err(ExtensionError::ManifestValidation(
                    "contribution integrity conflicts with the files list".into(),
                ));
            }
            None => {
                return Err(ExtensionError::ManifestValidation(
                    "contribution integrity was not inherited from the files list".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Validate the `files` list itself. `Ok(None)` when no list is declared; otherwise the
/// `path -> integrity` map of the (now known well-formed) list.
fn validate_files_list(manifest: &ExtensionManifest) -> Result<Option<BTreeMap<&str, &str>>, ExtensionError> {
    if manifest.files.is_empty() {
        return Ok(None);
    }
    if manifest.files.len() > MAX_MANIFEST_FILES {
        return Err(ExtensionError::ManifestValidation(
            "manifest files list exceeds the maximum entry count".into(),
        ));
    }
    let mut declared: BTreeMap<&str, &str> = BTreeMap::new();
    let mut folded: Vec<String> = Vec::with_capacity(manifest.files.len());
    let mut folded_seen: HashSet<String> = HashSet::with_capacity(manifest.files.len());
    let manifest_fold = EXTENSION_MANIFEST_FILE.to_lowercase();
    for file in &manifest.files {
        if canonical_relative_path(&file.path).is_none() {
            return Err(ExtensionError::ManifestValidation(
                "manifest files path must be a canonical relative path".into(),
            ));
        }
        if !is_canonical_sha512_sri(&file.integrity) {
            return Err(ExtensionError::ManifestValidation(
                "manifest files integrity must be a canonical sha512 SRI".into(),
            ));
        }
        let fold = file.path.to_lowercase();
        if fold == manifest_fold {
            return Err(ExtensionError::ManifestValidation(
                "manifest files must not list the manifest itself".into(),
            ));
        }
        if !folded_seen.insert(fold.clone()) {
            return Err(ExtensionError::ManifestValidation(
                "manifest files contains a duplicate path".into(),
            ));
        }
        folded.push(fold);
        declared.insert(file.path.as_str(), file.integrity.as_str());
    }
    // A listed path may not be a directory prefix of another (`a` vs `a/b`), case-folded.
    for fold in &folded {
        let mut end = 0;
        while let Some(pos) = fold[end..].find('/') {
            end += pos;
            if folded_seen.contains(&fold[..end]) {
                return Err(ExtensionError::ManifestValidation(
                    "manifest files contains a path that is a directory prefix of another".into(),
                ));
            }
            end += 1;
        }
    }
    Ok(Some(declared))
}

/// `(path, integrity)` for every contribution entry that names a package file: panel /
/// workPanelView / backgroundService `entryPoint` and agentTool `handler`. Entries with no path
/// (empty `entryPoint`, absent `handler`) name no file and are skipped.
fn contribution_file_entries(manifest: &ExtensionManifest) -> Vec<(&str, Option<&str>)> {
    let mut out = Vec::new();
    let Some(c) = manifest.contributes.as_ref() else {
        return out;
    };
    for p in c.panels.iter().filter(|p| !p.entry_point.is_empty()) {
        out.push((p.entry_point.as_str(), p.entry_point_integrity.as_deref()));
    }
    for v in c.work_panel_views.iter().filter(|v| !v.entry_point.is_empty()) {
        out.push((v.entry_point.as_str(), v.entry_point_integrity.as_deref()));
    }
    for s in c.background_services.iter().filter(|s| !s.entry_point.is_empty()) {
        out.push((s.entry_point.as_str(), s.entry_point_integrity.as_deref()));
    }
    for t in &c.agent_tools {
        if let Some(handler) = t.handler.as_deref().filter(|h| !h.is_empty()) {
            out.push((handler, t.handler_integrity.as_deref()));
        }
    }
    out
}

/// Declare-once authoring (M9 E13): when `files` is non-empty, give every contribution entry that
/// names a package file the integrity listed for that path in `files`, unless the entry already
/// carries its own value (which `validate_manifest` then requires to equal the listed one).
///
/// Runs inside `parse_manifest`, **before** validation, on the typed manifest every binder and
/// gate reads, so inherited integrity is stored in the same `entry_point_integrity` /
/// `handler_integrity` fields as declared integrity and is indistinguishable from it downstream
/// (asset-serve gate, background-start gate, panel / workPanelView / backgroundService / tool
/// binders, install allow-list). It never invents an integrity: an entry whose path is not a
/// canonical path listed in `files` is refused. Without `files` nothing changes.
fn inherit_integrity_from_files(manifest: &mut ExtensionManifest) -> Result<(), ExtensionError> {
    if manifest.files.is_empty() {
        return Ok(());
    }
    let listed: BTreeMap<String, String> = manifest
        .files
        .iter()
        .map(|f| (f.path.clone(), f.integrity.clone()))
        .collect();
    let Some(c) = manifest.contributes.as_mut() else {
        return Ok(());
    };
    let mut slots: Vec<(&str, &mut Option<String>)> = Vec::new();
    for p in c.panels.iter_mut().filter(|p| !p.entry_point.is_empty()) {
        slots.push((p.entry_point.as_str(), &mut p.entry_point_integrity));
    }
    for v in c.work_panel_views.iter_mut().filter(|v| !v.entry_point.is_empty()) {
        slots.push((v.entry_point.as_str(), &mut v.entry_point_integrity));
    }
    for s in c.background_services.iter_mut().filter(|s| !s.entry_point.is_empty()) {
        slots.push((s.entry_point.as_str(), &mut s.entry_point_integrity));
    }
    for t in c.agent_tools.iter_mut() {
        if let Some(handler) = t.handler.as_deref().filter(|h| !h.is_empty()) {
            slots.push((handler, &mut t.handler_integrity));
        }
    }
    for (path, slot) in slots {
        let canonical = canonical_relative_path(path).ok_or_else(|| {
            ExtensionError::ManifestValidation(
                "contribution entry point must be a canonical relative path when files is declared".into(),
            )
        })?;
        let integrity = listed.get(&canonical).ok_or_else(|| {
            ExtensionError::ManifestValidation("contribution entry point is missing from the files list".into())
        })?;
        if slot.is_none() {
            *slot = Some(integrity.clone());
        }
    }
    Ok(())
}

/// Fail closed when an optional handler digest is present but malformed or unbound.
///
/// Missing/None remains allowed for back-compat and does **not** mean verified.
fn validate_agent_tool_handler_digests(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Ok(());
    };
    for tool in &contributes.agent_tools {
        let Some(integrity) = tool.handler_integrity.as_deref() else {
            continue;
        };
        if !is_canonical_sha512_sri(integrity) {
            return Err(ExtensionError::ManifestValidation(
                "agent tool handlerIntegrity must be a canonical sha512 SRI".into(),
            ));
        }
        if tool.handler.as_deref().is_none_or(str::is_empty) {
            return Err(ExtensionError::ManifestValidation(
                "agent tool handlerIntegrity requires a non-empty handler path".into(),
            ));
        }
    }
    Ok(())
}

/// Fail closed when an optional panel entry-point digest is present but malformed.
///
/// Missing/None remains allowed for back-compat and does **not** mean verified.
/// Scoped to standalone `contributes.panels` only (workPanelViews have a sibling validator).
fn validate_panel_entry_point_digests(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Ok(());
    };
    for panel in &contributes.panels {
        let Some(integrity) = panel.entry_point_integrity.as_deref() else {
            continue;
        };
        if !is_canonical_sha512_sri(integrity) {
            return Err(ExtensionError::ManifestValidation(
                "panel entryPointIntegrity must be a canonical sha512 SRI".into(),
            ));
        }
        if panel.entry_point.is_empty() {
            return Err(ExtensionError::ManifestValidation(
                "panel entryPointIntegrity requires a non-empty entryPoint".into(),
            ));
        }
    }
    Ok(())
}

/// Fail closed when an optional workPanelView entry-point digest is present but malformed.
///
/// Missing/None remains allowed for back-compat and does **not** mean verified.
/// Scoped to `contributes.workPanelViews` only (standalone panels have a sibling validator).
fn validate_work_panel_view_entry_point_digests(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Ok(());
    };
    for view in &contributes.work_panel_views {
        let Some(integrity) = view.entry_point_integrity.as_deref() else {
            continue;
        };
        if !is_canonical_sha512_sri(integrity) {
            return Err(ExtensionError::ManifestValidation(
                "workPanelView entryPointIntegrity must be a canonical sha512 SRI".into(),
            ));
        }
        if view.entry_point.is_empty() {
            return Err(ExtensionError::ManifestValidation(
                "workPanelView entryPointIntegrity requires a non-empty entryPoint".into(),
            ));
        }
    }
    Ok(())
}

/// Fail closed when an optional backgroundService entry-point digest is present but malformed.
///
/// Missing/None remains allowed for back-compat and does **not** mean verified.
/// Scoped to `contributes.backgroundServices` only (panels / workPanelViews have sibling validators).
fn validate_background_service_entry_point_digests(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Ok(());
    };
    for service in &contributes.background_services {
        let Some(integrity) = service.entry_point_integrity.as_deref() else {
            continue;
        };
        if !is_canonical_sha512_sri(integrity) {
            return Err(ExtensionError::ManifestValidation(
                "backgroundService entryPointIntegrity must be a canonical sha512 SRI".into(),
            ));
        }
        if service.entry_point.is_empty() {
            return Err(ExtensionError::ManifestValidation(
                "backgroundService entryPointIntegrity requires a non-empty entryPoint".into(),
            ));
        }
    }
    Ok(())
}

/// Reject extension names that use reserved prefixes.
fn validate_name(name: &str) -> Result<(), ExtensionError> {
    if name.is_empty() {
        return Err(ExtensionError::ManifestValidation(
            "extension name must not be empty".into(),
        ));
    }

    let lower = name.to_lowercase();
    for prefix in RESERVED_NAME_PREFIXES {
        if lower.starts_with(prefix) {
            return Err(ExtensionError::ReservedNamePrefix {
                name: name.to_owned(),
                prefix: (*prefix).to_owned(),
            });
        }
    }
    Ok(())
}

/// Validate that the version string is valid semver.
fn validate_version(version: &str) -> Result<(), ExtensionError> {
    if version.is_empty() {
        return Err(ExtensionError::ManifestValidation(
            "extension version must not be empty".into(),
        ));
    }

    semver::Version::parse(version).map_err(|e| ExtensionError::InvalidVersion {
        version: version.to_owned(),
        reason: e.to_string(),
    })?;
    Ok(())
}

/// Parse and validate a manifest from JSON bytes.
pub fn parse_manifest(json_bytes: &[u8]) -> Result<ExtensionManifest, ExtensionError> {
    parse_manifest_inner(json_bytes, None)
}

/// Parse and validate a manifest from JSON bytes, resolving legacy `$file:`
/// references relative to the extension directory before deserialization.
pub fn parse_manifest_in_dir(json_bytes: &[u8], extension_dir: &Path) -> Result<ExtensionManifest, ExtensionError> {
    parse_manifest_inner(json_bytes, Some(extension_dir))
}

fn parse_manifest_inner(json_bytes: &[u8], extension_dir: Option<&Path>) -> Result<ExtensionManifest, ExtensionError> {
    let mut manifest_json: Value = serde_json::from_slice(json_bytes)?;
    if let Some(dir) = extension_dir {
        let mut visited = HashSet::new();
        manifest_json = resolve_file_refs(manifest_json, dir, &mut visited)?;
    }
    normalize_manifest_json(&mut manifest_json);
    let mut manifest: ExtensionManifest = serde_json::from_value(manifest_json)?;
    // Well-formed `files` first (clear errors), then declare-once inheritance, then full validation.
    validate_files_list(&manifest)?;
    inherit_integrity_from_files(&mut manifest)?;
    validate_manifest(&manifest)?;
    Ok(manifest)
}

fn resolve_file_refs(
    value: Value,
    extension_dir: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Value, ExtensionError> {
    match value {
        Value::String(text) if is_file_ref(&text) => resolve_file_ref_value(&text, extension_dir, visited),
        Value::Array(values) => {
            let mut resolved = Vec::with_capacity(values.len());
            for item in values {
                resolved.push(resolve_file_refs(item, extension_dir, visited)?);
            }
            Ok(Value::Array(resolved))
        }
        Value::Object(map) => {
            let mut resolved = Map::with_capacity(map.len());
            for (key, value) in map {
                resolved.insert(key, resolve_file_refs(value, extension_dir, visited)?);
            }
            Ok(Value::Object(resolved))
        }
        other => Ok(other),
    }
}

fn is_file_ref(value: &str) -> bool {
    value.starts_with("$file:")
}

fn resolve_file_ref_value(
    reference: &str,
    extension_dir: &Path,
    visited: &mut HashSet<PathBuf>,
) -> Result<Value, ExtensionError> {
    let relative = reference.trim_start_matches("$file:").trim();
    let absolute = extension_dir.join(relative);
    let canonical_base = std::fs::canonicalize(extension_dir)?;
    let canonical_path =
        std::fs::canonicalize(&absolute).map_err(|_| ExtensionError::FileReferenceNotFound(relative.to_owned()))?;

    if !canonical_path.starts_with(&canonical_base) {
        return Err(ExtensionError::PathTraversal(relative.to_owned()));
    }

    if !visited.insert(canonical_path.clone()) {
        return Err(ExtensionError::ManifestValidation(format!(
            "circular $file reference detected: {relative}"
        )));
    }

    let content = std::fs::read_to_string(&canonical_path)?;
    let resolved = match canonical_path.extension().and_then(|ext| ext.to_str()) {
        Some("json") | Some("jsonc") | Some("json5") => {
            let parsed: Value = serde_json::from_str(&content)?;
            resolve_file_refs(parsed, extension_dir, visited)?
        }
        _ => Value::String(content.trim_end_matches('\n').to_owned()),
    };

    visited.remove(&canonical_path);
    Ok(resolved)
}

fn normalize_manifest_json(value: &mut Value) {
    let Some(root) = value.as_object_mut() else {
        return;
    };

    move_key(root, "displayName", "display_name");
    move_key(root, "apiVersion", "api_version");
    move_key(root, "entryPoint", "entry_point");

    if let Some(i18n) = root.get_mut("i18n") {
        normalize_i18n(i18n);
    }

    if let Some(lifecycle) = root.get_mut("lifecycle") {
        normalize_lifecycle(lifecycle);
    }

    if let Some(contributes) = root.get_mut("contributes") {
        normalize_contributes(contributes);
    }
}

fn normalize_i18n(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "localesDir", "directory");
    if !obj.contains_key("locales") {
        if let Some(default_locale) = obj.remove("defaultLocale") {
            obj.insert("locales".into(), Value::Array(vec![default_locale]));
        }
    } else {
        obj.remove("defaultLocale");
    }
}

fn normalize_lifecycle(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "onInstall", "on_install");
    move_key(obj, "onUninstall", "on_uninstall");
    move_key(obj, "onActivate", "on_activate");
    move_key(obj, "onDeactivate", "on_deactivate");
}

fn normalize_contributes(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "acpAdapters", "acp_adapters");
    move_key(obj, "mcpServers", "mcp_servers");
    move_key(obj, "channelPlugins", "channel_plugins");
    move_key(obj, "settingsTabs", "settings_tabs");
    move_key(obj, "modelProviders", "model_providers");
    move_key(obj, "workPanelViews", "work_panel_views");
    move_key(obj, "agentTools", "agent_tools");
    move_key(obj, "backgroundServices", "background_services");

    normalize_array_entries(obj.get_mut("acp_adapters"), normalize_acp_adapter);
    normalize_array_entries(obj.get_mut("mcp_servers"), normalize_mcp_server);
    normalize_array_entries(obj.get_mut("assistants"), normalize_assistant);
    normalize_array_entries(obj.get_mut("agents"), normalize_agent);
    normalize_array_entries(obj.get_mut("skills"), normalize_skill);
    normalize_array_entries(obj.get_mut("channel_plugins"), normalize_channel_plugin);
    normalize_array_entries(obj.get_mut("themes"), normalize_theme);
    normalize_array_entries(obj.get_mut("settings_tabs"), normalize_settings_tab);
    normalize_array_entries(obj.get_mut("model_providers"), normalize_model_provider);
    normalize_array_entries(obj.get_mut("commands"), normalize_command);
    normalize_array_entries(obj.get_mut("panels"), normalize_panel);
    normalize_array_entries(obj.get_mut("work_panel_views"), normalize_work_panel_view);
    normalize_array_entries(obj.get_mut("agent_tools"), normalize_agent_tool);
    normalize_array_entries(obj.get_mut("background_services"), normalize_background_service);

    if let Some(webui) = obj.get_mut("webui") {
        normalize_webui(webui);
    }
}

fn normalize_command(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    move_key(obj, "keyBinding", "keybinding");
}

fn normalize_panel(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    move_key(obj, "entryPoint", "entry_point");
    // ExtPanel integrity field (workPanelViews use a separate normalizer).
    move_key(obj, "entryPointIntegrity", "entry_point_integrity");
}

fn normalize_work_panel_view(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    move_key(obj, "entryPoint", "entry_point");
    // ExtWorkPanelView integrity field (panels use a separate normalizer).
    move_key(obj, "entryPointIntegrity", "entry_point_integrity");
}

fn normalize_agent_tool(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    move_key(obj, "planSafeActions", "plan_safe_actions");
    move_key(obj, "handlerIntegrity", "handler_integrity");
}

fn normalize_background_service(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };
    move_key(obj, "entryPoint", "entry_point");
    // ExtBackgroundService integrity field (panels / workPanelViews use separate normalizers).
    move_key(obj, "entryPointIntegrity", "entry_point_integrity");
}

fn normalize_array_entries(value: Option<&mut Value>, normalize_item: fn(&mut Value)) {
    let Some(Value::Array(items)) = value else {
        return;
    };

    for item in items {
        normalize_item(item);
    }
}

fn normalize_acp_adapter(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "cliCommand", "cli_command");
    move_key(obj, "defaultCliPath", "default_cli_path");
    move_key(obj, "acpArgs", "acp_args");
    move_key(obj, "authRequired", "auth_required");
    move_key(obj, "supportsStreaming", "supports_streaming");
    move_key(obj, "connectionType", "connection_type");
    move_key(obj, "apiKeyFields", "api_key_fields");
    move_key(obj, "yoloMode", "yolo_mode");
    move_key(obj, "healthCheck", "health_check");
    move_key(obj, "icon", "avatar");
}

fn normalize_mcp_server(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    if !obj.contains_key("id")
        && let Some(Value::String(name)) = obj.get("name")
    {
        obj.insert("id".into(), Value::String(name.clone()));
    }
}

fn normalize_assistant(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "avatar", "icon");
    move_key(obj, "systemPrompt", "system_prompt");
    if !obj.contains_key("context") {
        if let Some(Value::String(path)) = obj.remove("contextFile") {
            obj.insert("context".into(), Value::String(format!("@file:{path}")));
        }
    } else {
        obj.remove("contextFile");
    }
}

fn normalize_agent(value: &mut Value) {
    normalize_assistant(value);
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "agentType", "agent_type");
}

fn normalize_skill(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "file", "path");
}

fn normalize_channel_plugin(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    if !obj.contains_key("id") {
        move_key(obj, "type", "id");
    }
    move_key(obj, "entryPoint", "entry_point");
}

fn normalize_theme(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "file", "css_file");
    move_key(obj, "cover", "cover_image");
}

fn normalize_settings_tab(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "name", "label");
    move_key(obj, "entryPoint", "url");

    if let Some(position) = obj.get_mut("position").and_then(Value::as_object_mut) {
        move_key(position, "anchor", "relativeTo");
    }
}

fn normalize_model_provider(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    move_key(obj, "baseUrl", "base_url");
    move_key(obj, "platform", "protocol");
}

fn normalize_webui(value: &mut Value) {
    let Some(obj) = value.as_object_mut() else {
        return;
    };

    if obj.contains_key("directory") && obj.contains_key("routes") {
        return;
    }

    let api_routes = obj
        .remove("apiRoutes")
        .or_else(|| obj.remove("api_routes"))
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let static_assets = obj
        .remove("staticAssets")
        .or_else(|| obj.remove("static_assets"))
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();

    let mut webui_entries = Vec::new();

    if !api_routes.is_empty() {
        let routes = api_routes
            .into_iter()
            .map(|mut route| {
                if let Some(route_obj) = route.as_object_mut() {
                    move_key(route_obj, "entryPoint", "handler");
                    route_obj.entry("method").or_insert_with(|| Value::String("GET".into()));
                }
                route
            })
            .collect::<Vec<_>>();

        webui_entries.push(Value::Object(Map::from_iter([
            ("id".into(), Value::String("legacy-webui-routes".into())),
            ("directory".into(), Value::String(".".into())),
            ("routes".into(), Value::Array(routes)),
        ])));
    }

    for (index, asset) in static_assets.into_iter().enumerate() {
        let Some(asset_obj) = asset.as_object() else {
            continue;
        };
        let directory = asset_obj
            .get("directory")
            .cloned()
            .unwrap_or_else(|| Value::String(".".into()));
        let id = asset_obj
            .get("urlPrefix")
            .or_else(|| asset_obj.get("url_prefix"))
            .and_then(Value::as_str)
            .map(|prefix| prefix.trim_matches('/').replace(['/', '.', '_'], "-"))
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| format!("legacy-webui-assets-{index}"));

        webui_entries.push(Value::Object(Map::from_iter([
            ("id".into(), Value::String(id)),
            ("directory".into(), directory),
            ("routes".into(), Value::Array(Vec::new())),
        ])));
    }

    *value = Value::Array(webui_entries);
}

fn move_key(map: &mut Map<String, Value>, old_key: &str, new_key: &str) {
    if map.contains_key(new_key) {
        map.remove(old_key);
        return;
    }

    if let Some(value) = map.remove(old_key) {
        map.insert(new_key.to_owned(), value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    // -- validate_manifest --

    #[test]
    fn test_valid_manifest() {
        let manifest = ExtensionManifest {
            name: "my-cool-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        assert!(validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn test_empty_name_rejected() {
        let manifest = ExtensionManifest {
            name: "".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        let err = validate_manifest(&manifest).unwrap_err();
        assert!(matches!(err, ExtensionError::ManifestValidation(_)));
    }

    #[test]
    fn test_reserved_prefix_aion() {
        let manifest = ExtensionManifest {
            name: "aion-my-ext".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        let err = validate_manifest(&manifest).unwrap_err();
        assert!(matches!(
            err,
            ExtensionError::ReservedNamePrefix { ref prefix, .. } if prefix == "aion-"
        ));
    }

    #[test]
    fn test_all_reserved_prefixes_rejected() {
        for prefix in RESERVED_NAME_PREFIXES {
            let name = format!("{prefix}test");
            let manifest = ExtensionManifest {
                name,
                version: "1.0.0".into(),
                display_name: None,
                description: None,
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: Default::default(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
                files: Vec::new(),
            };
            assert!(
                validate_manifest(&manifest).is_err(),
                "prefix '{prefix}' should be rejected"
            );
        }
    }

    #[test]
    fn test_reserved_prefix_case_insensitive() {
        let manifest = ExtensionManifest {
            name: "AION-upper".into(),
            version: "1.0.0".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn test_empty_version_rejected() {
        let manifest = ExtensionManifest {
            name: "my-ext".into(),
            version: "".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        let err = validate_manifest(&manifest).unwrap_err();
        assert!(matches!(err, ExtensionError::ManifestValidation(_)));
    }

    #[test]
    fn test_invalid_semver_rejected() {
        let manifest = ExtensionManifest {
            name: "my-ext".into(),
            version: "not-semver".into(),
            display_name: None,
            description: None,
            author: None,
            license: None,
            homepage: None,
            icon: None,
            engine: None,
            api_version: None,
            dependencies: Default::default(),
            entry_point: None,
            permissions: None,
            contributes: None,
            lifecycle: None,
            i18n: None,
            files: Vec::new(),
        };
        let err = validate_manifest(&manifest).unwrap_err();
        assert!(matches!(err, ExtensionError::InvalidVersion { .. }));
    }

    #[test]
    fn test_valid_semver_versions() {
        for version in &["0.0.1", "1.0.0", "1.2.3", "10.20.30", "1.0.0-alpha.1"] {
            let manifest = ExtensionManifest {
                name: "ext".into(),
                version: (*version).into(),
                display_name: None,
                description: None,
                author: None,
                license: None,
                homepage: None,
                icon: None,
                engine: None,
                api_version: None,
                dependencies: Default::default(),
                entry_point: None,
                permissions: None,
                contributes: None,
                lifecycle: None,
                i18n: None,
                files: Vec::new(),
            };
            assert!(
                validate_manifest(&manifest).is_ok(),
                "version '{version}' should be accepted"
            );
        }
    }

    // -- parse_manifest --

    #[test]
    fn test_parse_manifest_valid() {
        let raw = json!({"name": "my-ext", "version": "1.0.0"});
        let bytes = serde_json::to_vec(&raw).unwrap();
        let manifest = parse_manifest(&bytes).unwrap();
        assert_eq!(manifest.name, "my-ext");
        assert_eq!(manifest.version, "1.0.0");
    }

    #[test]
    fn test_parse_manifest_invalid_json() {
        let err = parse_manifest(b"not json").unwrap_err();
        assert!(matches!(err, ExtensionError::JsonParse(_)));
    }

    #[test]
    fn test_parse_manifest_missing_name() {
        let raw = json!({"version": "1.0.0"});
        let bytes = serde_json::to_vec(&raw).unwrap();
        let err = parse_manifest(&bytes).unwrap_err();
        assert!(matches!(err, ExtensionError::JsonParse(_)));
    }

    #[test]
    fn test_parse_manifest_reserved_name() {
        let raw = json!({"name": "internal-test", "version": "1.0.0"});
        let bytes = serde_json::to_vec(&raw).unwrap();
        let err = parse_manifest(&bytes).unwrap_err();
        assert!(matches!(err, ExtensionError::ReservedNamePrefix { .. }));
    }

    #[test]
    fn test_parse_manifest_in_dir_supports_aionui_main_contract() {
        let tmp = TempDir::new().unwrap();
        let contributes_dir = tmp.path().join("contributes");
        std::fs::create_dir_all(&contributes_dir).unwrap();
        std::fs::write(
            contributes_dir.join("settings-tabs.json"),
            serde_json::to_vec(&json!([
                {
                    "id": "legacy-settings",
                    "name": "Legacy Settings",
                    "entryPoint": "settings/legacy.html",
                    "position": { "anchor": "display", "placement": "after" }
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        let raw = json!({
            "name": "legacy-ext",
            "displayName": "Legacy Extension",
            "version": "1.0.0",
            "i18n": {
                "localesDir": "i18n",
                "defaultLocale": "en-US"
            },
            "contributes": {
                "settingsTabs": "$file:contributes/settings-tabs.json"
            }
        });

        let manifest = parse_manifest_in_dir(&serde_json::to_vec(&raw).unwrap(), tmp.path()).unwrap();
        assert_eq!(manifest.display_name.as_deref(), Some("Legacy Extension"));
        assert_eq!(manifest.i18n.as_ref().unwrap().locales, vec!["en-US".to_owned()]);
        assert_eq!(manifest.i18n.as_ref().unwrap().directory, "i18n");
        let settings_tabs = &manifest.contributes.as_ref().unwrap().settings_tabs;
        assert_eq!(settings_tabs.len(), 1);
        assert_eq!(settings_tabs[0].label, "Legacy Settings");
        assert_eq!(settings_tabs[0].url, "settings/legacy.html");
        assert_eq!(settings_tabs[0].position.as_ref().unwrap().relative_to, "display");
    }

    #[test]
    fn test_parse_manifest_in_dir_supports_legacy_webui_object() {
        let tmp = TempDir::new().unwrap();
        let raw = json!({
            "name": "legacy-webui-ext",
            "version": "1.0.0",
            "contributes": {
                "webui": {
                    "apiRoutes": [
                        {
                            "path": "/legacy-webui-ext/collect",
                            "entryPoint": "webui/collector.js"
                        }
                    ],
                    "staticAssets": [
                        {
                            "urlPrefix": "/legacy-webui-ext/assets",
                            "directory": "assets"
                        }
                    ]
                }
            }
        });

        let manifest = parse_manifest_in_dir(&serde_json::to_vec(&raw).unwrap(), tmp.path()).unwrap();
        let webui = &manifest.contributes.as_ref().unwrap().webui;
        assert_eq!(webui.len(), 2);
        assert_eq!(webui[0].routes[0].handler, "webui/collector.js");
        assert_eq!(webui[0].routes[0].method, "GET");
        assert_eq!(webui[1].directory, "assets");
    }

    // -- top-level `files` list (M9 E13 slice A) --

    fn sri(data: &[u8]) -> String {
        crate::hub::sri_sha512(data)
    }

    fn files_manifest(files: serde_json::Value, contributes: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "name": "demo.plugin",
            "version": "1.0.0",
            "files": files,
            "contributes": contributes,
        }))
        .unwrap()
    }

    fn panel(entry: &str, integrity: Option<&str>) -> serde_json::Value {
        let mut p = json!({"id": "board", "title": "Board", "entryPoint": entry});
        if let Some(i) = integrity {
            p["entryPointIntegrity"] = json!(i);
        }
        json!({"panels": [p]})
    }

    fn is_validation_err(r: Result<ExtensionManifest, ExtensionError>) -> bool {
        matches!(r, Err(ExtensionError::ManifestValidation(_)))
    }

    #[test]
    fn files_list_parses_and_round_trips() {
        let a = sri(b"a");
        let b = sri(b"b");
        let bytes = files_manifest(
            json!([{"path": "panels/board.html", "integrity": a}, {"path": "icons/logo.svg", "integrity": b}]),
            json!({}),
        );
        let m = parse_manifest(&bytes).unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[0].path, "panels/board.html");
        assert_eq!(m.files[1].integrity, b);
    }

    #[test]
    fn omitted_or_empty_files_is_back_compat_and_not_verified() {
        let m = parse_manifest(br#"{"name":"demo.plugin","version":"1.0.0"}"#).unwrap();
        assert!(m.files.is_empty());
        let m = parse_manifest(&files_manifest(json!([]), json!({}))).unwrap();
        assert!(m.files.is_empty());
        // The empty list serializes away (no new field on legacy manifests).
        assert!(!serde_json::to_string(&m).unwrap().contains("\"files\""));
    }

    #[test]
    fn legacy_contribution_integrity_without_files_is_unchanged() {
        let i = sri(b"x");
        let m = parse_manifest(&files_manifest(json!([]), panel("panels/board.html", Some(&i)))).unwrap();
        assert!(m.files.is_empty());
    }

    #[test]
    fn files_entry_rejects_bad_path_forms() {
        let good = sri(b"x");
        for bad in [
            "",
            "../a",
            "/a",
            "a/../b",
            "./a",
            "a//b",
            "a/./b",
            "a/",
            "a\\b",
            "a\0b",
            "C:\\x",
            "aion-extension.json",
            "AION-Extension.JSON",
        ] {
            let bytes = files_manifest(json!([{"path": bad, "integrity": good}]), json!({}));
            assert!(is_validation_err(parse_manifest(&bytes)), "{bad:?}");
        }
    }

    #[test]
    fn files_entry_rejects_bad_integrity_forms() {
        let good = sri(b"x");
        let mut bad_pad = good.clone().into_bytes();
        bad_pad[92] = b'B';
        let bad_pad = String::from_utf8(bad_pad).unwrap();
        for bad in [
            String::new(),
            "sha256-abc".to_owned(),
            good.to_uppercase(),
            format!("{good} "),
            good[..94].to_owned(),
            bad_pad,
        ] {
            let bytes = files_manifest(json!([{"path": "a.css", "integrity": bad}]), json!({}));
            assert!(is_validation_err(parse_manifest(&bytes)), "{bad:?}");
        }
    }

    #[test]
    fn files_entry_rejects_unknown_or_missing_fields() {
        let good = sri(b"x");
        for entry in [
            json!({"path": "a.css", "integrity": good, "mode": "0777"}),
            json!({"path": "a.css"}),
            json!({"integrity": good}),
        ] {
            let bytes = files_manifest(json!([entry]), json!({}));
            assert!(matches!(parse_manifest(&bytes), Err(ExtensionError::JsonParse(_))));
        }
        // Legacy non-object shape is refused too, not silently ignored.
        let bytes = files_manifest(json!(["a.css"]), json!({}));
        assert!(matches!(parse_manifest(&bytes), Err(ExtensionError::JsonParse(_))));
    }

    #[test]
    fn files_list_rejects_duplicates_including_case_folded() {
        let i = sri(b"x");
        let j = sri(b"y");
        for list in [
            json!([{"path": "a.css", "integrity": i}, {"path": "a.css", "integrity": j}]),
            json!([{"path": "a.css", "integrity": i}, {"path": "A.CSS", "integrity": i}]),
        ] {
            assert!(is_validation_err(parse_manifest(&files_manifest(list, json!({})))));
        }
    }

    #[test]
    fn files_list_rejects_directory_prefix_clash_including_case_folded() {
        let i = sri(b"x");
        for (a, b) in [("a", "a/b.css"), ("a/b", "a/b/c.css"), ("Icons", "icons/logo.svg")] {
            let list = json!([{"path": a, "integrity": i}, {"path": b, "integrity": i}]);
            assert!(
                is_validation_err(parse_manifest(&files_manifest(list.clone(), json!({})))),
                "{a} {b}"
            );
            // Order must not matter.
            let rev = json!([{"path": b, "integrity": i}, {"path": a, "integrity": i}]);
            assert!(
                is_validation_err(parse_manifest(&files_manifest(rev, json!({})))),
                "{b} {a}"
            );
        }
        // Sibling names sharing a string prefix are not directory clashes.
        let ok = json!([{"path": "a", "integrity": i}, {"path": "ab/c.css", "integrity": i}]);
        assert!(parse_manifest(&files_manifest(ok, json!({}))).is_ok());
    }

    #[test]
    fn files_list_count_is_bounded() {
        let i = sri(b"x");
        let build = |n: usize| {
            let list: Vec<_> = (0..n)
                .map(|k| json!({"path": format!("f/{k}.css"), "integrity": i}))
                .collect();
            files_manifest(json!(list), json!({}))
        };
        assert!(parse_manifest(&build(MAX_MANIFEST_FILES)).is_ok());
        assert!(is_validation_err(parse_manifest(&build(MAX_MANIFEST_FILES + 1))));
    }

    #[test]
    fn contribution_integrity_must_match_files_entry_when_files_declared() {
        let i = sri(b"panel");
        let other = sri(b"other");
        let css = sri(b"css");
        // Same path + same SRI: accepted (css ships via files only).
        let ok = files_manifest(
            json!([{"path": "panels/board.html", "integrity": i}, {"path": "panels/board.css", "integrity": css}]),
            panel("panels/board.html", Some(&i)),
        );
        assert!(parse_manifest(&ok).is_ok());
        // Contribution path absent from files.
        let missing = files_manifest(
            json!([{"path": "panels/board.css", "integrity": css}]),
            panel("panels/board.html", Some(&i)),
        );
        assert!(is_validation_err(parse_manifest(&missing)));
        // Different SRI for the same path.
        let conflict = files_manifest(
            json!([{"path": "panels/board.html", "integrity": other}]),
            panel("panels/board.html", Some(&i)),
        );
        assert!(is_validation_err(parse_manifest(&conflict)));
        // Non-canonical contribution path cannot silently match a files entry.
        let noncanon = files_manifest(
            json!([{"path": "panels/board.html", "integrity": i}]),
            panel("./panels/board.html", Some(&i)),
        );
        assert!(is_validation_err(parse_manifest(&noncanon)));
        // A contribution without integrity whose path is not in files is refused (no unlisted entry).
        let no_decl = files_manifest(
            json!([{"path": "panels/board.css", "integrity": css}]),
            panel("panels/board.html", None),
        );
        assert!(is_validation_err(parse_manifest(&no_decl)));
    }

    fn kind_contributes(kind: &str, path: &str, integrity: Option<&str>) -> serde_json::Value {
        let mut v = match kind {
            "panels" => json!({"id": "p", "title": "P", "entryPoint": path}),
            "workPanelViews" => json!({"id": "v", "title": "V", "entryPoint": path}),
            "backgroundServices" => json!({"id": "s", "entryPoint": path}),
            _ => json!({"id": "t", "name": "T", "handler": path}),
        };
        if let Some(i) = integrity {
            let key = if kind == "agentTools" {
                "handlerIntegrity"
            } else {
                "entryPointIntegrity"
            };
            v[key] = json!(i);
        }
        json!({ kind: [v] })
    }

    fn inherited(m: &ExtensionManifest, kind: &str) -> Option<String> {
        let c = m.contributes.as_ref().unwrap();
        match kind {
            "panels" => c.panels[0].entry_point_integrity.clone(),
            "workPanelViews" => c.work_panel_views[0].entry_point_integrity.clone(),
            "backgroundServices" => c.background_services[0].entry_point_integrity.clone(),
            _ => c.agent_tools[0].handler_integrity.clone(),
        }
    }

    const KINDS: [(&str, &str); 4] = [
        ("panels", "p.html"),
        ("workPanelViews", "v.html"),
        ("backgroundServices", "s.js"),
        ("agentTools", "t.js"),
    ];

    #[test]
    fn files_only_declaration_inherits_integrity_for_every_kind() {
        let i = sri(b"x");
        for (kind, path) in KINDS {
            let only_files = files_manifest(
                json!([{"path": path, "integrity": i}]),
                kind_contributes(kind, path, None),
            );
            let declared = files_manifest(
                json!([{"path": path, "integrity": i}]),
                kind_contributes(kind, path, Some(&i)),
            );
            let a = parse_manifest(&only_files).unwrap();
            let b = parse_manifest(&declared).unwrap();
            assert_eq!(inherited(&a, kind).as_deref(), Some(i.as_str()), "{kind}");
            // Inherited and declared manifests are identical typed values.
            assert_eq!(
                serde_json::to_value(&a).unwrap(),
                serde_json::to_value(&b).unwrap(),
                "{kind}"
            );
        }
    }

    #[test]
    fn inheritance_refuses_unlisted_noncanonical_and_conflicting_entries() {
        let i = sri(b"x");
        let other = sri(b"y");
        for (kind, path) in KINDS {
            let unlisted = files_manifest(
                json!([{"path": "z.css", "integrity": i}]),
                kind_contributes(kind, path, None),
            );
            assert!(is_validation_err(parse_manifest(&unlisted)), "{kind} unlisted");
            for bad in [format!("./{path}"), format!("a/../{path}"), path.to_uppercase()] {
                let m = files_manifest(
                    json!([{"path": path, "integrity": i}]),
                    kind_contributes(kind, &bad, None),
                );
                assert!(is_validation_err(parse_manifest(&m)), "{kind} {bad}");
            }
            let conflict = files_manifest(
                json!([{"path": path, "integrity": i}]),
                kind_contributes(kind, path, Some(&other)),
            );
            assert!(is_validation_err(parse_manifest(&conflict)), "{kind} conflict");
            // A malformed per-entry SRI is not replaced by the listed one.
            let malformed = files_manifest(
                json!([{"path": path, "integrity": i}]),
                kind_contributes(kind, path, Some("sha512-bad")),
            );
            assert!(is_validation_err(parse_manifest(&malformed)), "{kind} malformed");
        }
    }

    #[test]
    fn legacy_manifest_without_files_never_inherits() {
        for (kind, path) in KINDS {
            for files in [None, Some(json!([]))] {
                let mut raw = json!({"name": "demo.plugin", "version": "1.0.0", "contributes": kind_contributes(kind, path, None)});
                if let Some(f) = files {
                    raw["files"] = f;
                }
                let m = parse_manifest(&serde_json::to_vec(&raw).unwrap()).unwrap();
                assert_eq!(inherited(&m, kind), None, "{kind}");
            }
        }
    }

    #[test]
    fn empty_agent_tool_handler_and_empty_entry_point_need_no_files_entry() {
        let i = sri(b"x");
        let tool = json!({"agentTools": [{"id": "t", "name": "T"}]});
        let m = parse_manifest(&files_manifest(json!([{"path": "a.css", "integrity": i}]), tool)).unwrap();
        assert!(m.contributes.unwrap().agent_tools[0].handler_integrity.is_none());
    }

    #[test]
    fn validate_manifest_refuses_typed_files_manifest_that_did_not_inherit() {
        let i = sri(b"x");
        let bytes = files_manifest(
            json!([{"path": "p.html", "integrity": i}]),
            kind_contributes("panels", "p.html", None),
        );
        let mut m = parse_manifest(&bytes).unwrap();
        assert!(validate_manifest(&m).is_ok());
        // Hand-clearing the inherited field must never read as "undeclared": validation refuses it.
        m.contributes.as_mut().unwrap().panels[0].entry_point_integrity = None;
        assert!(is_validation_err(validate_manifest(&m).map(|()| m.clone())));
    }

    #[test]
    fn all_contribution_kinds_are_cross_checked_against_files() {
        let i = sri(b"x");
        let other = sri(b"y");
        let contributes = |kind: &str| match kind {
            "workPanelViews" => {
                json!({"workPanelViews": [{"id": "v", "title": "V", "entryPoint": "v.html", "entryPointIntegrity": i}]})
            }
            "backgroundServices" => {
                json!({"backgroundServices": [{"id": "s", "entryPoint": "s.js", "entryPointIntegrity": i}]})
            }
            _ => json!({"agentTools": [{"id": "t", "name": "T", "handler": "t.js", "handlerIntegrity": i}]}),
        };
        for (kind, path) in [
            ("workPanelViews", "v.html"),
            ("backgroundServices", "s.js"),
            ("agentTools", "t.js"),
        ] {
            let ok = files_manifest(json!([{"path": path, "integrity": i}]), contributes(kind));
            assert!(parse_manifest(&ok).is_ok(), "{kind}");
            let conflict = files_manifest(json!([{"path": path, "integrity": other}]), contributes(kind));
            assert!(is_validation_err(parse_manifest(&conflict)), "{kind}");
            let missing = files_manifest(json!([{"path": "z.css", "integrity": i}]), contributes(kind));
            assert!(is_validation_err(parse_manifest(&missing)), "{kind}");
        }
    }
}
