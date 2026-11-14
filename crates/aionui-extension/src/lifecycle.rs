use std::path::Path;

use aionui_runtime::Builder as CmdBuilder;
use tracing::{info, warn};

use crate::constants::{
    LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS, LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
    LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
};
use crate::error::ExtensionError;
use crate::types::LifecycleHooks;

/// Which lifecycle hook to execute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    OnInstall,
    OnUninstall,
    OnActivate,
    OnDeactivate,
}

impl HookKind {
    /// Default timeout in seconds for this hook kind.
    pub fn timeout_secs(self) -> u64 {
        match self {
            Self::OnInstall => LIFECYCLE_ON_INSTALL_TIMEOUT_SECS,
            Self::OnUninstall => LIFECYCLE_ON_UNINSTALL_TIMEOUT_SECS,
            Self::OnActivate => LIFECYCLE_ON_ACTIVATE_TIMEOUT_SECS,
            Self::OnDeactivate => LIFECYCLE_ON_DEACTIVATE_TIMEOUT_SECS,
        }
    }

    /// Human-readable label for logging and error messages.
    pub fn label(self) -> &'static str {
        match self {
            Self::OnInstall => "onInstall",
            Self::OnUninstall => "onUninstall",
            Self::OnActivate => "onActivate",
            Self::OnDeactivate => "onDeactivate",
        }
    }
}

/// Resolve the hook script path from the manifest for a given hook kind.
///
/// Only a plain relative path inside the extension directory is accepted (no absolute path, drive
/// prefix, `..` or `.` component, backslash or empty segment); anything else is treated as "no
/// hook" so the runner never executes a program outside the extension's own tree.
pub fn resolve_hook_path(hooks: &LifecycleHooks, kind: HookKind) -> Option<&str> {
    let value = match kind {
        HookKind::OnInstall => hooks.on_install.as_deref(),
        HookKind::OnUninstall => hooks.on_uninstall.as_deref(),
        HookKind::OnActivate => hooks.on_activate.as_deref(),
        HookKind::OnDeactivate => hooks.on_deactivate.as_deref(),
    };
    value.filter(|s| is_contained_hook_path(s))
}

fn is_contained_hook_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\\')
        && !path.contains(':')
        && !path.starts_with('/')
        && path.split('/').all(|seg| !seg.is_empty() && seg != "." && seg != "..")
}

/// The lifecycle hooks the Host may run for `manifest`, or `None`.
///
/// Hooks are shell / native programs started **by path**. A signed-layout manifest (one with a
/// top-level `files` list) never gets them: signed extensions are only ever read through their
/// retained root and verified bytes, and a bound spawn of shell / native code is permanently refused
/// (E13 design §3; internal security review F2). Signed installs already refuse manifests that
/// declare hooks (`package_lifecycle_hooks_refused`); this is the runtime backstop.
pub fn runnable_lifecycle_hooks(manifest: &crate::types::ExtensionManifest) -> Option<&LifecycleHooks> {
    let hooks = manifest.lifecycle.as_ref()?;
    if !manifest.files.is_empty() {
        tracing::warn!(
            extension = %manifest.name,
            "lifecycle hooks of a signed-layout extension are never run"
        );
        return None;
    }
    Some(hooks)
}

/// Whether `hooks` declares at least one hook (any kind, any value).
pub fn declares_any_hook(hooks: &LifecycleHooks) -> bool {
    [
        &hooks.on_install,
        &hooks.on_uninstall,
        &hooks.on_activate,
        &hooks.on_deactivate,
    ]
    .iter()
    .any(|h| h.as_deref().is_some_and(|s| !s.trim().is_empty()))
}

/// Execute a lifecycle hook script in a child process.
///
/// - `ext_dir`: absolute path to the extension root directory (used as cwd).
/// - `hook_path`: script path relative to `ext_dir`.
/// - `kind`: which hook is being executed (determines timeout and label).
/// - `extension_name`: used for logging and error context.
///
/// Returns `Ok(())` on success. Returns an error if the script is not found,
/// times out, or exits with a non-zero status.
pub async fn execute_hook(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
) -> Result<(), ExtensionError> {
    execute_hook_with_timeout(
        ext_dir,
        hook_path,
        kind,
        extension_name,
        std::time::Duration::from_secs(kind.timeout_secs()),
    )
    .await
}

/// [`execute_hook`] with an explicit deadline (the production callers always pass the per-kind
/// default). Exists so the timeout path is testable through the real runner instead of a copy of it.
pub async fn execute_hook_with_timeout(
    ext_dir: &Path,
    hook_path: &str,
    kind: HookKind,
    extension_name: &str,
    timeout: std::time::Duration,
) -> Result<(), ExtensionError> {
    let script = ext_dir.join(hook_path);

    if !script.exists() {
        warn!(
            extension = extension_name,
            hook = kind.label(),
            path = %script.display(),
            "lifecycle hook script not found, skipping"
        );
        return Err(ExtensionError::HookNotFound(script.display().to_string()));
    }

    let timeout_secs = timeout.as_secs();
    let label = kind.label();

    info!(
        extension = extension_name,
        hook = label,
        path = %script.display(),
        timeout_secs,
        "executing lifecycle hook"
    );

    // The retry for a busy executable lives inside the deadline: it never extends the timeout.
    let result = tokio::time::timeout(timeout, run_script_retrying_busy(&script, ext_dir)).await;

    match result {
        Err(_elapsed) => {
            warn!(
                extension = extension_name,
                hook = label,
                timeout_secs,
                "lifecycle hook timed out"
            );
            Err(ExtensionError::HookTimeout {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                timeout_secs,
            })
        }
        Ok(Err(io_err)) => {
            warn!(
                extension = extension_name,
                hook = label,
                error = %io_err,
                "lifecycle hook I/O error"
            );
            Err(ExtensionError::HookFailed {
                extension_name: extension_name.to_owned(),
                hook: label.to_owned(),
                reason: io_err.to_string(),
            })
        }
        Ok(Ok(output)) => {
            if output.status.success() {
                info!(
                    extension = extension_name,
                    hook = label,
                    "lifecycle hook completed successfully"
                );
                Ok(())
            } else {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let code = output
                    .status
                    .code()
                    .map_or_else(|| "signal".to_owned(), |c| c.to_string());
                warn!(
                    extension = extension_name,
                    hook = label,
                    exit_code = %code,
                    stderr = %stderr,
                    "lifecycle hook exited with error"
                );
                Err(ExtensionError::HookFailed {
                    extension_name: extension_name.to_owned(),
                    hook: label.to_owned(),
                    reason: format!("exit code {code}: {}", stderr.trim()),
                })
            }
        }
    }
}

/// How often a spawn that failed with `ETXTBSY` is retried, and the pause between attempts
/// (`step * attempt`, capped). The worst case adds about two seconds, inside the hook deadline.
const BUSY_RETRIES: u32 = 20;
const BUSY_STEP: std::time::Duration = std::time::Duration::from_millis(10);
const BUSY_MAX_PAUSE: std::time::Duration = std::time::Duration::from_millis(100);

/// `execve` refused the script because some process still has it open for writing (`ETXTBSY`).
fn is_executable_busy(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::ExecutableFileBusy
}

/// Run the hook script, retrying **only** a spawn that failed with `ETXTBSY`.
///
/// Why this is a real race and not a flaky test: Unix `fork` duplicates every open file descriptor
/// into the child until it reaches `exec` (the `O_CLOEXEC` flag only closes them there). A thread
/// that has just written the script — installing or extracting an extension — holds a writable
/// descriptor for a moment; if any other thread of the process forks (any other spawn: another hook,
/// a CLI probe) in that moment, the forked child keeps a copy open for writing until its own exec,
/// and `exec` of the script fails with "Text file busy" even though the writer already closed it.
/// Nothing was executed on that failure, so retrying is safe, and the window is microseconds to
/// milliseconds. Every other error (not found, permission, exit status, timeout) is reported
/// exactly as before and never retried.
async fn run_script_retrying_busy(script: &Path, ext_dir: &Path) -> std::io::Result<std::process::Output> {
    let mut attempt: u32 = 0;
    loop {
        let mut builder = CmdBuilder::clean_cli(script);
        builder.current_dir(ext_dir);
        match builder.output().await {
            Err(error) if is_executable_busy(&error) && attempt < BUSY_RETRIES => {
                attempt += 1;
                tokio::time::sleep((BUSY_STEP * attempt).min(BUSY_MAX_PAUSE)).await;
            }
            other => return other,
        }
    }
}

/// Determine whether the `onInstall` hook should run.
///
/// Returns `true` when:
/// - There is no persisted version (first-time install).
/// - The persisted version differs from the current manifest version.
pub fn needs_install_hook(current_version: &str, persisted_version: Option<&str>) -> bool {
    match persisted_version {
        None => true,
        Some(prev) => prev != current_version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn make_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // -----------------------------------------------------------------------
    // needs_install_hook
    // -----------------------------------------------------------------------

    #[test]
    fn test_needs_install_first_time() {
        assert!(needs_install_hook("1.0.0", None));
    }

    #[test]
    fn test_needs_install_version_changed() {
        assert!(needs_install_hook("2.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_no_install_same_version() {
        assert!(!needs_install_hook("1.0.0", Some("1.0.0")));
    }

    #[test]
    fn test_needs_install_downgrade() {
        assert!(needs_install_hook("0.9.0", Some("1.0.0")));
    }

    // -----------------------------------------------------------------------
    // HookKind
    // -----------------------------------------------------------------------

    #[test]
    fn test_hook_kind_timeout_values() {
        assert_eq!(HookKind::OnInstall.timeout_secs(), 120);
        assert_eq!(HookKind::OnUninstall.timeout_secs(), 60);
        assert_eq!(HookKind::OnActivate.timeout_secs(), 30);
        assert_eq!(HookKind::OnDeactivate.timeout_secs(), 30);
    }

    #[test]
    fn test_hook_kind_labels() {
        assert_eq!(HookKind::OnInstall.label(), "onInstall");
        assert_eq!(HookKind::OnUninstall.label(), "onUninstall");
        assert_eq!(HookKind::OnActivate.label(), "onActivate");
        assert_eq!(HookKind::OnDeactivate.label(), "onDeactivate");
    }

    // -----------------------------------------------------------------------
    // resolve_hook_path
    // -----------------------------------------------------------------------

    #[test]
    fn hook_paths_outside_the_extension_tree_are_never_resolved() {
        for bad in [
            "/bin/sh",
            "../other/run.sh",
            "scripts/../../run.sh",
            "./run.sh",
            "scripts//run.sh",
            "C:/Windows/system32/cmd.exe",
            "scripts\\run.bat",
            "",
        ] {
            let hooks = LifecycleHooks {
                on_activate: Some(bad.into()),
                ..Default::default()
            };
            assert_eq!(resolve_hook_path(&hooks, HookKind::OnActivate), None, "{bad:?}");
        }
        let hooks = LifecycleHooks {
            on_activate: Some("scripts/activate.sh".into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnActivate),
            Some("scripts/activate.sh")
        );
    }

    #[test]
    fn signed_layout_manifests_never_get_runnable_hooks() {
        let mut manifest: crate::types::ExtensionManifest = serde_json::from_value(serde_json::json!({
            "name": "demo",
            "version": "1.0.0",
            "lifecycle": {"on_activate": "scripts/activate.sh"}
        }))
        .unwrap();
        assert!(runnable_lifecycle_hooks(&manifest).is_some());
        manifest.files = vec![
            serde_json::from_value(serde_json::json!({"path": "scripts/activate.sh", "integrity": "sha512-x"}))
                .unwrap(),
        ];
        assert!(runnable_lifecycle_hooks(&manifest).is_none());
        assert!(declares_any_hook(manifest.lifecycle.as_ref().unwrap()));
        assert!(!declares_any_hook(&LifecycleHooks::default()));
    }

    #[test]
    fn test_resolve_hook_path_present() {
        let hooks = LifecycleHooks {
            on_install: Some("scripts/install.sh".into()),
            on_activate: Some("scripts/activate.sh".into()),
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnInstall),
            Some("scripts/install.sh")
        );
        assert_eq!(
            resolve_hook_path(&hooks, HookKind::OnActivate),
            Some("scripts/activate.sh")
        );
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnDeactivate), None);
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnUninstall), None);
    }

    #[test]
    fn test_resolve_hook_path_empty_string() {
        let hooks = LifecycleHooks {
            on_install: Some(String::new()),
            on_activate: None,
            on_deactivate: None,
            on_uninstall: None,
        };
        assert_eq!(resolve_hook_path(&hooks, HookKind::OnInstall), None);
    }

    // -----------------------------------------------------------------------
    // execute_hook (async unit tests)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_execute_hook_script_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let result = execute_hook(dir.path(), "nonexistent.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, ExtensionError::HookNotFound(_)));
    }

    // Hook scripts are POSIX `#!/bin/sh` files run directly; hooks do not run on Windows (not part of
    // the signed-install path), so this is unix-only by design.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_hook_success() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("hook.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "hook.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
    }

    // Hook scripts are POSIX `#!/bin/sh` files run directly; hooks do not run on Windows (not part of
    // the signed-install path), so this is unix-only by design.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_hook_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("fail.sh");
        std::fs::write(&script_path, "#!/bin/sh\necho 'something broke' >&2\nexit 1\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "fail.sh", HookKind::OnInstall, "test-ext").await;

        assert!(result.is_err());
        match result.unwrap_err() {
            ExtensionError::HookFailed {
                extension_name,
                hook,
                reason,
            } => {
                assert_eq!(extension_name, "test-ext");
                assert_eq!(hook, "onInstall");
                assert!(reason.contains("something broke"));
            }
            other => panic!("expected HookFailed, got {other:?}"),
        }
    }

    // Hook scripts are POSIX `#!/bin/sh` files run directly; hooks do not run on Windows (not part of
    // the signed-install path), so this is unix-only by design.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_hook_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("slow.sh");
        // `exec` so the timeout kills the sleeper itself, not just its parent shell.
        std::fs::write(&script_path, "#!/bin/sh\nexec sleep 60\n").unwrap();
        make_executable(&script_path);

        let result = execute_hook_with_timeout(
            dir.path(),
            "slow.sh",
            HookKind::OnActivate,
            "test-ext",
            std::time::Duration::from_millis(300),
        )
        .await;

        assert!(
            matches!(result, Err(ExtensionError::HookTimeout { .. })),
            "expected HookTimeout, got {result:?}"
        );
    }

    /// Deterministic reproduction of the fork/exec race: a writable descriptor on the script makes
    /// `exec` fail with ETXTBSY exactly like a forked sibling holding a copy. The runner must wait
    /// it out (the descriptor is released after a short pause) and then run the hook.
    // Linux enforces ETXTBSY on exec of a file open for writing; the premise assert guards that.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_execute_hook_waits_out_a_busy_executable() {
        let dir = tempfile::tempdir().unwrap();
        let script_path = dir.path().join("busy.sh");
        std::fs::write(&script_path, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&script_path);
        let writer = std::fs::OpenOptions::new().write(true).open(&script_path).unwrap();
        // Prove the premise: while the writer is open the script cannot be executed.
        let direct = std::process::Command::new(&script_path).output();
        assert!(
            matches!(&direct, Err(e) if is_executable_busy(e)),
            "premise: exec must fail with ETXTBSY, got {direct:?}"
        );
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            drop(writer);
        });

        let result = execute_hook(dir.path(), "busy.sh", HookKind::OnActivate, "test-ext").await;
        releaser.join().unwrap();
        assert!(result.is_ok(), "{result:?}");
    }

    /// Only ETXTBSY is retried: any other spawn error is reported at once, and the retry never
    /// outlives the hook deadline.
    // Linux enforces ETXTBSY on exec of a file open for writing; the premise assert guards that.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn test_execute_hook_busy_retry_respects_the_deadline_and_other_errors() {
        let dir = tempfile::tempdir().unwrap();
        // Not executable: PermissionDenied, reported immediately as HookFailed.
        let plain = dir.path().join("plain.sh");
        std::fs::write(&plain, "#!/bin/sh\nexit 0\n").unwrap();
        let started = std::time::Instant::now();
        let result = execute_hook(dir.path(), "plain.sh", HookKind::OnActivate, "test-ext").await;
        assert!(matches!(result, Err(ExtensionError::HookFailed { .. })), "{result:?}");
        assert!(started.elapsed() < std::time::Duration::from_millis(500));

        // Permanently busy: gives up by the deadline with a timeout, never hangs.
        let busy = dir.path().join("busy.sh");
        std::fs::write(&busy, "#!/bin/sh\nexit 0\n").unwrap();
        make_executable(&busy);
        let _writer = std::fs::OpenOptions::new().write(true).open(&busy).unwrap();
        let result = execute_hook_with_timeout(
            dir.path(),
            "busy.sh",
            HookKind::OnActivate,
            "test-ext",
            std::time::Duration::from_millis(300),
        )
        .await;
        assert!(matches!(result, Err(ExtensionError::HookTimeout { .. })), "{result:?}");
    }

    // Hook scripts are POSIX `#!/bin/sh` files run directly; hooks do not run on Windows (not part of
    // the signed-install path), so this is unix-only by design.
    #[cfg(unix)]
    #[tokio::test]
    async fn test_execute_hook_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("cwd_marker.txt");
        let script_path = dir.path().join("check_cwd.sh");
        // Write cwd to a file so we can verify it
        std::fs::write(&script_path, "#!/bin/sh\npwd > cwd_marker.txt\n").unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script_path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let result = execute_hook(dir.path(), "check_cwd.sh", HookKind::OnActivate, "test-ext").await;

        assert!(result.is_ok());
        assert!(marker.exists());
        let cwd_content = std::fs::read_to_string(&marker).unwrap();
        // The cwd written by the script should match the extension dir
        // (may have symlink resolution differences, compare canonical)
        let expected = dir.path().canonicalize().unwrap();
        let actual_trimmed = cwd_content.trim();
        let actual = Path::new(actual_trimmed).canonicalize().unwrap();
        assert_eq!(actual, expected);
    }
}
