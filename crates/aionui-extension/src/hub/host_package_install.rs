//! Host-owned automatic install, upgrade and replace of an **already-local** signed package
//! (M9 E13 slices C + D).
//!
//! The customer only hands over a package; upgrading means handing over a newer one. The Host
//! supplies its own extensions parent directory handle ([`HostExtensionsParent`], opened once by
//! the Host from its own trusted location — this module never opens a path, and never derives a
//! directory from the manifest, the package or `LoadedExtension.directory`). Everything below is
//! relative to that handle:
//!
//! ```text
//! {parent}/                    Host-held directory handle
//!   {name-component}/          one directory per signed extension name
//!     current                  regular file (never a symlink): which version is installed
//!     {version-component}/     installed root (the retained handle is this very inode)
//!     .wip-{nonce}/            staging for ONE attempt
//!     .old-{nonce}/            superseded root awaiting deletion
//!     .current.tmp-{nonce}     temp file for the atomic `current` switch
//! ```
//!
//! # Naming (derived from the signed identity, never chosen by the customer)
//! `check_signed_manifest` yields the signed `name` and `version`. Each becomes one directory
//! component: bytes in `[A-Za-z0-9_-]` (and `.` after the first byte) are kept, every other byte —
//! including a leading `.`, `~`, `/`, `\`, NUL, spaces and all non-ASCII — becomes `~XX` (upper hex).
//! The mapping is injective, so two identities never share a component, and no component can be
//! `.`, `..`, start with a dot, or contain a separator. Over 255 bytes after encoding fails closed
//! (never truncated). `demo.plugin` / `1.0.0` stays readable as `demo.plugin/1.0.0`.
//!
//! # Flow (fail closed; the live binding changes only once the new root is complete)
//! 1. signature check and whole-archive validation in memory (no filesystem effect on failure);
//! 2. policy from the *installed* version (the live binding, else a valid `current` marker):
//!    newer ⇒ upgrade, equal ⇒ replace (repair), older ⇒ `DowngradeRefused` unless the Host sets
//!    [`AutoInstallOptions::allow_downgrade`] (an explicit rollback action; a signed older package
//!    replayed by anyone else must not roll an extension back);
//! 3. create/open the name directory, exclusively create `.wip-{nonce}`, write the verified files
//!    (261 `O_EXCL|O_NOFOLLOW` rules) — the old version stays untouched and live;
//! 4. place: no-replace rename to the version component when it is free; when a real directory is
//!    already there (same-version replace) an atomic **exchange** (`RENAME_EXCHANGE` / Apple
//!    `RENAME_SWAP`) puts the new tree in place and the old tree under `.old-{nonce}` in one step —
//!    there is no instant with the version directory missing (platforms without an exchange
//!    primitive refuse that case with `PlaceFailed`, still fail closed);
//! 5. atomically switch the `current` marker (temp file + `renameat`, skipped when already
//!    identical), then replace the live binding in the store. Both happen only after step 4;
//! 6. delete superseded roots (`.old-{nonce}`, and the previous version directory named by the old
//!    binding/marker) FD-relative and no-follow. In-flight users of the old root keep their open
//!    handles; path-string readers of the old tree (none are wired to this installer yet) must be
//!    switched before relying on this deletion.
//!
//! Any failure after placement undoes it (version directory removed, or exchanged back) and leaves
//! the previous binding and marker as they were.
//!
//! # `current` marker
//! A small JSON file written by the Host: marker version, signed name, signed version, the version
//! component, and the signed entry metadata (`pubkey_id`, `integrity`, `signature`) so a restarted
//! Host can re-run `check_signed_manifest`. It is not trusted by itself: restore re-verifies the
//! signature against the Host policy and the files against their SRIs (see `host_package_restore`).
//!
//! # Cleanup (deletes only what this attempt created or that is clearly superseded)
//! FD-relative, `O_NOFOLLOW`, depth-bounded; never a path string, never the parent, never anything
//! a symlink points to. [`sweep_abandoned_staging`] clears `.wip-*`, `.old-*` and stray
//! `.current.tmp-*` left by a crashed attempt; call it at Host start before installing (it cannot
//! tell a crashed attempt from a concurrent live one — the parent also serializes this process's
//! own install/sweep/restore calls with an internal lock; other processes must not share it).
//!
//! # Not
//! No route, network, exec, permission/HTTP/plan-safe change; not execution authorization or owner
//! proof beyond the Host holding `parent`. A crash between placing a *new* version directory and
//! writing the marker leaves an unreferenced version directory (harmless; replaced on the next
//! install of that version). Unix and Windows (slice W2b: handle-relative `NtCreateFile`, two-step
//! rename instead of an atomic exchange — see the Windows `imp` docs); other targets return
//! `Unsupported`.

use std::fs::File;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use super::curated::CuratedInstallPolicy;
use super::local_package_install::{LocalPackageError, LocalPackageLimits, LocalPackageSource};
use super::signed_manifest::SignatureCheckedManifest;
use crate::signed_root_binding::SignedRootBindingStore;

/// Maximum bytes of one encoded directory component (typical filesystem `NAME_MAX`).
const MAX_COMPONENT_BYTES: usize = 255;
/// Prefix of this module's staging directories.
pub(super) const STAGING_PREFIX: &str = ".wip-";
/// Prefix of superseded-root directories awaiting deletion.
pub(super) const SUPERSEDED_PREFIX: &str = ".old-";
/// Prefix of the temp file used to switch the marker atomically.
pub(super) const MARKER_TMP_PREFIX: &str = ".current.tmp-";
/// File name of the Host-owned marker inside a name directory.
pub(super) const MARKER_FILE: &str = "current";
/// The marker is a few hundred bytes; anything bigger is refused unread.
pub(super) const MAX_MARKER_BYTES: u64 = 4096;

/// The Host's own extensions parent directory handle. The caller must obtain the `File` itself
/// (from its trusted data dir at startup); there is deliberately no constructor from a path.
pub struct HostExtensionsParent {
    pub(super) dir: File,
    /// Serializes this process's install / sweep / restore calls on the parent.
    pub(super) lock: Mutex<()>,
}

impl std::fmt::Debug for HostExtensionsParent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostExtensionsParent").finish_non_exhaustive()
    }
}

impl HostExtensionsParent {
    /// Wrap a Host-opened directory handle; refuses anything that is not a directory.
    pub fn new(dir: File) -> Result<Self, LocalPackageError> {
        match dir.metadata() {
            Ok(m) if m.is_dir() => Ok(Self {
                dir,
                lock: Mutex::new(()),
            }),
            _ => Err(LocalPackageError::ParentNotDirectory),
        }
    }
}

/// Outcome of [`sweep_abandoned_staging`]: counts only, never names or paths.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StagingSweepReport {
    pub removed: usize,
    pub failed: usize,
}

/// Host choices for one install. Defaults are the safe customer path.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoInstallOptions {
    /// Allow installing an older version than the installed one. Off by default: only an explicit
    /// Host rollback action should set it.
    pub allow_downgrade: bool,
}

/// What the install did relative to what was installed before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallKind {
    /// Nothing was installed (no live binding and no valid marker).
    Fresh,
    /// A newer version replaced the installed one.
    Upgraded,
    /// The same version was installed again (atomic repair/replace).
    Reinstalled,
    /// An older version replaced the installed one (`allow_downgrade`).
    Downgraded,
}

/// Result of a successful install (no paths/handles exposed; the root lives in the store).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoInstallReport {
    pub files_written: usize,
    pub kind: InstallKind,
}

/// Host-written `current` marker content (see module docs). Not trusted on its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CurrentMarker {
    pub(super) v: u32,
    pub(super) name: String,
    pub(super) version: String,
    pub(super) component: String,
    pub(super) pubkey_id: String,
    pub(super) integrity: String,
    pub(super) signature: String,
}

impl CurrentMarker {
    /// Shape check only: the component must look like one `encode_component` output.
    pub(super) fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() as u64 > MAX_MARKER_BYTES {
            return None;
        }
        let m: Self = serde_json::from_slice(bytes).ok()?;
        (m.v == 1 && is_encoded_component(&m.component)).then_some(m)
    }
}

/// Encode one signed identity string as a single safe directory component (see module docs).
pub(crate) fn encode_component(value: &str) -> Option<String> {
    use std::fmt::Write as _;
    if value.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(value.len());
    for (i, b) in value.bytes().enumerate() {
        let keep = b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || (b == b'.' && i != 0);
        if keep {
            out.push(char::from(b));
        } else {
            write!(out, "~{b:02X}").ok()?;
        }
        if out.len() > MAX_COMPONENT_BYTES {
            return None;
        }
    }
    Some(out)
}

/// The directory component the Host uses for a signed identity on **this platform**:
/// [`encode_component`], and on Windows additionally refused when the result is a name Windows
/// cannot hold (DOS device names such as `con`/`nul`/`aux`/`com1`, trailing dot/space, …). Such an
/// extension cannot be installed on Windows (fail closed, `InvalidDirName`), never renamed.
pub(crate) fn host_component(value: &str) -> Option<String> {
    let component = encode_component(value)?;
    #[cfg(windows)]
    if super::verified_reader::windows_portable_components(&component).is_err() {
        return None;
    }
    Some(component)
}

/// True iff `entries` holds a name that equals `component` ignoring ASCII case but is not exactly
/// `component`. [`encode_component`] output is ASCII only (everything else is `~XX` with upper-case
/// hex), so ASCII case folding is exactly what a case-insensitive filesystem merges. Pure so it is
/// tested on every platform; only the Windows `imp` applies it to real directory listings.
#[cfg(any(windows, test))]
pub(crate) fn has_case_alias<S: AsRef<str>>(entries: &[S], component: &str) -> bool {
    entries
        .iter()
        .any(|e| e.as_ref() != component && e.as_ref().eq_ignore_ascii_case(component))
}

/// True iff `value` has exactly the shape `encode_component` produces.
pub(super) fn is_encoded_component(value: &str) -> bool {
    let b = value.as_bytes();
    if b.is_empty() || b.len() > MAX_COMPONENT_BYTES || b[0] == b'.' {
        return false;
    }
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'~' => {
                if i + 3 > b.len() {
                    return false;
                }
                let hex = &b[i + 1..i + 3];
                if !hex.iter().all(|c| matches!(c, b'0'..=b'9' | b'A'..=b'F')) {
                    return false;
                }
                i += 3;
            }
            c if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'.' => i += 1,
            _ => return false,
        }
    }
    true
}

/// Serialize the marker for `checked` + the signed entry metadata of `source`.
fn marker_bytes(
    checked: &SignatureCheckedManifest,
    source: &LocalPackageSource<'_>,
    version_component: &str,
) -> Result<Vec<u8>, LocalPackageError> {
    let marker = CurrentMarker {
        v: 1,
        name: checked.name().to_owned(),
        version: checked.version().to_owned(),
        component: version_component.to_owned(),
        pubkey_id: source.entry.pubkey_id.unwrap_or_default().to_owned(),
        integrity: source.entry.integrity.unwrap_or_default().to_owned(),
        signature: source.entry.signature.unwrap_or_default().to_owned(),
    };
    serde_json::to_vec(&marker).map_err(|_| LocalPackageError::MarkerWriteFailed)
}

/// Install, upgrade or replace `source` under the Host parent at `{name}/{version}` (both derived
/// from the signed manifest) and switch the extension's live binding to it.
///
/// See the module docs for the flow, the downgrade/same-version policy and the cleanup rules.
/// Nothing is retained, no live binding or marker is changed on any error.
pub fn install_local_signed_package_auto(
    store: &SignedRootBindingStore,
    policy: &CuratedInstallPolicy,
    source: &LocalPackageSource<'_>,
    parent: &HostExtensionsParent,
    limits: LocalPackageLimits,
    options: AutoInstallOptions,
) -> Result<AutoInstallReport, LocalPackageError> {
    use super::local_package_install::validate_package;
    use super::signed_manifest::check_signed_manifest;

    let checked =
        check_signed_manifest(policy, &source.entry, source.raw_manifest).map_err(LocalPackageError::Signature)?;
    let package = validate_package(&checked, source.package_zip, limits)?;
    let name_component = host_component(checked.name()).ok_or(LocalPackageError::InvalidDirName)?;
    let version_component = host_component(checked.version()).ok_or(LocalPackageError::InvalidDirName)?;
    let marker = marker_bytes(&checked, source, &version_component)?;
    let _guard = parent.lock.lock().unwrap_or_else(|e| e.into_inner());
    imp::install(
        store,
        checked,
        package,
        &parent.dir,
        &name_component,
        &version_component,
        &marker,
        options,
    )
}

/// What [`uninstall_local_signed_package`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UninstallOutcome {
    /// A live binding and/or a Host-layout directory existed and is now gone.
    Removed,
    /// Nothing under the Host layout for that name (a legacy directory install is not touched).
    NotInstalled,
}

/// Uninstall an extension that was installed through [`install_local_signed_package_auto`].
///
/// Fail closed order: unlink the `current` marker (a restart can no longer re-bind it), drop the live
/// binding, then delete the name directory FD-relative and no-follow. Only the Host-layout directory
/// for the encoded signed name under `parent` is touched; legacy directories are never read here.
pub fn uninstall_local_signed_package(
    store: &SignedRootBindingStore,
    parent: &HostExtensionsParent,
    name: &str,
) -> Result<UninstallOutcome, LocalPackageError> {
    let name_component = host_component(name).ok_or(LocalPackageError::InvalidDirName)?;
    let _guard = parent.lock.lock().unwrap_or_else(|e| e.into_inner());
    imp::uninstall(store, &parent.dir, name, &name_component)
}

/// Loader discovery helper (read-only, path based, **no trust**): for a name directory in the Host
/// layout return `{name_dir}/{component}` of the installed version named by a well-formed `current`
/// marker. The marker must be a regular file (not a symlink), the version directory a real directory,
/// and both identities must encode to exactly the directory names they were found under. The result
/// only says where to look for a manifest; trust comes solely from restore / install bindings.
#[cfg(any(unix, windows))]
pub(crate) fn resolve_current_version_dir(name_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    use std::io::Read as _;
    let marker_path = name_dir.join(MARKER_FILE);
    let meta = std::fs::symlink_metadata(&marker_path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MARKER_BYTES {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&marker_path)
        .ok()?
        .take(MAX_MARKER_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    let marker = CurrentMarker::parse(&bytes)?;
    if encode_component(&marker.name).as_deref() != Some(name_dir.file_name()?.to_str()?)
        || encode_component(&marker.version).as_deref() != Some(marker.component.as_str())
    {
        return None;
    }
    let version_dir = name_dir.join(&marker.component);
    std::fs::symlink_metadata(&version_dir)
        .ok()?
        .is_dir()
        .then_some(version_dir)
}

/// True iff `name` / `version` encode to exactly the directory names of `name_dir` / `version_dir`.
pub(crate) fn loaded_identity_matches_layout(
    name: &str,
    version: &str,
    name_dir: &std::path::Path,
    version_dir: &std::path::Path,
) -> bool {
    let leaf = |p: &std::path::Path| p.file_name().and_then(|n| n.to_str()).map(str::to_owned);
    encode_component(name).is_some_and(|c| Some(c) == leaf(name_dir))
        && encode_component(version).is_some_and(|c| Some(c) == leaf(version_dir))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn resolve_current_version_dir(_name_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    None
}

/// Random 64-bit hex nonce for staging / superseded / temp names.
#[cfg(any(unix, windows))]
pub(super) fn nonce() -> Option<String> {
    let mut bytes = [0u8; 8];
    getrandom::getrandom(&mut bytes).ok()?;
    Some(hex::encode(bytes))
}

#[cfg(any(unix, windows))]
/// Decide what this install is, from the installed version (binding first, else valid marker).
pub(super) fn classify(
    prior: Option<&str>,
    new: &str,
    allow_downgrade: bool,
) -> Result<InstallKind, LocalPackageError> {
    let Some(prior) = prior else {
        return Ok(InstallKind::Fresh);
    };
    // An unparsable prior (tampered marker) gives no policy signal: treat as nothing installed.
    let (Ok(p), Ok(n)) = (semver::Version::parse(prior), semver::Version::parse(new)) else {
        return Ok(InstallKind::Fresh);
    };
    match n.cmp(&p) {
        std::cmp::Ordering::Greater => Ok(InstallKind::Upgraded),
        std::cmp::Ordering::Equal => Ok(InstallKind::Reinstalled),
        std::cmp::Ordering::Less if allow_downgrade => Ok(InstallKind::Downgraded),
        std::cmp::Ordering::Less => Err(LocalPackageError::DowngradeRefused),
    }
}

/// Remove leftovers of crashed attempts under the Host parent: `.wip-*` and `.old-*` directories
/// and stray `.current.tmp-*` files directly inside name directories.
///
/// Walks `parent` → each real (non-symlink, non-dot) directory → matching real entries, deleting
/// FD-relative with `O_NOFOLLOW`. Version directories, `current`, other files, symlinks and
/// dot-entries at the parent level are left alone.
pub fn sweep_abandoned_staging(parent: &HostExtensionsParent) -> StagingSweepReport {
    let _guard = parent.lock.lock().unwrap_or_else(|e| e.into_inner());
    imp::sweep(&parent.dir)
}

#[cfg(unix)]
mod imp {
    use std::fs::File;

    use super::super::host_fs::{
        Kind, kind_nofollow, list_dir, read_file_under, remove_tree_at, rename_exchange, rename_noreplace, unlinkat,
        write_file_atomic,
    };
    use super::super::local_package_install::{LocalPackageError, ValidatedPackage, sys as base, write_file_under};
    use super::super::signed_manifest::SignatureCheckedManifest;
    use super::{
        AutoInstallOptions, AutoInstallReport, CurrentMarker, MARKER_FILE, MARKER_TMP_PREFIX, MAX_MARKER_BYTES,
        STAGING_PREFIX, SUPERSEDED_PREFIX, StagingSweepReport, UninstallOutcome, classify, encode_component, nonce,
    };
    use crate::signed_root_binding::SignedRootBindingStore;

    /// Where the new tree went, so a later failure can put things back.
    enum Placement {
        /// The version slot was free; the staging directory was renamed into it.
        New,
        /// A real directory was there; trees were exchanged and the old one lives at `old_name`.
        Swapped { old_name: String },
    }

    fn undo(name_dir: &File, version_component: &str, placement: &Placement) {
        match placement {
            Placement::New => {
                let _ = remove_tree_at(name_dir, version_component, 0);
            }
            Placement::Swapped { old_name } => {
                // Exchange back: old tree returns to the version slot, the new tree lands at
                // `old_name` and is removed. If the exchange back fails, nothing is deleted.
                if rename_exchange(name_dir, old_name, version_component).is_ok() {
                    let _ = remove_tree_at(name_dir, old_name, 0);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        parent: &File,
        name_component: &str,
        version_component: &str,
        marker: &[u8],
        options: AutoInstallOptions,
    ) -> Result<AutoInstallReport, LocalPackageError> {
        // Exclusive create tells us whether this call made the name directory (for tidy cleanup).
        let created_name = match base::mkdirat(parent, name_component, false) {
            Ok(()) => true,
            Err(e) if e.raw_os_error() == Some(libc::EEXIST) => false,
            Err(_) => return Err(LocalPackageError::RootCreateFailed),
        };
        let result = (|| {
            let name_dir = base::openat(parent, name_component, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|_| LocalPackageError::RootCreateFailed)?;
            stage_place_switch(store, checked, package, &name_dir, version_component, marker, options)
        })();
        if result.is_err() && created_name {
            // rmdir only succeeds on an empty directory: never removes anything that has content.
            let _ = unlinkat(parent, name_component, libc::AT_REMOVEDIR);
        }
        result
    }

    fn stage_place_switch(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        name_dir: &File,
        version_component: &str,
        marker: &[u8],
        options: AutoInstallOptions,
    ) -> Result<AutoInstallReport, LocalPackageError> {
        // --- policy: what is installed now? -------------------------------------------------
        let bound_version = store.lookup_checked(checked.name()).map(|c| c.version().to_owned());
        let old_marker = read_file_under(name_dir, MARKER_FILE, MAX_MARKER_BYTES)
            .ok()
            .and_then(|bytes| CurrentMarker::parse(&bytes).map(|m| (m, bytes)))
            .filter(|(m, _)| m.name == checked.name());
        let prior_version = bound_version
            .clone()
            .or_else(|| old_marker.as_ref().map(|(m, _)| m.version.clone()));
        let kind = classify(prior_version.as_deref(), checked.version(), options.allow_downgrade)?;
        // Previous version directories to delete once the switch commits (never the new slot).
        let mut superseded: Vec<String> = Vec::new();
        for candidate in [
            bound_version.as_deref().and_then(encode_component),
            old_marker.as_ref().map(|(m, _)| m.component.clone()),
        ]
        .into_iter()
        .flatten()
        {
            if candidate != version_component && !superseded.contains(&candidate) {
                superseded.push(candidate);
            }
        }

        // --- stage -------------------------------------------------------------------------
        let mut staged: Option<(String, File)> = None;
        for _ in 0..8 {
            let wip = format!(
                "{STAGING_PREFIX}{}",
                nonce().ok_or(LocalPackageError::RootCreateFailed)?
            );
            match base::mkdirat(name_dir, &wip, false) {
                Ok(()) => {
                    let root = match base::openat(name_dir, &wip, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
                        Ok(f) => f,
                        Err(_) => {
                            let _ = unlinkat(name_dir, &wip, libc::AT_REMOVEDIR);
                            return Err(LocalPackageError::RootCreateFailed);
                        }
                    };
                    staged = Some((wip, root));
                    break;
                }
                Err(e) if e.raw_os_error() == Some(libc::EEXIST) => continue,
                Err(_) => return Err(LocalPackageError::RootCreateFailed),
            }
        }
        let (wip, root) = staged.ok_or(LocalPackageError::RootCreateFailed)?;
        let drop_staging = |err: LocalPackageError| {
            let _ = remove_tree_at(name_dir, &wip, 0);
            err
        };
        for (rel, bytes) in &package.files {
            write_file_under(&root, rel, bytes).map_err(&drop_staging)?;
        }
        // The store requires a directory handle; check before anything becomes visible.
        if !root.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            return Err(drop_staging(LocalPackageError::RootCreateFailed));
        }

        // --- place (the previous version, if any, is still live and untouched in the store) --
        let placement = match kind_nofollow(name_dir, version_component) {
            Ok(None) => {
                rename_noreplace(name_dir, &wip, version_component)
                    .map_err(|_| drop_staging(LocalPackageError::PlaceFailed))?;
                Placement::New
            }
            Ok(Some(Kind::Dir)) => {
                rename_exchange(name_dir, &wip, version_component)
                    .map_err(|_| drop_staging(LocalPackageError::PlaceFailed))?;
                // The old tree now sits at `wip`; relabel it as superseded (best effort).
                let relabeled = nonce()
                    .map(|n| format!("{SUPERSEDED_PREFIX}{n}"))
                    .filter(|old| rename_noreplace(name_dir, &wip, old).is_ok());
                Placement::Swapped {
                    old_name: relabeled.unwrap_or_else(|| wip.clone()),
                }
            }
            Ok(Some(_)) => return Err(drop_staging(LocalPackageError::VersionSlotBlocked)),
            Err(_) => return Err(drop_staging(LocalPackageError::PlaceFailed)),
        };
        let _ = name_dir.sync_all();

        // --- switch: marker first (persistent truth), then the live binding ---------------------
        let marker_unchanged = old_marker.as_ref().is_some_and(|(_, bytes)| bytes == marker);
        if !marker_unchanged {
            let tmp = match nonce() {
                Some(n) => format!("{MARKER_TMP_PREFIX}{n}"),
                None => {
                    undo(name_dir, version_component, &placement);
                    return Err(LocalPackageError::MarkerWriteFailed);
                }
            };
            if write_file_atomic(name_dir, &tmp, MARKER_FILE, marker).is_err() {
                undo(name_dir, version_component, &placement);
                return Err(LocalPackageError::MarkerWriteFailed);
            }
        }
        let key = checked.name().to_owned();
        let files_written = package.files.len();
        // `root` is the staging inode, now named `version_component`. Replace the live binding.
        if let Err(e) = store.install(&key, checked, root) {
            // Only reachable if the root handle died after the pre-check; marker already names the
            // new version, so a restart would restore it. Report, do not delete anything further.
            return Err(LocalPackageError::Retain(e));
        }

        // --- cleanup superseded roots (best effort; sweep catches leftovers) -------------------
        if let Placement::Swapped { old_name } = &placement {
            let _ = remove_tree_at(name_dir, old_name, 0);
        }
        for previous in superseded {
            if !matches!(kind_nofollow(name_dir, &previous), Ok(Some(Kind::Dir))) {
                continue;
            }
            let Some(old) = nonce().map(|n| format!("{SUPERSEDED_PREFIX}{n}")) else {
                continue;
            };
            if rename_noreplace(name_dir, &previous, &old).is_ok() {
                let _ = remove_tree_at(name_dir, &old, 0);
            }
        }
        Ok(AutoInstallReport { files_written, kind })
    }

    pub(super) fn uninstall(
        store: &SignedRootBindingStore,
        parent: &File,
        name: &str,
        name_component: &str,
    ) -> Result<UninstallOutcome, LocalPackageError> {
        let kind = kind_nofollow(parent, name_component).map_err(|_| LocalPackageError::RemoveFailed)?;
        match kind {
            None => Ok(if store.remove(name) {
                UninstallOutcome::Removed
            } else {
                UninstallOutcome::NotInstalled
            }),
            Some(Kind::Dir) => {
                let name_dir = base::openat(parent, name_component, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                    .map_err(|_| LocalPackageError::RemoveFailed)?;
                // 1. No restart may re-bind this tree.
                if matches!(kind_nofollow(&name_dir, MARKER_FILE), Ok(Some(_))) {
                    let _ = remove_marker_entry(&name_dir);
                    if matches!(kind_nofollow(&name_dir, MARKER_FILE), Ok(Some(_))) {
                        return Err(LocalPackageError::RemoveFailed);
                    }
                }
                // 2. No more verified serving / prepare from the old root.
                store.remove(name);
                // 3. Delete the tree.
                drop(name_dir);
                remove_tree_at(parent, name_component, 0).map_err(|_| LocalPackageError::RemoveFailed)?;
                Ok(UninstallOutcome::Removed)
            }
            // A file / symlink / special entry where the name directory should be: refuse, touch nothing.
            Some(_) => Err(LocalPackageError::RemoveFailed),
        }
    }

    fn remove_marker_entry(name_dir: &File) -> std::io::Result<()> {
        match kind_nofollow(name_dir, MARKER_FILE)? {
            Some(Kind::Dir) => remove_tree_at(name_dir, MARKER_FILE, 0),
            Some(_) => unlinkat(name_dir, MARKER_FILE, 0),
            None => Ok(()),
        }
    }

    pub(super) fn sweep(parent: &File) -> StagingSweepReport {
        let mut report = StagingSweepReport::default();
        let Ok(names) = list_dir(parent) else {
            return report;
        };
        for name in names {
            // Name directories are never dot-prefixed (encode_component); leave Host entries alone.
            if name.starts_with('.') || !matches!(kind_nofollow(parent, &name), Ok(Some(Kind::Dir))) {
                continue;
            }
            let Ok(name_dir) = base::openat(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) else {
                continue;
            };
            let Ok(children) = list_dir(&name_dir) else {
                continue;
            };
            let mut removed_here = 0;
            for child in children {
                let is_dir_leftover = child.starts_with(STAGING_PREFIX) || child.starts_with(SUPERSEDED_PREFIX);
                let is_tmp_marker = child.starts_with(MARKER_TMP_PREFIX);
                let outcome = match kind_nofollow(&name_dir, &child) {
                    Ok(Some(Kind::Dir)) if is_dir_leftover => remove_tree_at(&name_dir, &child, 0),
                    Ok(Some(Kind::Regular)) if is_tmp_marker => unlinkat(&name_dir, &child, 0),
                    _ => continue,
                };
                match outcome {
                    Ok(()) => {
                        report.removed += 1;
                        removed_here += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            }
            if removed_here > 0 {
                // Only succeeds if now empty.
                let _ = unlinkat(parent, &name, libc::AT_REMOVEDIR);
            }
        }
        report
    }
}

#[cfg(windows)]
mod imp {
    //! Windows implementation (slice W2b). Same flow as the unix one, with handle-relative
    //! `NtCreateFile` / `NtSetInformationFile` operations ([`super::super::host_fs_windows`]).
    //!
    //! Differences that matter:
    //! * there is no atomic directory exchange, so a same-version replace is a **two-step rename**
    //!   (`{version}` -> `.old-{nonce}`, then `.wip-{nonce}` -> `{version}`) with an undo;
    //! * **crash window**: if the process dies between the two renames, `{version}` is missing
    //!   while `current` still names it; the next start leaves that extension unbound
    //!   (`VersionDirMissing`) and the startup sweep deletes the `.old-*` leftover, so the
    //!   extension must be installed again. A first install or a version change has no such window
    //!   (the new directory is placed with one no-replace rename before the marker switches);
    //! * directories cannot be fsynced through a handle on Windows; file contents are flushed.

    use std::fs::File;

    use super::super::host_fs_windows::{
        Kind, create_dir_exclusive, kind_nofollow, list_dir, open_dir_existing, open_host_dir_existing,
        remove_empty_dir_at, remove_tree_at, rename_noreplace, unlink_at, write_file_atomic,
    };
    use super::super::local_package_install::{LocalPackageError, ValidatedPackage, write_file_under};
    use super::super::signed_manifest::SignatureCheckedManifest;
    use super::super::verified_reader::read_file_under;
    use super::{
        AutoInstallOptions, AutoInstallReport, CurrentMarker, MARKER_FILE, MARKER_TMP_PREFIX, MAX_MARKER_BYTES,
        STAGING_PREFIX, SUPERSEDED_PREFIX, StagingSweepReport, UninstallOutcome, classify, encode_component, nonce,
    };
    use crate::signed_root_binding::SignedRootBindingStore;

    /// Whether another entry of `dir` differs from `component` only by ASCII case. A Windows
    /// directory lookup is case-insensitive, so such an entry would be silently reused (a second
    /// signed identity overwriting or deleting the first).
    fn case_alias(dir: &File, component: &str) -> std::io::Result<bool> {
        Ok(super::has_case_alias(&list_dir(dir)?, component))
    }

    /// Where the new tree went, so a later failure can put things back.
    enum Placement {
        /// The version slot was free; the staging directory was renamed into it.
        New,
        /// A real directory was there; it now lives at `old_name` and the new tree took its place.
        Swapped { old_name: String },
    }

    fn undo(name_dir: &File, version_component: &str, placement: &Placement) {
        match placement {
            Placement::New => {
                let _ = remove_tree_at(name_dir, version_component, 0);
            }
            Placement::Swapped { old_name } => {
                // Move the new tree aside, put the old tree back, then drop the new one. If the
                // second step fails the new tree is moved back so no slot is left empty.
                let Some(aside) = nonce().map(|n| format!("{SUPERSEDED_PREFIX}{n}")) else {
                    return;
                };
                if rename_noreplace(name_dir, version_component, &aside).is_err() {
                    return;
                }
                if rename_noreplace(name_dir, old_name, version_component).is_ok() {
                    let _ = remove_tree_at(name_dir, &aside, 0);
                } else {
                    let _ = rename_noreplace(name_dir, &aside, version_component);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        parent: &File,
        name_component: &str,
        version_component: &str,
        marker: &[u8],
        options: AutoInstallOptions,
    ) -> Result<AutoInstallReport, LocalPackageError> {
        match case_alias(parent, name_component) {
            Ok(false) => {}
            Ok(true) => return Err(LocalPackageError::CaseCollision),
            Err(_) => return Err(LocalPackageError::RootCreateFailed),
        }
        // Exclusive create tells us whether this call made the name directory (for tidy cleanup).
        let (name_dir, created_name) = match create_dir_exclusive(parent, name_component) {
            Ok(dir) => (dir, true),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (
                open_host_dir_existing(parent, name_component).map_err(|_| LocalPackageError::RootCreateFailed)?,
                false,
            ),
            Err(_) => return Err(LocalPackageError::RootCreateFailed),
        };
        let result = stage_place_switch(store, checked, package, &name_dir, version_component, marker, options);
        if result.is_err() && created_name {
            // Removal only succeeds on an empty directory: never removes anything that has content.
            let _ = remove_empty_dir_at(parent, name_component);
        }
        result
    }

    fn stage_place_switch(
        store: &SignedRootBindingStore,
        checked: SignatureCheckedManifest,
        package: ValidatedPackage,
        name_dir: &File,
        version_component: &str,
        marker: &[u8],
        options: AutoInstallOptions,
    ) -> Result<AutoInstallReport, LocalPackageError> {
        match case_alias(name_dir, version_component) {
            Ok(false) => {}
            Ok(true) => return Err(LocalPackageError::CaseCollision),
            Err(_) => return Err(LocalPackageError::RootCreateFailed),
        }
        // --- policy: what is installed now? -------------------------------------------------
        let bound_version = store.lookup_checked(checked.name()).map(|c| c.version().to_owned());
        let old_marker = read_file_under(name_dir, MARKER_FILE, MAX_MARKER_BYTES)
            .ok()
            .and_then(|bytes| CurrentMarker::parse(&bytes).map(|m| (m, bytes)))
            .filter(|(m, _)| m.name == checked.name());
        let prior_version = bound_version
            .clone()
            .or_else(|| old_marker.as_ref().map(|(m, _)| m.version.clone()));
        let kind = classify(prior_version.as_deref(), checked.version(), options.allow_downgrade)?;
        // Previous version directories to delete once the switch commits (never the new slot).
        let mut superseded: Vec<String> = Vec::new();
        for candidate in [
            bound_version.as_deref().and_then(encode_component),
            old_marker.as_ref().map(|(m, _)| m.component.clone()),
        ]
        .into_iter()
        .flatten()
        {
            if candidate != version_component && !superseded.contains(&candidate) {
                superseded.push(candidate);
            }
        }

        // --- stage -------------------------------------------------------------------------
        let mut staged: Option<(String, File)> = None;
        for _ in 0..8 {
            let wip = format!(
                "{STAGING_PREFIX}{}",
                nonce().ok_or(LocalPackageError::RootCreateFailed)?
            );
            match create_dir_exclusive(name_dir, &wip) {
                Ok(root) => {
                    staged = Some((wip, root));
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(_) => return Err(LocalPackageError::RootCreateFailed),
            }
        }
        let (wip, root) = staged.ok_or(LocalPackageError::RootCreateFailed)?;
        let drop_staging = |err: LocalPackageError| {
            let _ = remove_tree_at(name_dir, &wip, 0);
            err
        };
        for (rel, bytes) in &package.files {
            write_file_under(&root, rel, bytes).map_err(&drop_staging)?;
        }
        // The store requires a directory handle; check before anything becomes visible.
        if !root.metadata().map(|m| m.is_dir()).unwrap_or(false) {
            return Err(drop_staging(LocalPackageError::RootCreateFailed));
        }

        // --- place (the previous version, if any, is still live and untouched in the store) --
        let placement = match kind_nofollow(name_dir, version_component) {
            Ok(None) => {
                rename_noreplace(name_dir, &wip, version_component)
                    .map_err(|_| drop_staging(LocalPackageError::PlaceFailed))?;
                Placement::New
            }
            Ok(Some(Kind::Dir)) => {
                // Two-step replace: old tree aside, new tree in; undo the first step on failure.
                let old_name = nonce()
                    .map(|n| format!("{SUPERSEDED_PREFIX}{n}"))
                    .ok_or_else(|| drop_staging(LocalPackageError::PlaceFailed))?;
                rename_noreplace(name_dir, version_component, &old_name)
                    .map_err(|_| drop_staging(LocalPackageError::PlaceFailed))?;
                if rename_noreplace(name_dir, &wip, version_component).is_err() {
                    let _ = rename_noreplace(name_dir, &old_name, version_component);
                    return Err(drop_staging(LocalPackageError::PlaceFailed));
                }
                Placement::Swapped { old_name }
            }
            Ok(Some(_)) => return Err(drop_staging(LocalPackageError::VersionSlotBlocked)),
            Err(_) => return Err(drop_staging(LocalPackageError::PlaceFailed)),
        };

        // --- switch: marker first (persistent truth), then the live binding ---------------------
        let marker_unchanged = old_marker.as_ref().is_some_and(|(_, bytes)| bytes == marker);
        if !marker_unchanged {
            let tmp = match nonce() {
                Some(n) => format!("{MARKER_TMP_PREFIX}{n}"),
                None => {
                    undo(name_dir, version_component, &placement);
                    return Err(LocalPackageError::MarkerWriteFailed);
                }
            };
            if write_file_atomic(name_dir, &tmp, MARKER_FILE, marker).is_err() {
                undo(name_dir, version_component, &placement);
                return Err(LocalPackageError::MarkerWriteFailed);
            }
        }
        let key = checked.name().to_owned();
        let files_written = package.files.len();
        // `root` is the staging inode, now named `version_component`. Replace the live binding.
        if let Err(e) = store.install(&key, checked, root) {
            // Only reachable if the root handle died after the pre-check; marker already names the
            // new version, so a restart would restore it. Report, do not delete anything further.
            return Err(LocalPackageError::Retain(e));
        }

        // --- cleanup superseded roots (best effort; sweep catches leftovers) -------------------
        if let Placement::Swapped { old_name } = &placement {
            let _ = remove_tree_at(name_dir, old_name, 0);
        }
        for previous in superseded {
            if !matches!(kind_nofollow(name_dir, &previous), Ok(Some(Kind::Dir))) {
                continue;
            }
            let Some(old) = nonce().map(|n| format!("{SUPERSEDED_PREFIX}{n}")) else {
                continue;
            };
            if rename_noreplace(name_dir, &previous, &old).is_ok() {
                let _ = remove_tree_at(name_dir, &old, 0);
            }
        }
        Ok(AutoInstallReport { files_written, kind })
    }

    pub(super) fn uninstall(
        store: &SignedRootBindingStore,
        parent: &File,
        name: &str,
        name_component: &str,
    ) -> Result<UninstallOutcome, LocalPackageError> {
        if case_alias(parent, name_component).map_err(|_| LocalPackageError::RemoveFailed)? {
            return Err(LocalPackageError::CaseCollision);
        }
        let kind = kind_nofollow(parent, name_component).map_err(|_| LocalPackageError::RemoveFailed)?;
        match kind {
            None => Ok(if store.remove(name) {
                UninstallOutcome::Removed
            } else {
                UninstallOutcome::NotInstalled
            }),
            Some(Kind::Dir) => {
                let name_dir =
                    open_dir_existing(parent, name_component).map_err(|_| LocalPackageError::RemoveFailed)?;
                // 1. No restart may re-bind this tree.
                if matches!(kind_nofollow(&name_dir, MARKER_FILE), Ok(Some(_))) {
                    let _ = remove_marker_entry(&name_dir);
                    if matches!(kind_nofollow(&name_dir, MARKER_FILE), Ok(Some(_))) {
                        return Err(LocalPackageError::RemoveFailed);
                    }
                }
                // 2. No more verified serving / prepare from the old root.
                store.remove(name);
                // 3. Delete the tree.
                drop(name_dir);
                remove_tree_at(parent, name_component, 0).map_err(|_| LocalPackageError::RemoveFailed)?;
                Ok(UninstallOutcome::Removed)
            }
            // A file / link / special entry where the name directory should be: refuse, touch nothing.
            Some(_) => Err(LocalPackageError::RemoveFailed),
        }
    }

    fn remove_marker_entry(name_dir: &File) -> std::io::Result<()> {
        match kind_nofollow(name_dir, MARKER_FILE)? {
            Some(Kind::Dir) => remove_tree_at(name_dir, MARKER_FILE, 0),
            Some(_) => unlink_at(name_dir, MARKER_FILE),
            None => Ok(()),
        }
    }

    pub(super) fn sweep(parent: &File) -> StagingSweepReport {
        let mut report = StagingSweepReport::default();
        let Ok(names) = list_dir(parent) else {
            return report;
        };
        for name in names {
            // Name directories are never dot-prefixed (encode_component); leave Host entries alone.
            if name.starts_with('.') || !matches!(kind_nofollow(parent, &name), Ok(Some(Kind::Dir))) {
                continue;
            }
            let Ok(name_dir) = open_dir_existing(parent, &name) else {
                continue;
            };
            let Ok(children) = list_dir(&name_dir) else {
                continue;
            };
            let mut removed_here = 0;
            for child in children {
                let is_dir_leftover = child.starts_with(STAGING_PREFIX) || child.starts_with(SUPERSEDED_PREFIX);
                let is_tmp_marker = child.starts_with(MARKER_TMP_PREFIX);
                let outcome = match kind_nofollow(&name_dir, &child) {
                    Ok(Some(Kind::Dir)) if is_dir_leftover => remove_tree_at(&name_dir, &child, 0),
                    Ok(Some(Kind::Regular)) if is_tmp_marker => unlink_at(&name_dir, &child),
                    _ => continue,
                };
                match outcome {
                    Ok(()) => {
                        report.removed += 1;
                        removed_here += 1;
                    }
                    Err(_) => report.failed += 1,
                }
            }
            if removed_here > 0 {
                drop(name_dir);
                // Only succeeds if now empty.
                let _ = remove_empty_dir_at(parent, &name);
            }
        }
        report
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    use std::fs::File;

    use super::super::local_package_install::{LocalPackageError, ValidatedPackage};
    use super::super::signed_manifest::SignatureCheckedManifest;
    use super::{AutoInstallOptions, AutoInstallReport, StagingSweepReport, UninstallOutcome};
    use crate::signed_root_binding::SignedRootBindingStore;

    pub(super) fn uninstall(
        _store: &SignedRootBindingStore,
        _parent: &File,
        _name: &str,
        _name_component: &str,
    ) -> Result<UninstallOutcome, LocalPackageError> {
        Err(LocalPackageError::Unsupported)
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn install(
        _store: &SignedRootBindingStore,
        _checked: SignatureCheckedManifest,
        _package: ValidatedPackage,
        _parent: &File,
        _name_component: &str,
        _version_component: &str,
        _marker: &[u8],
        _options: AutoInstallOptions,
    ) -> Result<AutoInstallReport, LocalPackageError> {
        Err(LocalPackageError::Unsupported)
    }

    pub(super) fn sweep(_parent: &File) -> StagingSweepReport {
        StagingSweepReport::default()
    }
}

#[cfg(all(test, any(unix, windows)))]
mod tests {
    use super::imp;
    use super::*;
    use crate::hub::curated::{signature_payload, sri_sha512};
    use crate::hub::local_package_install::ValidatedPackage;
    use crate::hub::signed_manifest::{SignedManifestEntry, check_signed_manifest};
    use crate::hub::test_support::{identity, link_dir, link_file, open_host_dir, path_identity, remove_link};
    use ed25519_dalek::{Signer, SigningKey};
    use std::collections::{HashMap, HashSet};
    use std::io::{Cursor, Write};
    use std::path::{Path, PathBuf};
    use zip::write::SimpleFileOptions;

    const CSS: &[u8] = b"p{}";

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[29; 32])
    }

    fn policy() -> CuratedInstallPolicy {
        CuratedInstallPolicy::new(
            HashSet::new(),
            false,
            HashMap::from([("fixture".into(), key().verifying_key())]),
        )
    }

    struct Pkg {
        name: String,
        version: String,
        panel: Vec<u8>,
        raw: Vec<u8>,
        integrity: String,
        signature: String,
        zip: Vec<u8>,
    }

    /// `tag` makes the panel bytes differ between packages of the same version (replace tests).
    fn pkg_full(name: &str, version: &str, tag: &str, extra: &[(&str, &[u8])], tamper_css: bool) -> Pkg {
        let panel = format!("<p>{version}-{tag}</p>").into_bytes();
        let raw = format!(
            r#"{{"name":{name:?},"version":{version:?},"files":[{{"path":"panels/board.html","integrity":"{}"}},{{"path":"panels/board.css","integrity":"{}"}}]}}"#,
            sri_sha512(&panel),
            sri_sha512(CSS)
        )
        .into_bytes();
        let integrity = sri_sha512(&raw);
        let signature = hex::encode(
            key()
                .sign(signature_payload(name, version, &integrity).as_bytes())
                .to_bytes(),
        );
        let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let css: &[u8] = if tamper_css { b"evil{}" } else { CSS };
        let mut all: Vec<(&str, &[u8])> = vec![
            ("aion-extension.json", &raw),
            ("panels/board.html", &panel),
            ("panels/board.css", css),
        ];
        all.extend_from_slice(extra);
        for (n, b) in all {
            w.start_file(n, SimpleFileOptions::default()).unwrap();
            w.write_all(b).unwrap();
        }
        let zip = w.finish().unwrap().into_inner();
        Pkg {
            name: name.into(),
            version: version.into(),
            panel,
            raw,
            integrity,
            signature,
            zip,
        }
    }

    fn pkg_with(name: &str, version: &str, extra: &[(&str, &[u8])], tamper_css: bool) -> Pkg {
        pkg_full(name, version, "a", extra, tamper_css)
    }

    fn pkg(version: &str) -> Pkg {
        pkg_with("demo.plugin", version, &[], false)
    }

    fn source(p: &Pkg) -> LocalPackageSource<'_> {
        LocalPackageSource {
            entry: SignedManifestEntry {
                name: &p.name,
                version: &p.version,
                pubkey_id: Some("fixture"),
                integrity: Some(&p.integrity),
                signature: Some(&p.signature),
            },
            raw_manifest: &p.raw,
            package_zip: &p.zip,
        }
    }

    struct Env {
        tmp: tempfile::TempDir,
        parent_path: PathBuf,
        parent: HostExtensionsParent,
        store: SignedRootBindingStore,
    }

    fn env() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let parent_path = tmp.path().join("extensions");
        std::fs::create_dir(&parent_path).unwrap();
        let parent = HostExtensionsParent::new(open_host_dir(&parent_path)).unwrap();
        Env {
            tmp,
            parent_path,
            parent,
            store: SignedRootBindingStore::new(),
        }
    }

    fn install_opts(env: &Env, p: &Pkg, allow_downgrade: bool) -> Result<AutoInstallReport, LocalPackageError> {
        install_local_signed_package_auto(
            &env.store,
            &policy(),
            &source(p),
            &env.parent,
            LocalPackageLimits::default(),
            AutoInstallOptions { allow_downgrade },
        )
    }

    fn install(env: &Env, p: &Pkg) -> Result<AutoInstallReport, LocalPackageError> {
        install_opts(env, p, false)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        v.sort();
        v
    }

    fn assert_parent_empty(env: &Env) {
        assert!(names(&env.parent_path).is_empty(), "{:?}", names(&env.parent_path));
        assert!(env.store.is_empty());
    }

    fn name_dir(env: &Env) -> PathBuf {
        env.parent_path.join("demo.plugin")
    }

    fn bound_version(env: &Env) -> String {
        env.store.lookup_checked("demo.plugin").unwrap().version().to_owned()
    }

    fn marker_version(env: &Env) -> String {
        let bytes = std::fs::read(name_dir(env).join("current")).unwrap();
        CurrentMarker::parse(&bytes).unwrap().version
    }

    fn decode(component: &str) -> String {
        let b = component.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'~' {
                out.push(u8::from_str_radix(&component[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn component_encoding_is_safe_injective_and_readable() {
        assert_eq!(encode_component("demo.plugin").as_deref(), Some("demo.plugin"));
        assert_eq!(encode_component("1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(encode_component("1.0.0+build.5").as_deref(), Some("1.0.0~2Bbuild.5"));
        assert_eq!(encode_component(".").as_deref(), Some("~2E"));
        assert_eq!(encode_component("..").as_deref(), Some("~2E."));
        assert_eq!(encode_component(".wip-x").as_deref(), Some("~2Ewip-x"));
        assert_eq!(encode_component("a/b\\c").as_deref(), Some("a~2Fb~5Cc"));
        assert_eq!(encode_component("a~b").as_deref(), Some("a~7Eb"));
        assert_eq!(encode_component("é").as_deref(), Some("~C3~A9"));
        assert!(encode_component("").is_none());
        let samples = [
            "demo", ".", "..", "a/b", "a~2Fb", "~", "é", "x y", ".hidden", "a.b", "a\u{1}b",
        ];
        let mut seen = HashSet::new();
        for s in samples {
            let c = encode_component(s).unwrap();
            assert!(!c.starts_with('.') && c != "." && c != "..", "{c}");
            assert!(
                c.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-~".contains(&b)),
                "{c}"
            );
            assert_eq!(decode(&c), s);
            assert!(is_encoded_component(&c), "{c}");
            assert!(seen.insert(c), "collision for {s}");
        }
        assert!(encode_component(&"a".repeat(255)).is_some());
        assert!(encode_component(&"a".repeat(256)).is_none());
        assert!(encode_component(&format!("{}+", "a".repeat(253))).is_none());
        for bad in [
            "",
            ".x",
            "..",
            "a/b",
            "a~",
            "a~2",
            "a~zz",
            "a~2f",
            "a b",
            &"a".repeat(256),
        ] {
            assert!(!is_encoded_component(bad), "{bad:?}");
        }
    }

    #[test]
    fn parent_must_be_a_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("f");
        std::fs::write(&plain, b"x").unwrap();
        assert_eq!(
            HostExtensionsParent::new(File::open(&plain).unwrap()).unwrap_err(),
            LocalPackageError::ParentNotDirectory
        );
    }

    #[test]
    fn fresh_install_uses_derived_layout_writes_marker_and_leaves_no_staging() {
        let env = env();
        let p = pkg("1.0.0");
        let out = install(&env, &p).unwrap();
        assert_eq!(out.files_written, 3);
        assert_eq!(out.kind, InstallKind::Fresh);
        assert_eq!(names(&env.parent_path), vec!["demo.plugin"]);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        let root = name_dir(&env).join("1.0.0");
        assert_eq!(std::fs::read(root.join("panels/board.css")).unwrap(), CSS);
        assert_eq!(std::fs::read(root.join("aion-extension.json")).unwrap(), p.raw);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(&root).unwrap().mode() & 0o777, 0o700);
        }
        let marker_path = name_dir(&env).join("current");
        let meta = std::fs::symlink_metadata(&marker_path).unwrap();
        assert!(meta.file_type().is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(meta.mode() & 0o777, 0o600);
        }
        let m = CurrentMarker::parse(&std::fs::read(&marker_path).unwrap()).unwrap();
        assert_eq!(
            (m.name.as_str(), m.version.as_str(), m.component.as_str()),
            ("demo.plugin", "1.0.0", "1.0.0")
        );
        assert_eq!(m.signature, p.signature);
        let (snap, fd) = env.store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.version(), "1.0.0");
        let (a, b) = (identity(&fd), path_identity(&root));
        assert_eq!(a, b, "retained handle is the placed inode");
    }

    #[test]
    fn hostile_signed_identity_cannot_escape_the_parent() {
        let env = env();
        let p = pkg_with("../escape", "1.0.0+b/../../x", &[], false);
        assert!(install(&env, &p).is_err());
        assert_parent_empty(&env);

        let p = pkg_with("../escape", "1.0.0+b.1", &[], false);
        install(&env, &p).unwrap();
        assert_eq!(names(&env.parent_path), vec!["~2E.~2Fescape"]);
        assert_eq!(
            names(&env.parent_path.join("~2E.~2Fescape")),
            vec!["1.0.0~2Bb.1", "current"]
        );
        assert_eq!(names(env.tmp.path()), vec!["extensions"]);
        assert!(env.store.contains("../escape"));
    }

    #[test]
    fn failures_before_staging_create_nothing() {
        let env = env();
        let mut bad_sig = pkg("1.0.0");
        bad_sig.signature = "0".repeat(128);
        assert!(matches!(install(&env, &bad_sig), Err(LocalPackageError::Signature(_))));
        assert_parent_empty(&env);
        assert_eq!(
            install(&env, &pkg_with("demo.plugin", "1.0.0", &[], true)),
            Err(LocalPackageError::IntegrityMismatch)
        );
        assert_parent_empty(&env);
        assert_eq!(
            install(&env, &pkg_with("demo.plugin", "1.0.0", &[("x.css", b"x")], false)),
            Err(LocalPackageError::EntryNotCoveredBySignature)
        );
        assert_parent_empty(&env);
        let long = pkg_with(&"n".repeat(256), "1.0.0", &[], false);
        assert_eq!(install(&env, &long), Err(LocalPackageError::InvalidDirName));
        assert_parent_empty(&env);
    }

    #[test]
    fn upgrade_switches_after_ready_and_removes_the_superseded_version() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let (_, old_root) = env.store.lookup_clone("demo.plugin").unwrap();
        let out = install(&env, &pkg("2.0.0")).unwrap();
        assert_eq!(out.kind, InstallKind::Upgraded);
        assert_eq!(names(&name_dir(&env)), vec!["2.0.0", "current"]);
        assert_eq!(bound_version(&env), "2.0.0");
        assert_eq!(marker_version(&env), "2.0.0");
        let (_, new_root) = env.store.lookup_clone("demo.plugin").unwrap();
        let (a, b) = (identity(&new_root), path_identity(&name_dir(&env).join("2.0.0")));
        assert_eq!(a, b);
        // An in-flight holder of the old root still has a valid handle.
        assert!(old_root.metadata().unwrap().is_dir());
    }

    #[test]
    fn same_version_reinstall_replaces_atomically_and_keeps_marker() {
        let env = env();
        install(&env, &pkg_full("demo.plugin", "1.0.0", "old", &[], false)).unwrap();
        let marker_before = std::fs::read(name_dir(&env).join("current")).unwrap();
        let newer = pkg_full("demo.plugin", "1.0.0", "fixed", &[], false);
        let out = install(&env, &newer).unwrap();
        assert_eq!(out.kind, InstallKind::Reinstalled);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        assert_eq!(
            std::fs::read(name_dir(&env).join("1.0.0/panels/board.html")).unwrap(),
            newer.panel
        );
        let (_, root) = env.store.lookup_clone("demo.plugin").unwrap();
        let (a, b) = (identity(&root), path_identity(&name_dir(&env).join("1.0.0")));
        assert_eq!(a, b, "binding follows the new inode");
        // Signed bytes differ (different panel SRI), so the marker legitimately changed.
        assert_ne!(std::fs::read(name_dir(&env).join("current")).unwrap(), marker_before);
        // Identical package again: marker bytes are unchanged and nothing is left behind.
        let marker_now = std::fs::read(name_dir(&env).join("current")).unwrap();
        assert_eq!(install(&env, &newer).unwrap().kind, InstallKind::Reinstalled);
        assert_eq!(std::fs::read(name_dir(&env).join("current")).unwrap(), marker_now);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
    }

    #[test]
    fn downgrade_is_refused_unless_the_host_explicitly_allows_it() {
        let env = env();
        install(&env, &pkg("2.0.0")).unwrap();
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::DowngradeRefused));
        assert_eq!(names(&name_dir(&env)), vec!["2.0.0", "current"]);
        assert_eq!(
            (bound_version(&env), marker_version(&env)),
            ("2.0.0".into(), "2.0.0".into())
        );
        // Pre-release of the same triple is older too.
        assert_eq!(
            install(&env, &pkg("2.0.0-rc.1")),
            Err(LocalPackageError::DowngradeRefused)
        );
        let out = install_opts(&env, &pkg("1.0.0"), true).unwrap();
        assert_eq!(out.kind, InstallKind::Downgraded);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        assert_eq!(
            (bound_version(&env), marker_version(&env)),
            ("1.0.0".into(), "1.0.0".into())
        );
    }

    #[test]
    fn policy_uses_the_marker_when_nothing_is_bound_yet() {
        let env = env();
        install(&env, &pkg("2.0.0")).unwrap();
        env.store.clear(); // as after a restart, before restore
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::DowngradeRefused));
        let out = install(&env, &pkg("3.0.0")).unwrap();
        assert_eq!(out.kind, InstallKind::Upgraded);
        assert_eq!(
            names(&name_dir(&env)),
            vec!["3.0.0", "current"],
            "marker-named 2.0.0 superseded"
        );
    }

    #[test]
    fn bad_or_symlinked_marker_is_ignored_for_policy_and_rewritten_without_following() {
        let env = env();
        std::fs::create_dir(name_dir(&env)).unwrap();
        std::fs::write(name_dir(&env).join("current"), b"garbage").unwrap();
        assert_eq!(install(&env, &pkg("1.0.0")).unwrap().kind, InstallKind::Fresh);
        assert_eq!(marker_version(&env), "1.0.0");

        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("victim");
        std::fs::write(&target, b"keep").unwrap();
        std::fs::remove_file(name_dir(&env).join("current")).unwrap();
        let linked = link_file(&target, &name_dir(&env).join("current"));
        env.store.clear();
        assert_eq!(install(&env, &pkg("2.0.0")).unwrap().kind, InstallKind::Fresh);
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        let _ = linked;
        let meta = std::fs::symlink_metadata(name_dir(&env).join("current")).unwrap();
        assert!(meta.file_type().is_file(), "marker is a regular file again");
        assert_eq!(marker_version(&env), "2.0.0");
    }

    #[test]
    fn blocked_version_slot_is_refused_and_staging_is_removed() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(name_dir(&env)).unwrap();
        link_dir(outside.path(), &name_dir(&env).join("2.0.0"));
        assert_eq!(install(&env, &pkg("2.0.0")), Err(LocalPackageError::VersionSlotBlocked));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "2.0.0", "current"]);
        assert_eq!(
            (bound_version(&env), marker_version(&env)),
            ("1.0.0".into(), "1.0.0".into())
        );
    }

    #[test]
    fn symlink_named_like_the_name_dir_is_not_followed() {
        let env = env();
        let outside = tempfile::tempdir().unwrap();
        link_dir(outside.path(), &env.parent_path.join("demo.plugin"));
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::RootCreateFailed));
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        assert!(env.store.is_empty());
    }

    fn validated(files: Vec<(&str, &[u8])>) -> ValidatedPackage {
        ValidatedPackage {
            files: files.into_iter().map(|(p, b)| (p.to_owned(), b.to_vec())).collect(),
        }
    }

    fn checked_for(p: &Pkg) -> crate::hub::signed_manifest::SignatureCheckedManifest {
        check_signed_manifest(&policy(), &source(p).entry, &p.raw).unwrap()
    }

    #[test]
    fn write_failure_keeps_the_live_version_and_removes_only_this_attempts_staging() {
        let env = env();
        // Created name dir is removed again when nothing was installed.
        let p = pkg("1.0.0");
        let dir = open_host_dir(&env.parent_path);
        let bad = || validated(vec![("a", b"1"), ("a/b", b"2")]);
        let r = imp::install(
            &env.store,
            checked_for(&p),
            bad(),
            &dir,
            "demo.plugin",
            "1.0.0",
            b"{}",
            AutoInstallOptions::default(),
        );
        assert_eq!(r.unwrap_err(), LocalPackageError::WriteFailed);
        assert_parent_empty(&env);

        install(&env, &pkg("1.0.0")).unwrap();
        let p2 = pkg("2.0.0");
        let r = imp::install(
            &env.store,
            checked_for(&p2),
            bad(),
            &dir,
            "demo.plugin",
            "2.0.0",
            b"{}",
            AutoInstallOptions::default(),
        );
        assert_eq!(r.unwrap_err(), LocalPackageError::WriteFailed);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        assert_eq!(
            (bound_version(&env), marker_version(&env)),
            ("1.0.0".into(), "1.0.0".into())
        );
    }

    #[test]
    fn marker_switch_failure_undoes_a_new_placement() {
        let env = env();
        std::fs::create_dir_all(name_dir(&env).join("current")).unwrap(); // rename over a dir fails
        assert_eq!(install(&env, &pkg("1.0.0")), Err(LocalPackageError::MarkerWriteFailed));
        assert_eq!(names(&name_dir(&env)), vec!["current"]);
        assert!(env.store.is_empty());
    }

    #[test]
    fn marker_switch_failure_exchanges_a_replaced_version_back() {
        let env = env();
        let old = pkg_full("demo.plugin", "1.0.0", "old", &[], false);
        install(&env, &old).unwrap();
        // Make the marker unwritable (a directory now) so the replace must roll back.
        std::fs::remove_file(name_dir(&env).join("current")).unwrap();
        std::fs::create_dir(name_dir(&env).join("current")).unwrap();
        let newer = pkg_full("demo.plugin", "1.0.0", "new", &[], false);
        assert_eq!(install(&env, &newer), Err(LocalPackageError::MarkerWriteFailed));
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        assert_eq!(
            std::fs::read(name_dir(&env).join("1.0.0/panels/board.html")).unwrap(),
            old.panel,
            "old tree is back in the version slot"
        );
        assert_eq!(bound_version(&env), "1.0.0");
    }

    #[test]
    fn concurrent_installs_serialize_into_a_consistent_state() {
        let env = env();
        let (a, b) = (pkg("1.0.0"), pkg("2.0.0"));
        std::thread::scope(|s| {
            for p in [&a, &b] {
                let env = &env;
                s.spawn(move || {
                    for _ in 0..3 {
                        install_opts(env, p, true).unwrap();
                    }
                });
            }
        });
        let v = marker_version(&env);
        assert_eq!(bound_version(&env), v);
        assert_eq!(names(&name_dir(&env)), vec![v.clone(), "current".to_owned()]);
    }

    #[test]
    fn sweep_removes_only_leftovers_and_never_follows_symlinks() {
        let env = env();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("precious"), b"keep").unwrap();
        let root = &env.parent_path;
        let wip = root.join("a/.wip-deadbeef");
        std::fs::create_dir_all(wip.join("x/y")).unwrap();
        std::fs::write(wip.join("x/y/f"), b"f").unwrap();
        link_dir(outside.path(), &wip.join("x/link"));
        link_file(&outside.path().join("precious"), &wip.join("filelink"));
        std::fs::create_dir_all(root.join("a/.old-cafe/z")).unwrap();
        std::fs::write(root.join("a/.current.tmp-1"), b"t").unwrap();
        // Survivors.
        std::fs::create_dir_all(root.join("a/1.0.0")).unwrap();
        std::fs::write(root.join("a/1.0.0/keep"), b"k").unwrap();
        std::fs::write(root.join("a/current"), b"c").unwrap();
        std::fs::write(root.join("a/.wip-regular-file"), b"not a dir").unwrap();
        std::fs::create_dir_all(root.join(".hidden/.wip-x")).unwrap();
        std::fs::write(root.join("top-level-file"), b"t").unwrap();
        std::fs::create_dir_all(outside.path().join(".wip-z")).unwrap();
        link_dir(outside.path(), &root.join("linkdir"));
        std::fs::create_dir_all(root.join("c/.wip-1")).unwrap();

        let report = sweep_abandoned_staging(&env.parent);
        assert_eq!(report, StagingSweepReport { removed: 4, failed: 0 });
        assert!(!wip.exists() && !root.join("a/.old-cafe").exists() && !root.join("a/.current.tmp-1").exists());
        assert!(!root.join("c").exists());
        assert!(root.join("a/1.0.0/keep").exists() && root.join("a/current").exists());
        assert!(root.join("a/.wip-regular-file").exists());
        assert!(root.join(".hidden/.wip-x").exists() && root.join("top-level-file").exists());
        assert_eq!(std::fs::read(outside.path().join("precious")).unwrap(), b"keep");
        assert!(outside.path().join(".wip-z").exists(), "symlinked dir was not entered");
        assert!(
            root.join("linkdir")
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn install_then_sweep_leaves_the_installed_version_alone() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        assert_eq!(sweep_abandoned_staging(&env.parent), StagingSweepReport::default());
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        // The parent handle is long-lived: later sweeps must list it again from the start.
        std::fs::create_dir_all(name_dir(&env).join(".wip-late")).unwrap();
        assert_eq!(
            sweep_abandoned_staging(&env.parent),
            StagingSweepReport { removed: 1, failed: 0 }
        );
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
    }

    // ---- restart: restore_installed_extensions --------------------------------------------

    use super::super::host_package_restore::{RestoreRejection, restore_installed_extensions};

    fn restore(env: &Env) -> (SignedRootBindingStore, crate::hub::host_package_restore::RestoreReport) {
        let fresh = SignedRootBindingStore::new();
        let report = restore_installed_extensions(&fresh, &policy(), &env.parent, LocalPackageLimits::default());
        (fresh, report)
    }

    #[test]
    fn restart_restores_the_marker_named_version_with_the_opened_directory_handle() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        install(&env, &pkg("2.0.0")).unwrap();
        install(&env, &pkg_with("other.plugin", "0.5.0", &[], false)).unwrap();
        let (store, report) = restore(&env);
        assert_eq!(report.restored, 2);
        assert!(report.rejected.is_empty(), "{:?}", report.rejected);
        let (snap, root) = store.lookup_clone("demo.plugin").unwrap();
        assert_eq!(snap.version(), "2.0.0");
        let (a, b) = (identity(&root), path_identity(&name_dir(&env).join("2.0.0")));
        assert_eq!(a, b, "root is the handle opened under the parent");
        assert!(store.contains("other.plugin"));
        // Restore deletes nothing and a second restore never replaces a binding.
        assert_eq!(names(&name_dir(&env)), vec!["2.0.0", "current"]);
        let again = restore_installed_extensions(&store, &policy(), &env.parent, LocalPackageLimits::default());
        assert_eq!(again.restored, 0);
        assert!(
            again
                .rejected
                .iter()
                .all(|(_, why)| *why == RestoreRejection::AlreadyBound)
        );
    }

    #[test]
    fn restore_then_upgrade_continues_from_the_restored_binding() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let (store, report) = restore(&env);
        assert_eq!(report.restored, 1);
        let out = install_local_signed_package_auto(
            &store,
            &policy(),
            &source(&pkg("1.1.0")),
            &env.parent,
            LocalPackageLimits::default(),
            AutoInstallOptions::default(),
        )
        .unwrap();
        assert_eq!(out.kind, InstallKind::Upgraded);
        assert_eq!(names(&name_dir(&env)), vec!["1.1.0", "current"]);
    }

    fn rejected_with(env: &Env) -> Vec<RestoreRejection> {
        let (store, report) = restore(env);
        assert_eq!(report.restored, 0, "{:?}", report.rejected);
        assert!(store.is_empty());
        report.rejected.into_iter().map(|(_, why)| why).collect()
    }

    #[test]
    fn restore_fails_closed_on_marker_problems() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let marker = name_dir(&env).join("current");
        let good = std::fs::read(&marker).unwrap();

        std::fs::remove_file(&marker).unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::MarkerMissing]);
        for garbage in [
            b"garbage".to_vec(),
            vec![b'x'; 5000],
            br#"{"v":2}"#.to_vec(),
            String::from_utf8(good.clone())
                .unwrap()
                .replace("\"component\":\"1.0.0\"", "\"component\":\"../x\"")
                .into_bytes(),
            String::from_utf8(good.clone())
                .unwrap()
                .replace("}", ",\"extra\":1}")
                .into_bytes(),
        ] {
            std::fs::write(&marker, garbage).unwrap();
            assert_eq!(rejected_with(&env), vec![RestoreRejection::MarkerInvalid]);
        }
        // Marker as a symlink, directory or FIFO is never read through / blocked on.
        std::fs::remove_file(&marker).unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("m"), &good).unwrap();
        if link_file(&outside.path().join("m"), &marker) {
            assert_eq!(rejected_with(&env), vec![RestoreRejection::MarkerInvalid]);
            std::fs::remove_file(&marker).unwrap();
        }
        std::fs::create_dir(&marker).unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::MarkerInvalid]);
        std::fs::remove_dir(&marker).unwrap();
        #[cfg(unix)]
        {
            let c = std::ffi::CString::new(marker.to_str().unwrap()).unwrap();
            // SAFETY: valid NUL-terminated path in a temp dir.
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            assert_eq!(rejected_with(&env), vec![RestoreRejection::MarkerInvalid]);
        }
    }

    #[test]
    fn restore_never_guesses_a_version_or_follows_a_swapped_directory() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        // Another version directory exists, but the marker names 1.0.0: only 1.0.0 may be used.
        std::fs::create_dir(name_dir(&env).join("9.9.9")).unwrap();
        let root = name_dir(&env).join("1.0.0");

        // Drop the live binding first: Windows refuses to rename a directory while a handle to a
        // directory *inside* it is open (the test moves the name directory itself).
        env.store.clear();

        // Name directory renamed so the marker's signed name no longer encodes to it.
        let moved = env.parent_path.join("moved.plugin");
        std::fs::rename(name_dir(&env), &moved).unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::IdentityMismatch]);
        std::fs::rename(&moved, name_dir(&env)).unwrap();

        // Version directory replaced by a symlink to an identical copy: O_NOFOLLOW refuses it.
        let copy = env.tmp.path().join("copy");
        std::fs::rename(&root, &copy).unwrap();
        link_dir(&copy, &root);
        assert_eq!(rejected_with(&env), vec![RestoreRejection::VersionDirMissing]);
        remove_link(&root);
        assert_eq!(rejected_with(&env), vec![RestoreRejection::VersionDirMissing]);
        std::fs::rename(&copy, &root).unwrap();
        let (store, report) = restore(&env);
        assert_eq!((report.restored, store.len()), (1, 1));
    }

    #[test]
    fn restore_reverifies_signature_manifest_and_every_listed_file() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        let root = name_dir(&env).join("1.0.0");

        let css = root.join("panels/board.css");
        std::fs::write(&css, b"evil{}").unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::IntegrityMismatch]);
        std::fs::remove_file(&css).unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::CoveredFileMissing]);
        std::fs::write(&css, CSS).unwrap();

        let manifest = root.join("aion-extension.json");
        let raw = std::fs::read(&manifest).unwrap();
        std::fs::write(&manifest, [raw.as_slice(), b" "].concat()).unwrap();
        assert!(matches!(
            rejected_with(&env).as_slice(),
            [RestoreRejection::Signature(_)]
        ));
        std::fs::write(&manifest, &raw).unwrap();
        std::fs::remove_file(&manifest).unwrap();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::ManifestUnreadable]);
        std::fs::write(&manifest, &raw).unwrap();

        // Marker signature/key tampered, or a Host policy that no longer trusts the key.
        let marker_path = name_dir(&env).join("current");
        let good = std::fs::read_to_string(&marker_path).unwrap();
        let sig = pkg("1.0.0").signature;
        std::fs::write(&marker_path, good.replace(&sig, &"0".repeat(128))).unwrap();
        assert!(matches!(
            rejected_with(&env).as_slice(),
            [RestoreRejection::Signature(_)]
        ));
        std::fs::write(&marker_path, &good).unwrap();
        let empty_policy = CuratedInstallPolicy::new(HashSet::new(), false, HashMap::new());
        let fresh = SignedRootBindingStore::new();
        let report = restore_installed_extensions(&fresh, &empty_policy, &env.parent, LocalPackageLimits::default());
        assert!(matches!(
            report.rejected.as_slice(),
            [(_, RestoreRejection::Signature(_))]
        ));
        assert!(fresh.is_empty());
        let (store, report) = restore(&env);
        assert_eq!((report.restored, store.len()), (1, 1));
    }

    #[test]
    fn one_bad_extension_does_not_stop_the_others() {
        let env = env();
        install(&env, &pkg("1.0.0")).unwrap();
        install(&env, &pkg_with("other.plugin", "0.5.0", &[], false)).unwrap();
        std::fs::write(name_dir(&env).join("1.0.0/panels/board.css"), b"evil{}").unwrap();
        std::fs::create_dir(env.parent_path.join("not-an-extension")).unwrap();
        let (store, report) = restore(&env);
        assert_eq!(report.restored, 1);
        assert!(store.contains("other.plugin") && !store.contains("demo.plugin"));
        let mut why: Vec<_> = report.rejected.iter().map(|(n, w)| (n.as_str(), *w)).collect();
        why.sort_by_key(|(n, _)| *n);
        assert_eq!(
            why,
            vec![
                ("demo.plugin", RestoreRejection::IntegrityMismatch),
                ("not-an-extension", RestoreRejection::MarkerMissing)
            ]
        );
    }

    #[test]
    fn case_alias_is_ascii_case_folding_of_a_different_entry() {
        assert!(has_case_alias(&["Demo.Plugin", "other"], "demo.plugin"));
        assert!(
            !has_case_alias(&["demo.plugin", "other"], "demo.plugin"),
            "exact match is no alias"
        );
        assert!(!has_case_alias(&["demo.plugin2"], "demo.plugin"));
        // Encoded non-ASCII is upper-case hex only, so folding never merges distinct identities.
        assert!(!has_case_alias(&["~C3~89"], "~C3~A9"));
        assert!(!has_case_alias::<&str>(&[], "x"));
    }

    /// Windows lookups are case-insensitive: a second identity differing only by case would reuse
    /// (overwrite / delete) the first one's directory. Both install and uninstall must refuse.
    #[cfg(windows)]
    #[test]
    fn windows_refuses_identities_that_differ_only_by_case() {
        let env = env();
        let first = pkg_full("demo.plugin", "1.0.0", "a", &[], false);
        install(&env, &first).unwrap();
        let clash = pkg_full("Demo.Plugin", "1.0.0", "b", &[], false);
        assert_eq!(install(&env, &clash), Err(LocalPackageError::CaseCollision));
        assert_eq!(
            uninstall_local_signed_package(&env.store, &env.parent, "DEMO.PLUGIN"),
            Err(LocalPackageError::CaseCollision)
        );
        // Nothing of the original changed.
        assert_eq!(names(&env.parent_path), vec!["demo.plugin"]);
        assert_eq!(names(&name_dir(&env)), vec!["1.0.0", "current"]);
        assert_eq!(marker_version(&env), "1.0.0");
        assert_eq!(
            std::fs::read(name_dir(&env).join("1.0.0/panels/board.html")).unwrap(),
            first.panel
        );
        assert_eq!(bound_version(&env), "1.0.0");
        // Same for a version that differs only by case from an installed one.
        let rc = pkg_with("demo.plugin", "2.0.0-RC1", &[], false);
        install(&env, &rc).unwrap();
        let rc_lower = pkg_with("demo.plugin", "2.0.0-rc1", &[], false);
        assert_eq!(
            install_opts(&env, &rc_lower, true),
            Err(LocalPackageError::CaseCollision)
        );
        assert_eq!(marker_version(&env), "2.0.0-RC1");
        // The exact identity still works.
        assert!(uninstall_local_signed_package(&env.store, &env.parent, "demo.plugin").is_ok());
    }

    #[cfg(windows)]
    #[test]
    fn windows_refuses_identities_that_encode_to_device_names() {
        let env = env();
        for name in ["con", "NUL", "aux", "com1", "lpt9", "con.txt", "Prn.plugin"] {
            let p = pkg_with(name, "1.0.0", &[], false);
            assert_eq!(install(&env, &p), Err(LocalPackageError::InvalidDirName), "{name}");
            assert_eq!(
                uninstall_local_signed_package(&env.store, &env.parent, name),
                Err(LocalPackageError::InvalidDirName),
                "{name}"
            );
        }
        assert_parent_empty(&env);
        // The same identities are fine as *names* on unix (they are only special to Windows).
        assert!(host_component("demo.plugin").is_some());
        assert!(host_component("con").is_none() && host_component("x.").is_none());
    }

    /// The documented Windows crash window of a same-version replace: between the two renames the
    /// version directory is missing while `current` still names it. Restore must fail closed, the
    /// sweep must clear the leftover, and a plain reinstall must recover.
    #[cfg(windows)]
    #[test]
    fn crash_between_the_two_renames_fails_closed_and_a_reinstall_recovers() {
        let env = env();
        let p = pkg("1.0.0");
        install(&env, &p).unwrap();
        // What a crash right after step one (version -> .old-*) leaves behind.
        std::fs::rename(name_dir(&env).join("1.0.0"), name_dir(&env).join(".old-dead")).unwrap();
        env.store.clear();
        assert_eq!(rejected_with(&env), vec![RestoreRejection::VersionDirMissing]);
        assert_eq!(
            sweep_abandoned_staging(&env.parent),
            StagingSweepReport { removed: 1, failed: 0 }
        );
        assert_eq!(names(&name_dir(&env)), vec!["current"]);
        assert_eq!(install(&env, &p).unwrap().kind, InstallKind::Reinstalled);
        let (store, report) = restore(&env);
        assert_eq!((report.restored, store.len()), (1, 1));
    }

    #[test]
    fn uninstall_handles_absent_symlinked_and_present_name_directories() {
        use super::{UninstallOutcome, uninstall_local_signed_package};
        let tmp = tempfile::tempdir().unwrap();
        let parent = HostExtensionsParent::new(open_host_dir(tmp.path())).unwrap();
        let store = SignedRootBindingStore::new();
        assert_eq!(
            uninstall_local_signed_package(&store, &parent, "demo.plugin"),
            Ok(UninstallOutcome::NotInstalled)
        );
        // A symlink where the name directory should be is refused and its target untouched.
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("keep"), b"k").unwrap();
        link_dir(elsewhere.path(), &tmp.path().join("demo.plugin"));
        assert_eq!(
            uninstall_local_signed_package(&store, &parent, "demo.plugin"),
            Err(LocalPackageError::RemoveFailed)
        );
        assert!(elsewhere.path().join("keep").exists());
        remove_link(&tmp.path().join("demo.plugin"));
        // Real directory with marker and a nested tree is removed.
        std::fs::create_dir_all(tmp.path().join("demo.plugin/1.0.0/sub")).unwrap();
        std::fs::write(tmp.path().join("demo.plugin/1.0.0/sub/f"), b"f").unwrap();
        std::fs::write(tmp.path().join("demo.plugin/current"), b"{}").unwrap();
        assert_eq!(
            uninstall_local_signed_package(&store, &parent, "demo.plugin"),
            Ok(UninstallOutcome::Removed)
        );
        assert!(!tmp.path().join("demo.plugin").exists());
        // Names that cannot be a component are refused before touching anything.
        assert_eq!(
            uninstall_local_signed_package(&store, &parent, ""),
            Err(LocalPackageError::InvalidDirName)
        );
    }
}
