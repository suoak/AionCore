//! Extension `capabilities` schema, split fingerprints, and coherence rules (M9 E13 S2).
//!
//! A top-level `capabilities` block is optional. When it is absent the extension is treated as
//! legacy / pure-compute: background services may not list any `uses`, and no fingerprint is
//! required. When it is present, parsing is fail-closed (`deny_unknown_fields`, unknown types,
//! unknown event names, invalid origins, ... all become [`ExtensionError::ManifestValidation`]).
//!
//! # Split fingerprints (§2.3)
//! Each kind is hashed as
//! `sha256(b"workmate.capabilities.v1\0" || kind || b"\0" || canonical_json)`
//! where `canonical_json` is compact JSON with sorted keys, no whitespace, no `reason`,
//! normalized origins/events, roots/secrets sorted by `id`, and integer quotas.
//!
//! The overall fingerprint is `"cap1:"` plus the lowercase hex of `sha256` over the kind
//! digests concatenated in this **fixed order**:
//!
//! 1. `network`
//! 2. `files`
//! 3. `events`
//! 4. `storage`
//! 5. `secrets` — **only when the `secrets` field is declared** (even as an empty array)
//!
//! An absent kind among the first four is represented by the canonical JSON `null`, so a
//! schema-1 manifest that does not mention `secrets` hashes identically to the original
//! four-kind formula in the design. Adding a `secrets` declaration extends the concatenation
//! with a fifth digest; it never changes the order of the first four.
//!
//! `reason` text is never hashed. `capabilityFingerprint` is written by `workmate-pack` and
//! recomputed by the Host; a mismatch, or a missing fingerprint on a manifest that declares
//! `capabilities`, is [`ExtensionError::ManifestValidation`].

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;

use serde::de::{self, Deserializer};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::error::ExtensionError;
use crate::types::{ExtPermissions, ExtensionManifest, FilesystemScope, NetworkPermission, ServiceCapabilityKind};

/// `capabilities.schema` must be this value.
pub const CAPABILITIES_SCHEMA: u32 = 1;
/// Overall fingerprint prefix (`cap1:` + 64 lowercase hex).
pub const CAPABILITY_FINGERPRINT_PREFIX: &str = "cap1:";
/// Domain-separator / kind-hash prefix.
pub const CAPABILITY_HASH_DOMAIN: &[u8] = b"workmate.capabilities.v1";

/// Maximum number of `network.origins` entries.
pub const MAX_NETWORK_ORIGINS: usize = 16;
/// Maximum length of any `reason` string (Unicode scalar values).
pub const MAX_REASON_CHARS: usize = 200;

/// Default / maximum quotas for `files.data`.
pub const FILES_DATA_DEFAULT_QUOTA_BYTES: u64 = 8 * 1024 * 1024;
pub const FILES_DATA_MAX_QUOTA_BYTES: u64 = 64 * 1024 * 1024;

/// Default / maximum quotas for `storage`.
pub const STORAGE_DEFAULT_QUOTA_BYTES: u64 = 1_048_576;
pub const STORAGE_DEFAULT_MAX_KEYS: u32 = 256;
pub const STORAGE_MAX_QUOTA_BYTES: u64 = 32 * 1024 * 1024;
pub const STORAGE_MAX_KEYS: u32 = 4096;

/// Host event-name whitelist (§6). These are the dotted names authors put in
/// `events.subscribe`, **not** the `SCREAMING_SNAKE_CASE` serde names of
/// [`crate::types::ExtensionSystemEvent`].
pub const CAPABILITY_EVENT_NAMES: &[&str] = &[
    "extension.activated",
    "extension.deactivated",
    "extension.installed",
    "extension.uninstalled",
    "registry.reloaded",
    "states.persisted",
];

/// Standard (non-custom) headers that `secrets[].inject.header` may name.
const STANDARD_INJECT_HEADERS: &[&str] = &["Authorization", "X-Api-Key", "X-Auth-Token"];

/// Kind names in the overall fingerprint concatenation (secrets is last and optional).
const KIND_ORDER_CORE: [&str; 4] = ["network", "files", "events", "storage"];

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Top-level `capabilities` object (schema 1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExtensionCapabilities {
    pub schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<NetworkCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<FilesCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<EventsCapabilities>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<StorageCapabilities>,
    /// `Some` iff the author declared the `secrets` field (possibly as `[]`).
    /// Absence vs an empty array is preserved so the overall fingerprint stays
    /// on the four-kind formula when the field is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<Vec<SecretCapability>>,
}

impl ExtensionCapabilities {
    /// Kinds this declaration actually provides.
    pub fn declared_kinds(&self) -> HashSet<ServiceCapabilityKind> {
        let mut kinds = HashSet::new();
        if self.network.is_some() {
            kinds.insert(ServiceCapabilityKind::Network);
        }
        if self.files.is_some() {
            kinds.insert(ServiceCapabilityKind::Files);
        }
        if self.events.is_some() {
            kinds.insert(ServiceCapabilityKind::Events);
        }
        if self.storage.is_some() {
            kinds.insert(ServiceCapabilityKind::Storage);
        }
        if self.secrets.as_ref().is_some_and(|s| !s.is_empty()) {
            kinds.insert(ServiceCapabilityKind::Secrets);
        }
        kinds
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCapabilities {
    pub origins: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesCapabilities {
    /// Expanded form: `true` becomes the default 8 MiB quota; `false` is omitted.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_files_data"
    )]
    pub data: Option<FilesDataQuota>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<FileRoot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FilesDataQuota {
    #[serde(alias = "quotaBytes")]
    pub quota_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileRoot {
    pub id: String,
    pub access: FileRootAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FileRootAccess {
    #[serde(rename = "read")]
    Read,
    #[serde(rename = "readwrite")]
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsCapabilities {
    pub subscribe: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCapabilities {
    pub quota_bytes: u64,
    pub max_keys: u32,
    pub reason: Option<String>,
}

impl Serialize for StorageCapabilities {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let n = 2 + usize::from(self.reason.is_some());
        let mut s = serializer.serialize_struct("StorageCapabilities", n)?;
        s.serialize_field("quotaBytes", &self.quota_bytes)?;
        s.serialize_field("maxKeys", &self.max_keys)?;
        if let Some(reason) = &self.reason {
            s.serialize_field("reason", reason)?;
        }
        s.end()
    }
}

impl<'de> Deserialize<'de> for StorageCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Object {
            #[serde(default, alias = "quotaBytes")]
            quota_bytes: Option<u64>,
            #[serde(default, alias = "maxKeys")]
            max_keys: Option<u32>,
            #[serde(default)]
            reason: Option<String>,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Wire {
            Flag(bool),
            Object(Object),
        }
        match Wire::deserialize(deserializer)? {
            Wire::Flag(true) => Ok(Self {
                quota_bytes: STORAGE_DEFAULT_QUOTA_BYTES,
                max_keys: STORAGE_DEFAULT_MAX_KEYS,
                reason: None,
            }),
            Wire::Flag(false) => Err(de::Error::custom(
                "capabilities.storage: false is invalid; omit the field instead",
            )),
            Wire::Object(object) => Ok(Self {
                quota_bytes: object.quota_bytes.unwrap_or(STORAGE_DEFAULT_QUOTA_BYTES),
                max_keys: object.max_keys.unwrap_or(STORAGE_DEFAULT_MAX_KEYS),
                reason: object.reason,
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretCapability {
    pub id: String,
    pub origins: Vec<String>,
    pub inject: SecretInject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretInject {
    pub header: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scheme: Option<String>,
}

fn deserialize_files_data<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<FilesDataQuota>, D::Error> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Object {
        #[serde(default, alias = "quotaBytes")]
        quota_bytes: Option<u64>,
        #[serde(default)]
        reason: Option<String>,
    }
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Wire {
        Flag(bool),
        Object(Object),
    }
    match Option::<Wire>::deserialize(deserializer)? {
        None | Some(Wire::Flag(false)) => Ok(None),
        Some(Wire::Flag(true)) => Ok(Some(FilesDataQuota {
            quota_bytes: FILES_DATA_DEFAULT_QUOTA_BYTES,
            reason: None,
        })),
        Some(Wire::Object(object)) => Ok(Some(FilesDataQuota {
            quota_bytes: object.quota_bytes.unwrap_or(FILES_DATA_DEFAULT_QUOTA_BYTES),
            reason: object.reason,
        })),
    }
}

impl<'de> Deserialize<'de> for ExtensionCapabilities {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: u32,
            #[serde(default)]
            network: Option<NetworkCapabilities>,
            #[serde(default)]
            files: Option<FilesCapabilities>,
            #[serde(default)]
            events: Option<EventsCapabilities>,
            #[serde(default)]
            storage: Option<StorageCapabilities>,
            #[serde(default)]
            secrets: Option<Vec<SecretCapability>>,
        }
        let raw = Raw::deserialize(deserializer)?;
        Ok(Self {
            schema: raw.schema,
            network: raw.network,
            files: raw.files,
            events: raw.events,
            storage: raw.storage,
            secrets: raw.secrets,
        })
    }
}

// ---------------------------------------------------------------------------
// Origin normalization
// ---------------------------------------------------------------------------

/// A normalized `https` origin: ASCII-lowercase host (or `*.host`), and a port
/// that is `None` when it is the default 443.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CanonicalOrigin {
    pub wildcard: bool,
    pub host: String,
    pub port: Option<u16>,
}

impl CanonicalOrigin {
    pub fn as_string(&self) -> String {
        let host = if self.wildcard {
            format!("*.{}", self.host)
        } else {
            self.host.clone()
        };
        match self.port {
            Some(port) => format!("https://{host}:{port}"),
            None => format!("https://{host}"),
        }
    }

    /// Whether this declared origin covers `other` (exact match, or a single-level
    /// wildcard covering an exact host on the same port). A wildcard is only
    /// covered by an identical wildcard.
    pub fn covers(&self, other: &CanonicalOrigin) -> bool {
        if self.port != other.port {
            return false;
        }
        if self.wildcard {
            if other.wildcard {
                return self.host == other.host;
            }
            return single_level_subdomain(&other.host, &self.host);
        }
        !other.wildcard && self.host == other.host
    }
}

fn single_level_subdomain(host: &str, parent: &str) -> bool {
    let suffix = format!(".{parent}");
    host.strip_suffix(&suffix)
        .is_some_and(|label| !label.is_empty() && !label.contains('.'))
}

fn invalid(msg: impl Into<String>) -> ExtensionError {
    ExtensionError::ManifestValidation(msg.into())
}

fn validate_reason(reason: &Option<String>, field: &str) -> Result<(), ExtensionError> {
    let Some(reason) = reason else {
        return Ok(());
    };
    if reason.chars().count() > MAX_REASON_CHARS {
        return Err(invalid(format!(
            "{field} reason must be at most {MAX_REASON_CHARS} characters"
        )));
    }
    Ok(())
}

fn is_capability_id(id: &str) -> bool {
    let len = id.len();
    (1..=32).contains(&len)
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// Parse and normalize a single `https://host[:port]` origin.
pub fn parse_origin(raw: &str) -> Result<CanonicalOrigin, ExtensionError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(invalid("network origin must not be empty"));
    }
    let rest = trimmed
        .strip_prefix("https://")
        .ok_or_else(|| invalid("network origin must be an https URL"))?;
    if rest.is_empty() {
        return Err(invalid("network origin must include a host"));
    }
    if rest.contains('@') {
        return Err(invalid("network origin must not include userinfo"));
    }
    if rest.contains('/') || rest.contains('?') || rest.contains('#') {
        return Err(invalid("network origin must not include a path, query, or fragment"));
    }
    if rest.starts_with('[') {
        return Err(invalid("network origin must not use an IP literal"));
    }

    let (host_raw, port) = split_host_port(rest)?;
    if host_raw.is_empty() {
        return Err(invalid("network origin must include a host"));
    }

    let (wildcard, domain) = if let Some(suffix) = host_raw.strip_prefix("*.") {
        if suffix.contains('*') || host_raw.matches('*').count() != 1 {
            return Err(invalid("network origin wildcard must be a single leftmost label"));
        }
        if suffix.is_empty() {
            return Err(invalid("network origin wildcard must have a domain suffix"));
        }
        (true, suffix)
    } else if host_raw.contains('*') || host_raw == "*" {
        return Err(invalid("network origin wildcard must be a single leftmost label"));
    } else {
        (false, host_raw)
    };

    let host = normalize_domain(domain)?;
    if is_reserved_host(&host) {
        return Err(invalid("network origin must not use a reserved or local host"));
    }
    if looks_like_ip(&host) {
        return Err(invalid("network origin must not use an IP literal"));
    }
    // A wildcard suffix of `example.com` is two labels; a bare `https://intranet`
    // single-label name is too close to a local alias and is refused.
    if !host.contains('.') {
        return Err(invalid("network origin host must be a fully-qualified domain name"));
    }
    Ok(CanonicalOrigin { wildcard, host, port })
}

fn split_host_port(authority: &str) -> Result<(&str, Option<u16>), ExtensionError> {
    if let Some((host, port_str)) = authority.rsplit_once(':')
        && !host.is_empty()
        && !host.contains(']')
        && port_str.bytes().all(|b| b.is_ascii_digit())
        && !port_str.is_empty()
    {
        if port_str.len() > 5 {
            return Err(invalid("network origin port is invalid"));
        }
        let port: u32 = port_str
            .parse()
            .map_err(|_| invalid("network origin port is invalid"))?;
        if port == 0 || port > u32::from(u16::MAX) {
            return Err(invalid("network origin port is invalid"));
        }
        let port = port as u16;
        let port = if port == 443 { None } else { Some(port) };
        return Ok((host, port));
    }
    Ok((authority, None))
}

fn normalize_domain(domain: &str) -> Result<String, ExtensionError> {
    if domain.is_empty() || domain.starts_with('.') || domain.ends_with('.') || domain.contains("..") {
        return Err(invalid("network origin host is invalid"));
    }
    if domain.contains('@') || domain.contains('/') || domain.contains(' ') {
        return Err(invalid("network origin host is invalid"));
    }
    // Route through the WHATWG parser so IDNA is applied consistently and
    // IPv4/IPv6 forms the raw string missed are still caught.
    let probe = format!("https://{domain}");
    let url = Url::parse(&probe).map_err(|_| invalid("network origin host is invalid"))?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(invalid("network origin must not include userinfo"));
    }
    if url.path() != "/" || url.query().is_some() || url.fragment().is_some() {
        return Err(invalid("network origin host is invalid"));
    }
    match url.host() {
        Some(url::Host::Domain(d)) => {
            let ascii = d.to_ascii_lowercase();
            if ascii.is_empty() || !ascii.is_ascii() || looks_like_ip(&ascii) {
                return Err(invalid("network origin must not use an IP literal"));
            }
            if !ascii
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
            {
                return Err(invalid("network origin host is invalid"));
            }
            for label in ascii.split('.') {
                if label.is_empty() || label.starts_with('-') || label.ends_with('-') || label.len() > 63 {
                    return Err(invalid("network origin host is invalid"));
                }
            }
            Ok(ascii)
        }
        Some(url::Host::Ipv4(_)) | Some(url::Host::Ipv6(_)) => {
            Err(invalid("network origin must not use an IP literal"))
        }
        None => Err(invalid("network origin host is invalid")),
    }
}

fn is_reserved_host(host: &str) -> bool {
    host == "localhost"
        || host == "local"
        || host == "internal"
        || host.ends_with(".localhost")
        || host.ends_with(".local")
        || host.ends_with(".internal")
}

fn looks_like_ip(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.is_empty() {
        return false;
    }
    // Dotted decimal / octal / hex IPv4 forms, and a single decimal/hex integer.
    labels.iter().all(|label| is_numeric_label(label)) && (labels.len() == 1 || labels.len() == 4)
}

fn is_numeric_label(label: &str) -> bool {
    if label.is_empty() {
        return false;
    }
    if label.bytes().all(|b| b.is_ascii_digit()) {
        return true;
    }
    if let Some(rest) = label.strip_prefix("0x").or_else(|| label.strip_prefix("0X")) {
        return !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_hexdigit());
    }
    false
}

fn parse_origin_list(origins: &[String], field: &str) -> Result<Vec<CanonicalOrigin>, ExtensionError> {
    if origins.is_empty() {
        return Err(invalid(format!("{field} must declare at least one origin")));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(origins.len());
    for raw in origins {
        let origin = parse_origin(raw)?;
        if !seen.insert(origin.clone()) {
            return Err(invalid(format!("{field} contains a duplicate origin")));
        }
        out.push(origin);
    }
    out.sort_by_key(|a| a.as_string());
    Ok(out)
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Deserialize + fail-closed validation of a `capabilities` JSON value.
pub fn parse_capabilities(value: &serde_json::Value) -> Result<ExtensionCapabilities, ExtensionError> {
    if !value.is_object() {
        return Err(invalid("capabilities must be an object"));
    }
    let caps: ExtensionCapabilities =
        serde_json::from_value(value.clone()).map_err(|e| invalid(format!("capabilities is invalid: {e}")))?;
    validate_capabilities(&caps)?;
    Ok(caps)
}

/// Structural validation of an already-deserialized `capabilities` block.
pub fn validate_capabilities(caps: &ExtensionCapabilities) -> Result<(), ExtensionError> {
    if caps.schema != CAPABILITIES_SCHEMA {
        return Err(invalid("capabilities.schema must be 1"));
    }

    if let Some(network) = &caps.network {
        if network.origins.len() > MAX_NETWORK_ORIGINS {
            return Err(invalid(format!(
                "capabilities.network.origins must have at most {MAX_NETWORK_ORIGINS} entries"
            )));
        }
        parse_origin_list(&network.origins, "capabilities.network.origins")?;
        validate_reason(&network.reason, "capabilities.network")?;
    }

    if let Some(files) = &caps.files {
        if files.data.is_none() && files.roots.is_empty() {
            return Err(invalid("capabilities.files must declare data, roots, or both"));
        }
        if let Some(data) = &files.data {
            if data.quota_bytes == 0 || data.quota_bytes > FILES_DATA_MAX_QUOTA_BYTES {
                return Err(invalid(
                    "capabilities.files.data.quotaBytes must be between 1 and 64 MiB",
                ));
            }
            validate_reason(&data.reason, "capabilities.files.data")?;
        }
        let mut ids = HashSet::new();
        for root in &files.roots {
            if !is_capability_id(&root.id) {
                return Err(invalid(
                    "capabilities.files.roots[].id must match [a-z0-9_-]{1,32} and must not be a path",
                ));
            }
            if !ids.insert(root.id.as_str()) {
                return Err(invalid("capabilities.files.roots contains a duplicate id"));
            }
            validate_reason(&root.reason, "capabilities.files.roots")?;
        }
        validate_reason(&files.reason, "capabilities.files")?;
    }

    if let Some(events) = &caps.events {
        if events.subscribe.is_empty() {
            return Err(invalid("capabilities.events.subscribe must not be empty"));
        }
        let mut seen = HashSet::new();
        for name in &events.subscribe {
            if !CAPABILITY_EVENT_NAMES.contains(&name.as_str()) {
                return Err(invalid("capabilities.events.subscribe contains an unknown event name"));
            }
            if !seen.insert(name.as_str()) {
                return Err(invalid("capabilities.events.subscribe contains a duplicate event name"));
            }
        }
        validate_reason(&events.reason, "capabilities.events")?;
    }

    if let Some(storage) = &caps.storage {
        if storage.quota_bytes == 0 || storage.quota_bytes > STORAGE_MAX_QUOTA_BYTES {
            return Err(invalid("capabilities.storage.quotaBytes must be between 1 and 32 MiB"));
        }
        if storage.max_keys == 0 || storage.max_keys > STORAGE_MAX_KEYS {
            return Err(invalid("capabilities.storage.maxKeys must be between 1 and 4096"));
        }
        validate_reason(&storage.reason, "capabilities.storage")?;
    }

    if let Some(secrets) = &caps.secrets {
        let network_origins = match &caps.network {
            Some(network) => parse_origin_list(&network.origins, "capabilities.network.origins")?,
            None if !secrets.is_empty() => {
                return Err(invalid(
                    "capabilities.secrets requires capabilities.network to be declared",
                ));
            }
            None => Vec::new(),
        };
        let mut ids = HashSet::new();
        let mut custom_headers: HashSet<String> = HashSet::new();
        for secret in secrets {
            if !is_capability_id(&secret.id) {
                return Err(invalid("capabilities.secrets[].id must match [a-z0-9_-]{1,32}"));
            }
            if !ids.insert(secret.id.as_str()) {
                return Err(invalid("capabilities.secrets contains a duplicate id"));
            }
            let origins = parse_origin_list(&secret.origins, "capabilities.secrets[].origins")?;
            for origin in &origins {
                if !network_origins.iter().any(|declared| declared.covers(origin)) {
                    return Err(invalid(
                        "capabilities.secrets[].origins must be covered by capabilities.network.origins",
                    ));
                }
            }
            canonical_inject_header(&secret.inject.header, &mut custom_headers)?;
            if let Some(scheme) = &secret.inject.scheme {
                let scheme = scheme.trim();
                if scheme.is_empty() || scheme.len() > 64 || scheme.chars().any(|c| c.is_whitespace() || c.is_control())
                {
                    return Err(invalid("capabilities.secrets[].inject.scheme is invalid"));
                }
            }
            validate_reason(&secret.reason, "capabilities.secrets")?;
        }
        if custom_headers.len() > 1 {
            return Err(invalid(
                "capabilities.secrets may declare at most one custom X-* inject header",
            ));
        }
    }

    Ok(())
}

fn canonical_inject_header(header: &str, custom_headers: &mut HashSet<String>) -> Result<String, ExtensionError> {
    let trimmed = header.trim();
    if trimmed.is_empty() || trimmed.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(invalid("capabilities.secrets[].inject.header is invalid"));
    }
    if !trimmed.bytes().all(is_http_token_byte) {
        return Err(invalid("capabilities.secrets[].inject.header is invalid"));
    }
    for standard in STANDARD_INJECT_HEADERS {
        if trimmed.eq_ignore_ascii_case(standard) {
            return Ok((*standard).to_string());
        }
    }
    if trimmed.len() >= 2 && trimmed.as_bytes()[..2].eq_ignore_ascii_case(b"x-") {
        let canonical = trimmed.to_ascii_lowercase();
        custom_headers.insert(canonical.clone());
        return Ok(canonical);
    }
    Err(invalid(
        "capabilities.secrets[].inject.header must be Authorization, X-Api-Key, X-Auth-Token, or a custom X-* header",
    ))
}

fn is_http_token_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b)
}

// ---------------------------------------------------------------------------
// Canonicalization & fingerprints
// ---------------------------------------------------------------------------

fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_owned(),
        serde_json::Value::Bool(true) => "true".to_owned(),
        serde_json::Value::Bool(false) => "false".to_owned(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => serde_json::to_string(s).expect("string is valid JSON"),
        serde_json::Value::Array(items) => {
            let body = items.iter().map(canonical_json).collect::<Vec<_>>().join(",");
            format!("[{body}]")
        }
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut body = String::new();
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    body.push(',');
                }
                body.push_str(&serde_json::to_string(key).expect("key is valid JSON"));
                body.push(':');
                body.push_str(&canonical_json(&map[*key]));
            }
            format!("{{{body}}}")
        }
    }
}

fn kind_canonical_json(caps: &ExtensionCapabilities, kind: &str) -> Result<String, ExtensionError> {
    let value = match kind {
        "network" => match &caps.network {
            None => serde_json::Value::Null,
            Some(network) => {
                let origins = parse_origin_list(&network.origins, "capabilities.network.origins")?;
                serde_json::json!({
                    "origins": origins.iter().map(CanonicalOrigin::as_string).collect::<Vec<_>>()
                })
            }
        },
        "files" => match &caps.files {
            None => serde_json::Value::Null,
            Some(files) => {
                let mut obj = serde_json::Map::new();
                if let Some(data) = &files.data {
                    obj.insert("data".into(), serde_json::json!({"quotaBytes": data.quota_bytes}));
                }
                if !files.roots.is_empty() {
                    let mut roots = files.roots.clone();
                    roots.sort_by(|a, b| a.id.cmp(&b.id));
                    let list: Vec<serde_json::Value> = roots
                        .iter()
                        .map(|root| {
                            let access = match root.access {
                                FileRootAccess::Read => "read",
                                FileRootAccess::ReadWrite => "readwrite",
                            };
                            serde_json::json!({"access": access, "id": root.id})
                        })
                        .collect();
                    obj.insert("roots".into(), serde_json::Value::Array(list));
                }
                serde_json::Value::Object(obj)
            }
        },
        "events" => match &caps.events {
            None => serde_json::Value::Null,
            Some(events) => {
                let mut names = events.subscribe.clone();
                names.sort();
                names.dedup();
                serde_json::json!({"subscribe": names})
            }
        },
        "storage" => match &caps.storage {
            None => serde_json::Value::Null,
            Some(storage) => serde_json::json!({
                "maxKeys": storage.max_keys,
                "quotaBytes": storage.quota_bytes
            }),
        },
        "secrets" => match &caps.secrets {
            None => serde_json::Value::Null,
            Some(secrets) => {
                let mut secrets = secrets.clone();
                secrets.sort_by(|a, b| a.id.cmp(&b.id));
                let mut unused = HashSet::new();
                let list: Result<Vec<serde_json::Value>, ExtensionError> = secrets
                    .iter()
                    .map(|secret| {
                        let origins = parse_origin_list(&secret.origins, "capabilities.secrets[].origins")?;
                        let header = canonical_inject_header(&secret.inject.header, &mut unused)?;
                        let mut inject = serde_json::Map::new();
                        inject.insert("header".into(), serde_json::Value::String(header));
                        if let Some(scheme) = secret.inject.scheme.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                            inject.insert("scheme".into(), serde_json::Value::String(scheme.to_owned()));
                        }
                        Ok(serde_json::json!({
                            "id": secret.id,
                            "inject": inject,
                            "origins": origins.iter().map(CanonicalOrigin::as_string).collect::<Vec<_>>()
                        }))
                    })
                    .collect();
                serde_json::Value::Array(list?)
            }
        },
        other => return Err(invalid(format!("unknown capability kind {other}"))),
    };
    Ok(canonical_json(&value))
}

/// 32-byte digest of one capability kind.
pub fn kind_fingerprint(caps: &ExtensionCapabilities, kind: &str) -> Result<[u8; 32], ExtensionError> {
    let canonical = kind_canonical_json(caps, kind)?;
    let mut hasher = Sha256::new();
    hasher.update(CAPABILITY_HASH_DOMAIN);
    hasher.update([0u8]);
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(canonical.as_bytes());
    Ok(hasher.finalize().into())
}

/// Overall `cap1:` fingerprint of a validated `capabilities` block.
pub fn capability_fingerprint(caps: &ExtensionCapabilities) -> Result<String, ExtensionError> {
    let mut hasher = Sha256::new();
    for kind in KIND_ORDER_CORE {
        hasher.update(kind_fingerprint(caps, kind)?);
    }
    if caps.secrets.is_some() {
        hasher.update(kind_fingerprint(caps, "secrets")?);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut out = String::with_capacity(CAPABILITY_FINGERPRINT_PREFIX.len() + 64);
    out.push_str(CAPABILITY_FINGERPRINT_PREFIX);
    for byte in digest {
        let _ = write!(&mut out, "{byte:02x}");
    }
    Ok(out)
}

/// True when `value` is a well-formed `cap1:` + 64 lowercase hex string.
pub fn is_capability_fingerprint_format(value: &str) -> bool {
    let Some(hex) = value.strip_prefix(CAPABILITY_FINGERPRINT_PREFIX) else {
        return false;
    };
    hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Require `capability_fingerprint` to be present and to equal the recomputed value.
pub fn validate_capability_fingerprint(
    caps: &ExtensionCapabilities,
    fingerprint: Option<&str>,
) -> Result<String, ExtensionError> {
    let computed = capability_fingerprint(caps)?;
    match fingerprint {
        None => Err(invalid(
            "capabilityFingerprint is required when capabilities is declared",
        )),
        Some(declared) if !is_capability_fingerprint_format(declared) => Err(invalid(
            "capabilityFingerprint must be cap1: followed by 64 lowercase hex characters",
        )),
        Some(declared) if declared != computed => Err(invalid(
            "capabilityFingerprint does not match the declared capabilities",
        )),
        Some(_) => Ok(computed),
    }
}

// ---------------------------------------------------------------------------
// Coherence
// ---------------------------------------------------------------------------

/// `service.uses ⊆ declared capabilities` for every background service.
pub fn validate_uses_subset(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    let declared = manifest
        .capabilities
        .as_ref()
        .map(ExtensionCapabilities::declared_kinds)
        .unwrap_or_default();
    let Some(contributes) = manifest.contributes.as_ref() else {
        return Ok(());
    };
    for service in &contributes.background_services {
        for kind in &service.uses {
            if !declared.contains(kind) {
                return Err(invalid(
                    "backgroundService uses must be a subset of the declared capabilities",
                ));
            }
        }
    }
    Ok(())
}

fn origin_host_for_permissions(origin: &CanonicalOrigin) -> String {
    if origin.wildcard {
        format!("*.{}", origin.host)
    } else {
        origin.host.clone()
    }
}

fn domain_allowed(host: &str, allowed: &[String]) -> bool {
    allowed.iter().any(|domain| domain.eq_ignore_ascii_case(host))
}

fn network_permission_covers(permission: &Option<NetworkPermission>, origins: &[CanonicalOrigin]) -> bool {
    match permission {
        Some(NetworkPermission::Unrestricted(true)) => true,
        Some(NetworkPermission::Scoped { allowed_domains, .. }) => origins
            .iter()
            .all(|origin| domain_allowed(&origin_host_for_permissions(origin), allowed_domains)),
        _ => false,
    }
}

/// `capabilities ⊆ permissions` (§Q3).
pub fn validate_capabilities_covered_by_permissions(
    caps: &ExtensionCapabilities,
    permissions: Option<&ExtPermissions>,
) -> Result<(), ExtensionError> {
    let permissions = permissions.ok_or_else(|| invalid("capabilities requires a covering permissions block"))?;

    if let Some(network) = &caps.network {
        let origins = parse_origin_list(&network.origins, "capabilities.network.origins")?;
        if !network_permission_covers(&permissions.network, &origins) {
            return Err(invalid(
                "capabilities.network.origins hosts must be covered by permissions.network.allowedDomains",
            ));
        }
    }

    if let Some(files) = &caps.files {
        match permissions.filesystem {
            Some(FilesystemScope::ExtensionOnly | FilesystemScope::Workspace | FilesystemScope::Full)
                if files.data.is_some() && files.roots.is_empty() => {}
            Some(FilesystemScope::Workspace | FilesystemScope::Full) if !files.roots.is_empty() => {}
            Some(FilesystemScope::ExtensionOnly) if !files.roots.is_empty() => {
                return Err(invalid(
                    "capabilities.files.roots requires permissions.filesystem of workspace or full",
                ));
            }
            _ => {
                return Err(invalid("capabilities.files requires a covering permissions.filesystem"));
            }
        }
    }

    if caps.events.is_some() && permissions.events != Some(true) {
        return Err(invalid("capabilities.events requires permissions.events=true"));
    }
    if caps.storage.is_some() && permissions.storage != Some(true) {
        return Err(invalid("capabilities.storage requires permissions.storage=true"));
    }

    if let Some(secrets) = &caps.secrets
        && !secrets.is_empty()
    {
        let mut origins = Vec::new();
        for secret in secrets {
            origins.extend(parse_origin_list(&secret.origins, "capabilities.secrets[].origins")?);
        }
        if !network_permission_covers(&permissions.network, &origins) {
            return Err(invalid("capabilities.secrets requires a covering permissions.network"));
        }
    }

    Ok(())
}

/// Minimal `permissions` block that covers `caps`. Used by `workmate-pack`.
pub fn permissions_covering(caps: &ExtensionCapabilities) -> Result<ExtPermissions, ExtensionError> {
    let mut permissions = ExtPermissions::default();
    if let Some(network) = &caps.network {
        let origins = parse_origin_list(&network.origins, "capabilities.network.origins")?;
        let mut domains: Vec<String> = origins.iter().map(origin_host_for_permissions).collect();
        domains.sort();
        domains.dedup();
        permissions.network = Some(NetworkPermission::Scoped {
            allowed_domains: domains,
            reasoning: network
                .reason
                .clone()
                .filter(|r| !r.is_empty())
                .unwrap_or_else(|| "Declared network origins".into()),
        });
    }
    if let Some(files) = &caps.files {
        permissions.filesystem = Some(if files.roots.is_empty() {
            FilesystemScope::ExtensionOnly
        } else {
            FilesystemScope::Workspace
        });
    }
    if caps.events.is_some() {
        permissions.events = Some(true);
    }
    if caps.storage.is_some() {
        permissions.storage = Some(true);
    }
    Ok(permissions)
}

/// True when `existing` already covers everything `required` asks for.
pub fn permissions_cover(existing: &ExtPermissions, required: &ExtPermissions) -> bool {
    let network_ok = match &required.network {
        None => true,
        Some(NetworkPermission::Unrestricted(true)) => {
            matches!(existing.network, Some(NetworkPermission::Unrestricted(true)))
        }
        Some(NetworkPermission::Scoped { allowed_domains, .. }) => match &existing.network {
            Some(NetworkPermission::Unrestricted(true)) => true,
            Some(NetworkPermission::Scoped {
                allowed_domains: have, ..
            }) => allowed_domains.iter().all(|d| domain_allowed(d, have)),
            _ => false,
        },
        Some(NetworkPermission::Unrestricted(false)) => true,
    };
    let filesystem_ok = match required.filesystem {
        None => true,
        Some(FilesystemScope::ExtensionOnly) => existing.filesystem.is_some(),
        Some(FilesystemScope::Workspace) => matches!(
            existing.filesystem,
            Some(FilesystemScope::Workspace | FilesystemScope::Full)
        ),
        Some(FilesystemScope::Full) => existing.filesystem == Some(FilesystemScope::Full),
    };
    let storage_ok = required.storage != Some(true) || existing.storage == Some(true);
    let events_ok = required.events != Some(true) || existing.events == Some(true);
    network_ok && filesystem_ok && storage_ok && events_ok
}

/// Widen `existing` just enough to cover `required`. Existing extra permissions are kept.
pub fn merge_permissions(existing: &ExtPermissions, required: &ExtPermissions) -> ExtPermissions {
    let mut out = existing.clone();
    match (&existing.network, &required.network) {
        (_, None) => {}
        (Some(NetworkPermission::Unrestricted(true)), _) => {}
        (
            Some(NetworkPermission::Scoped {
                allowed_domains,
                reasoning,
            }),
            Some(NetworkPermission::Scoped {
                allowed_domains: need, ..
            }),
        ) => {
            let mut domains = allowed_domains.clone();
            for domain in need {
                if !domain_allowed(domain, &domains) {
                    domains.push(domain.clone());
                }
            }
            out.network = Some(NetworkPermission::Scoped {
                allowed_domains: domains,
                reasoning: reasoning.clone(),
            });
        }
        (_, Some(need)) => out.network = Some(need.clone()),
    }
    out.filesystem = match (existing.filesystem, required.filesystem) {
        (None, need) => need,
        (have, None) => have,
        (Some(FilesystemScope::Full), _) | (_, Some(FilesystemScope::Full)) => Some(FilesystemScope::Full),
        (Some(FilesystemScope::Workspace), _) | (_, Some(FilesystemScope::Workspace)) => {
            Some(FilesystemScope::Workspace)
        }
        (Some(FilesystemScope::ExtensionOnly), Some(FilesystemScope::ExtensionOnly)) => {
            Some(FilesystemScope::ExtensionOnly)
        }
    };
    if required.storage == Some(true) {
        out.storage = Some(true);
    }
    if required.events == Some(true) {
        out.events = Some(true);
    }
    out
}

/// Host-side validation of the optional `capabilities` + `capabilityFingerprint` pair,
/// plus the `uses ⊆ declared` and `capabilities ⊆ permissions` coherence rules.
pub fn validate_manifest_capabilities(manifest: &ExtensionManifest) -> Result<(), ExtensionError> {
    match &manifest.capabilities {
        None => {
            if manifest.capability_fingerprint.is_some() {
                return Err(invalid("capabilityFingerprint must not be set without capabilities"));
            }
            validate_uses_subset(manifest)?;
            Ok(())
        }
        Some(caps) => {
            validate_capabilities(caps)?;
            validate_capability_fingerprint(caps, manifest.capability_fingerprint.as_deref())?;
            validate_uses_subset(manifest)?;
            validate_capabilities_covered_by_permissions(caps, manifest.permissions.as_ref())?;
            Ok(())
        }
    }
}

/// Apply fingerprint + covering `permissions` to a source manifest JSON (author-side).
///
/// Returns `true` when the JSON was changed (missing/mismatched fingerprint, or
/// missing/insufficient permissions). When `capabilities` is absent the JSON is
/// left untouched and the function returns `false`.
pub fn apply_pack_capability_metadata(manifest: &mut serde_json::Value) -> Result<bool, ExtensionError> {
    let mut view = manifest.clone();
    crate::manifest::normalize_manifest_json(&mut view);
    let Some(caps_value) = view.get("capabilities") else {
        return Ok(false);
    };
    if caps_value.is_null() {
        return Ok(false);
    }
    let caps = parse_capabilities(caps_value)?;
    let fingerprint = capability_fingerprint(&caps)?;
    let required = permissions_covering(&caps)?;

    let mut changed = false;
    let root = manifest
        .as_object_mut()
        .ok_or_else(|| invalid("manifest must be a JSON object"))?;

    let existing_fp_key = if root.contains_key("capabilityFingerprint") {
        "capabilityFingerprint"
    } else if root.contains_key("capability_fingerprint") {
        "capability_fingerprint"
    } else {
        "capabilityFingerprint"
    };
    let already = root.get(existing_fp_key).and_then(|v| v.as_str());
    if already != Some(fingerprint.as_str()) {
        root.insert(existing_fp_key.to_owned(), serde_json::Value::String(fingerprint));
        changed = true;
    }

    match root.get("permissions") {
        None => {
            let value = serde_json::to_value(&required).map_err(|e| invalid(e.to_string()))?;
            root.insert("permissions".into(), value);
            changed = true;
        }
        Some(existing_value) => {
            let existing: ExtPermissions = serde_json::from_value(existing_value.clone())
                .map_err(|e| invalid(format!("permissions is invalid: {e}")))?;
            if !permissions_cover(&existing, &required) {
                let merged = merge_permissions(&existing, &required);
                let value = serde_json::to_value(&merged).map_err(|e| invalid(e.to_string()))?;
                root.insert("permissions".into(), value);
                changed = true;
            }
        }
    }

    Ok(changed)
}

/// Minimal valid `capabilities` covering `kinds`. `Secrets` also declares a network origin.
pub fn minimal_capabilities_for(kinds: &[ServiceCapabilityKind]) -> ExtensionCapabilities {
    let needs_network = kinds
        .iter()
        .any(|k| matches!(k, ServiceCapabilityKind::Network | ServiceCapabilityKind::Secrets));
    let mut caps = ExtensionCapabilities {
        schema: CAPABILITIES_SCHEMA,
        network: None,
        files: None,
        events: None,
        storage: None,
        secrets: None,
    };
    if needs_network {
        caps.network = Some(NetworkCapabilities {
            origins: vec!["https://api.example.com".into()],
            reason: None,
        });
    }
    if kinds.contains(&ServiceCapabilityKind::Files) {
        caps.files = Some(FilesCapabilities {
            data: Some(FilesDataQuota {
                quota_bytes: FILES_DATA_DEFAULT_QUOTA_BYTES,
                reason: None,
            }),
            roots: Vec::new(),
            reason: None,
        });
    }
    if kinds.contains(&ServiceCapabilityKind::Events) {
        caps.events = Some(EventsCapabilities {
            subscribe: vec!["extension.activated".into()],
            reason: None,
        });
    }
    if kinds.contains(&ServiceCapabilityKind::Storage) {
        caps.storage = Some(StorageCapabilities {
            quota_bytes: STORAGE_DEFAULT_QUOTA_BYTES,
            max_keys: STORAGE_DEFAULT_MAX_KEYS,
            reason: None,
        });
    }
    if kinds.contains(&ServiceCapabilityKind::Secrets) {
        caps.secrets = Some(vec![SecretCapability {
            id: "tok".into(),
            origins: vec!["https://api.example.com".into()],
            inject: SecretInject {
                header: "Authorization".into(),
                scheme: Some("Bearer".into()),
            },
            reason: None,
        }]);
    }
    caps
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse_ok(value: serde_json::Value) -> ExtensionCapabilities {
        parse_capabilities(&value).expect("valid capabilities")
    }

    fn parse_err(value: serde_json::Value) -> String {
        match parse_capabilities(&value) {
            Err(ExtensionError::ManifestValidation(msg)) => msg,
            other => panic!("expected ManifestValidation, got {other:?}"),
        }
    }

    fn schema1(extra: serde_json::Value) -> serde_json::Value {
        let mut value = json!({"schema": 1});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        value
    }

    #[test]
    fn schema_must_be_one() {
        assert!(parse_err(json!({"schema": 2})).contains("schema"));
        assert!(parse_err(json!({"schema": 0})).contains("schema"));
        assert!(parse_err(json!({"network": {"origins": ["https://api.example.com"]}})).contains("missing field"));
        assert!(parse_capabilities(&json!({"schema": 1})).is_ok());
    }

    #[test]
    fn deny_unknown_fields_including_network_private() {
        let msg = parse_err(schema1(json!({
            "network": {"origins": ["https://api.example.com"], "private": true}
        })));
        assert!(msg.contains("private") || msg.contains("unknown"), "{msg}");
        let msg = parse_err(schema1(json!({"camera": true})));
        assert!(msg.contains("unknown") || msg.contains("camera"), "{msg}");
    }

    #[test]
    fn origin_acceptance_and_rejection_vectors() {
        assert!(
            parse_err(schema1(json!({
                "network": {"origins": ["https://api.example.com", "https://API.EXAMPLE.COM:443"]}
            })))
            .contains("duplicate")
        );

        let accepted = parse_ok(schema1(json!({
            "network": {"origins": [
                "https://api.example.com",
                "https://*.cdn.example.com",
                "https://exämple.com:8443"
            ]}
        })));
        let origins = parse_origin_list(&accepted.network.unwrap().origins, "t").unwrap();
        assert_eq!(
            origins.iter().map(CanonicalOrigin::as_string).collect::<Vec<_>>(),
            [
                "https://*.cdn.example.com",
                "https://api.example.com",
                "https://xn--exmple-cua.com:8443"
            ]
        );

        let userinfo_user = format!("https://{user}@{host}", user = "user", host = "api.example.com");
        let userinfo_both = format!(
            "https://{user}:{secret}@{host}",
            user = "user",
            secret = "secret",
            host = "api.example.com"
        );
        let rejected = vec![
            "http://api.example.com".to_owned(),
            userinfo_user,
            userinfo_both,
            "https://api.example.com/path".to_owned(),
            "https://api.example.com?q=1".to_owned(),
            "https://api.example.com#frag".to_owned(),
            "https://localhost".to_owned(),
            "https://localhost:8443".to_owned(),
            "https://foo.localhost".to_owned(),
            "https://printer.local".to_owned(),
            "https://api.internal".to_owned(),
            "https://127.0.0.1".to_owned(),
            "https://127.0.0.1:443".to_owned(),
            "https://0x7f.0.0.1".to_owned(),
            "https://0177.0.0.1".to_owned(),
            "https://2130706433".to_owned(),
            "https://[::1]".to_owned(),
            "https://[::ffff:127.0.0.1]".to_owned(),
            "https://*".to_owned(),
            "https://*.".to_owned(),
            "https://*.*.example.com".to_owned(),
            "https://foo.*.example.com".to_owned(),
            "".to_owned(),
            "https://".to_owned(),
            "ftp://example.com".to_owned(),
            "https://intranet".to_owned(),
        ];
        for raw in rejected {
            let msg = parse_err(schema1(json!({"network": {"origins": [raw]}})));
            assert!(!msg.is_empty(), "{raw} -> {msg}");
        }

        let many: Vec<String> = (0..17).map(|i| format!("https://h{i}.example.com")).collect();
        assert!(parse_err(schema1(json!({"network": {"origins": many}}))).contains("at most"));
    }

    #[test]
    fn files_data_shorthand_and_roots() {
        let caps = parse_ok(schema1(json!({"files": {"data": true}})));
        assert_eq!(
            caps.files.as_ref().unwrap().data.as_ref().unwrap().quota_bytes,
            FILES_DATA_DEFAULT_QUOTA_BYTES
        );

        let caps = parse_ok(schema1(json!({"files": {"data": {"quota_bytes": 1024}}})));
        assert_eq!(caps.files.unwrap().data.unwrap().quota_bytes, 1024);

        assert!(parse_err(schema1(json!({"files": {"data": {"quotaBytes": 0}}}))).contains("quotaBytes"));
        assert!(
            parse_err(schema1(
                json!({"files": {"data": {"quotaBytes": 64 * 1024 * 1024 + 1}}})
            ))
            .contains("quotaBytes")
        );
        assert!(parse_err(schema1(json!({"files": {}}))).contains("data"));
        assert!(parse_err(schema1(json!({"files": {"roots": [{"id": "../x", "access": "read"}]}}))).contains("id"));
        assert!(parse_err(schema1(json!({"files": {"roots": [{"id": "a/b", "access": "read"}]}}))).contains("id"));
        assert!(
            parse_err(schema1(
                json!({"files": {"roots": [{"id": "Reports", "access": "read"}]}})
            ))
            .contains("id")
        );
        assert!(
            parse_err(schema1(json!({
                "files": {"roots": [{"id": "a", "access": "read"}, {"id": "a", "access": "readwrite"}]}
            })))
            .contains("duplicate")
        );
        let bad_access = parse_err(schema1(json!({"files": {"roots": [{"id": "a", "access": "write"}]}})));
        assert!(!bad_access.is_empty(), "{bad_access}");

        let caps = parse_ok(schema1(json!({
            "files": {"roots": [
                {"id": "export", "access": "readwrite"},
                {"id": "reports", "access": "read", "reason": "ok"}
            ]}
        })));
        assert_eq!(caps.files.unwrap().roots.len(), 2);
    }

    #[test]
    fn events_must_be_the_host_whitelist() {
        assert!(parse_err(schema1(json!({"events": {"subscribe": ["turn.completed"]}}))).contains("unknown"));
        assert!(parse_err(schema1(json!({"events": {"subscribe": ["EXTENSION_ACTIVATED"]}}))).contains("unknown"));
        assert!(
            parse_err(schema1(
                json!({"events": {"subscribe": ["extension.activated", "extension.activated"]}})
            ))
            .contains("duplicate")
        );
        assert!(
            parse_capabilities(&schema1(json!({"events": {"subscribe": [
                "extension.activated", "extension.deactivated", "extension.installed",
                "extension.uninstalled", "registry.reloaded", "states.persisted"
            ]}})))
            .is_ok()
        );
    }

    #[test]
    fn storage_shorthand_and_bounds() {
        let caps = parse_ok(schema1(json!({"storage": true})));
        let storage = caps.storage.unwrap();
        assert_eq!(storage.quota_bytes, STORAGE_DEFAULT_QUOTA_BYTES);
        assert_eq!(storage.max_keys, STORAGE_DEFAULT_MAX_KEYS);
        assert!(parse_err(schema1(json!({"storage": false}))).contains("storage"));
        assert!(parse_err(schema1(json!({"storage": {"quotaBytes": 0}}))).contains("quotaBytes"));
        assert!(parse_err(schema1(json!({"storage": {"maxKeys": 0}}))).contains("maxKeys"));
        assert!(parse_err(schema1(json!({"storage": {"quotaBytes": 32 * 1024 * 1024 + 1}}))).contains("quotaBytes"));
        assert!(parse_err(schema1(json!({"storage": {"maxKeys": 4097}}))).contains("maxKeys"));
    }

    #[test]
    fn secrets_require_covered_origins_and_header_whitelist() {
        assert!(parse_err(schema1(json!({
            "secrets": [{"id": "tok", "origins": ["https://api.example.com"], "inject": {"header": "Authorization"}}]
        })))
        .contains("network"));

        let ok = schema1(json!({
            "network": {"origins": ["https://api.example.com", "https://*.cdn.example.com"]},
            "secrets": [{
                "id": "tok",
                "origins": ["https://api.example.com"],
                "inject": {"header": "authorization", "scheme": "Bearer"}
            }]
        }));
        assert!(parse_capabilities(&ok).is_ok());

        assert!(parse_err(schema1(json!({
            "network": {"origins": ["https://api.example.com"]},
            "secrets": [{"id": "tok", "origins": ["https://evil.example.com"], "inject": {"header": "Authorization"}}]
        })))
        .contains("covered"));

        // A wildcard covers exactly one extra label.
        assert!(
            parse_capabilities(&schema1(json!({
                "network": {"origins": ["https://*.cdn.example.com"]},
                "secrets": [{"id": "tok", "origins": ["https://a.cdn.example.com"], "inject": {"header": "X-Api-Key"}}]
            })))
            .is_ok()
        );
        assert!(parse_err(schema1(json!({
            "network": {"origins": ["https://*.cdn.example.com"]},
            "secrets": [{"id": "tok", "origins": ["https://b.a.cdn.example.com"], "inject": {"header": "X-Api-Key"}}]
        })))
        .contains("covered"));

        let two_custom = schema1(json!({
            "network": {"origins": ["https://api.example.com"]},
            "secrets": [
                {"id": "a", "origins": ["https://api.example.com"], "inject": {"header": "X-Foo"}},
                {"id": "b", "origins": ["https://api.example.com"], "inject": {"header": "X-Bar"}}
            ]
        }));
        assert!(parse_err(two_custom).contains("custom"));

        assert!(
            parse_err(schema1(json!({
                "network": {"origins": ["https://api.example.com"]},
                "secrets": [{"id": "tok", "origins": ["https://api.example.com"], "inject": {"header": "Cookie"}}]
            })))
            .contains("header")
        );
    }

    #[test]
    fn reason_is_optional_and_bounded_and_ignored_by_fingerprint() {
        let with = parse_ok(schema1(json!({
            "storage": {"quotaBytes": 1048576, "maxKeys": 256, "reason": "kv"}
        })));
        let without = parse_ok(schema1(json!({"storage": true})));
        assert_eq!(
            capability_fingerprint(&with).unwrap(),
            capability_fingerprint(&without).unwrap()
        );
        let long = "x".repeat(201);
        assert!(parse_err(schema1(json!({"storage": {"reason": long}}))).contains("reason"));
    }

    #[test]
    fn fingerprint_is_stable_and_format_checked() {
        let caps = parse_ok(schema1(json!({
            "network": {"origins": ["https://API.Example.com:443", "https://*.cdn.example.com"], "reason": "n"},
            "files": {"data": true, "roots": [
                {"id": "export", "access": "readwrite", "reason": "w"},
                {"id": "reports", "access": "read"}
            ]},
            "events": {"subscribe": ["registry.reloaded", "extension.activated"]},
            "storage": true
        })));
        let fp = capability_fingerprint(&caps).unwrap();
        assert!(is_capability_fingerprint_format(&fp), "{fp}");
        assert_eq!(fp, capability_fingerprint(&caps).unwrap());

        // No secrets -> four-kind formula. Declaring secrets changes the digest.
        let mut with_secrets = caps.clone();
        with_secrets.secrets = Some(vec![SecretCapability {
            id: "tok".into(),
            origins: vec!["https://api.example.com".into()],
            inject: SecretInject {
                header: "Authorization".into(),
                scheme: Some("Bearer".into()),
            },
            reason: None,
        }]);
        let fp_secrets = capability_fingerprint(&with_secrets).unwrap();
        assert_ne!(fp, fp_secrets);
        assert!(is_capability_fingerprint_format(&fp_secrets));

        // Empty secrets array is "declared" and therefore uses the five-kind formula.
        let mut empty_secrets = caps.clone();
        empty_secrets.secrets = Some(vec![]);
        assert_ne!(capability_fingerprint(&empty_secrets).unwrap(), fp);

        validate_capability_fingerprint(&caps, Some(&fp)).unwrap();
        assert!(validate_capability_fingerprint(&caps, None).is_err());
        assert!(validate_capability_fingerprint(&caps, Some("cap1:deadbeef")).is_err());
        let mut bad = fp.clone();
        let ch = if fp.as_bytes()[5] == b'0' { '1' } else { '0' };
        bad.replace_range(5..6, &ch.to_string());
        assert!(validate_capability_fingerprint(&caps, Some(&bad)).is_err());
    }

    #[test]
    fn golden_canonical_strings() {
        let caps = parse_ok(schema1(json!({
            "network": {"origins": ["https://api.example.com"]},
            "storage": true
        })));
        let fp = capability_fingerprint(&caps).unwrap();
        assert_eq!(
            kind_canonical_json(&caps, "network").unwrap(),
            r#"{"origins":["https://api.example.com"]}"#
        );
        assert_eq!(kind_canonical_json(&caps, "files").unwrap(), "null");
        assert_eq!(kind_canonical_json(&caps, "events").unwrap(), "null");
        assert_eq!(
            kind_canonical_json(&caps, "storage").unwrap(),
            r#"{"maxKeys":256,"quotaBytes":1048576}"#
        );
        assert_eq!(fp.len(), 69);
        assert!(fp.starts_with("cap1:"));
        // Frozen digest for the four-kind (network + storage defaults) declaration above.
        assert_eq!(
            fp,
            "cap1:8e55c848280fd88cd9cb9c5c79ca65b6a1a8b9357250ffe89dca54bd257b1e5a"
        );
    }

    #[test]
    fn uses_and_permissions_coherence() {
        let caps = minimal_capabilities_for(&[ServiceCapabilityKind::Network, ServiceCapabilityKind::Storage]);
        let perms = permissions_covering(&caps).unwrap();
        validate_capabilities_covered_by_permissions(&caps, Some(&perms)).unwrap();
        assert!(validate_capabilities_covered_by_permissions(&caps, None).is_err());
        let mut narrow = perms.clone();
        narrow.storage = Some(false);
        assert!(validate_capabilities_covered_by_permissions(&caps, Some(&narrow)).is_err());
        let mut wrong_domain = perms.clone();
        wrong_domain.network = Some(NetworkPermission::Scoped {
            allowed_domains: vec!["other.example.com".into()],
            reasoning: "no".into(),
        });
        assert!(validate_capabilities_covered_by_permissions(&caps, Some(&wrong_domain)).is_err());

        let with_roots = parse_ok(schema1(json!({
            "files": {"roots": [{"id": "docs", "access": "read"}]}
        })));
        let mut ext_only = ExtPermissions {
            filesystem: Some(FilesystemScope::ExtensionOnly),
            ..Default::default()
        };
        assert!(validate_capabilities_covered_by_permissions(&with_roots, Some(&ext_only)).is_err());
        ext_only.filesystem = Some(FilesystemScope::Workspace);
        validate_capabilities_covered_by_permissions(&with_roots, Some(&ext_only)).unwrap();
    }

    #[test]
    fn pack_metadata_is_idempotent() {
        let mut manifest = json!({
            "name": "demo.plugin",
            "version": "1.0.0",
            "capabilities": {"schema": 1, "storage": true}
        });
        assert!(apply_pack_capability_metadata(&mut manifest).unwrap());
        let first = manifest.clone();
        assert!(
            manifest
                .get("capabilityFingerprint")
                .and_then(|v| v.as_str())
                .unwrap()
                .starts_with("cap1:")
        );
        assert_eq!(manifest["permissions"]["storage"], json!(true));
        assert!(!apply_pack_capability_metadata(&mut manifest).unwrap());
        assert_eq!(manifest, first);
    }
}
