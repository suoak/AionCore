//! Application configuration parsed from CLI arguments + key derivation.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentityMode {
    Local,
    WebUi,
    AionPro,
}

impl IdentityMode {
    pub fn auth_label(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::WebUi => "webui",
            Self::AionPro => "aionpro",
        }
    }

    pub fn is_local(self) -> bool {
        self == Self::Local
    }
}

/// Launcher opt-in for handing Core the desktop keychain copy of the
/// storage-encryption root as the second stdin line (after the local API
/// secret), so it never appears on argv or in the environment inherited by
/// agent subprocesses.
pub const ENCRYPTION_STDIN_ENV: &str = "AIONUI_ENCRYPTION_STDIN";

/// Desktop keychain (Electron `safeStorage`) copy of the storage-encryption
/// root. `Debug` is redacted so it never reaches logs.
#[derive(Clone, PartialEq, Eq)]
pub struct EncryptionRoot(String);

impl EncryptionRoot {
    /// Parse one stdin line; surrounding whitespace is ignored and an empty
    /// value is treated as absent.
    pub fn parse(line: &str) -> Option<Self> {
        let value = line.trim();
        (!value.is_empty()).then(|| Self(value.to_owned()))
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EncryptionRoot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EncryptionRoot(<redacted>)")
    }
}

/// Application configuration parsed from CLI arguments.
#[derive(Debug, Clone)]
pub struct AppConfig {
    pub host: String,
    pub port: u16,
    /// Extra `Host` values accepted besides loopback and the bind address
    /// (`--allowed-hosts` + `AIONCORE_ALLOWED_HOSTS`; `*` disables the check).
    pub allowed_hosts: Vec<String>,
    pub data_dir: PathBuf,
    pub work_dir: PathBuf,
    pub app_version: String,
    /// Run in local embedded mode (skip authentication, use system_default_user).
    pub local: bool,
    pub identity_mode: IdentityMode,
    pub bootstrap_secret: Option<String>,
    /// Per-launch desktop API secret (local mode), read from stdin when the
    /// launcher sets `AIONUI_LOCAL_AUTH_STDIN=1`. `Debug` is redacted.
    pub local_api_secret: Option<aionui_auth::LocalApiSecret>,
    /// Desktop keychain copy of the storage-encryption root (E12-6), read from
    /// stdin when the launcher sets `AIONUI_ENCRYPTION_STDIN=1`. Only honoured
    /// when it equals the persisted `users.encryption_secret`.
    pub encryption_root: Option<EncryptionRoot>,
    /// Dump prompt diagnostics under `data_dir/prompt-dumps`.
    pub dump_prompts: bool,
    /// Explicitly authorize backup and rebuild for corruption-like local databases.
    pub recover_corrupted_database: bool,
}

impl AppConfig {
    pub fn effective_identity_mode(&self) -> IdentityMode {
        if self.local {
            IdentityMode::Local
        } else {
            self.identity_mode
        }
    }

    /// Format as `host:port` for socket binding.
    pub fn socket_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Local URL helpers should use to call this backend from the same machine.
    pub fn local_base_url(&self) -> String {
        let host = match self.host.as_str() {
            "0.0.0.0" | "::" => "127.0.0.1",
            other => other,
        };
        format!("http://{host}:{}", self.port)
    }

    /// Path to the SQLite database file.
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("aionui-backend.db")
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            host: aionui_common::constants::DEFAULT_HOST.to_string(),
            port: aionui_common::constants::DEFAULT_PORT,
            allowed_hosts: Vec::new(),
            data_dir: PathBuf::from("data"),
            work_dir: PathBuf::from("data"),
            app_version: env!("CARGO_PKG_VERSION").to_string(),
            local: false,
            identity_mode: IdentityMode::WebUi,
            bootstrap_secret: None,
            local_api_secret: None,
            encryption_root: None,
            dump_prompts: false,
            recover_corrupted_database: false,
        }
    }
}

/// Derive a 32-byte encryption key from the storage-encryption secret using
/// SHA-256. The domain-separation prefix is part of the on-disk contract and
/// must never change — doing so would orphan every stored ciphertext.
pub fn derive_encryption_key(secret: &str) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"aionui-encryption-key:");
    hasher.update(secret.as_bytes());
    hasher.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_app_config_default() {
        let config = AppConfig::default();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 25808);
        assert_eq!(config.data_dir, PathBuf::from("data"));
        assert_eq!(config.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(config.identity_mode, IdentityMode::WebUi);
        assert!(config.bootstrap_secret.is_none());
        assert!(!config.dump_prompts);
        assert!(!config.recover_corrupted_database);
    }

    #[test]
    fn test_app_config_socket_addr() {
        let config = AppConfig {
            host: "0.0.0.0".to_string(),
            port: 3000,
            ..Default::default()
        };
        assert_eq!(config.socket_addr(), "0.0.0.0:3000");
    }

    #[test]
    fn local_base_url_uses_loopback_for_wildcard_host() {
        let config = AppConfig {
            host: "0.0.0.0".to_string(),
            port: 49152,
            ..Default::default()
        };
        assert_eq!(config.local_base_url(), "http://127.0.0.1:49152");
    }

    #[test]
    fn test_app_config_database_path() {
        let config = AppConfig {
            data_dir: PathBuf::from("/tmp/aionui"),
            ..Default::default()
        };
        assert_eq!(config.database_path(), PathBuf::from("/tmp/aionui/aionui-backend.db"));
    }
}
