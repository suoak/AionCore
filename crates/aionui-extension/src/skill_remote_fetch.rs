//! E10-2: fetch skills from remote sources (URL / Git) safely.
//!
//! Rules (design E10 §2.2 + M8 S5 instruction):
//! - https only; `file://`, `ssh://`, `git://`, scp-style and local paths are refused.
//! - The host must resolve only to public addresses: loopback, private, link-local,
//!   CGNAT, unspecified, multicast, documentation/benchmark ranges and IPv6 ULA /
//!   link-local / mapped-private addresses are refused, as are `localhost` names.
//! - URL downloads pin the vetted addresses (no DNS rebinding), never follow
//!   redirects and stop at [`MAX_FETCH_BYTES`].
//! - Git uses the system `git` CLI (D10-5): shallow, single branch, no tags, no
//!   submodules, no hooks (`core.hooksPath=/dev/null`), no symlinks, no
//!   global/system config, no credential prompt, https-only protocol allowlist,
//!   no redirects, a timeout, and a size check on the result.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use url::Url;

/// Upper bound for a downloaded archive / SKILL.md and for a cloned tree (20 MB, design §2.2).
pub const MAX_FETCH_BYTES: u64 = 20 * 1024 * 1024;
/// Upper bound for one fetch (network + clone).
pub const FETCH_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("source_url_invalid: {0}")]
    InvalidUrl(String),
    #[error("source_address_blocked: {0}")]
    BlockedAddress(String),
    #[error("source_too_large: limit {limit} bytes")]
    TooLarge { limit: u64 },
    #[error("source_fetch_failed: {0}")]
    Failed(String),
    #[error("source_fetch_timeout")]
    Timeout,
    /// The local git cannot pin resolved addresses (`http.curloptResolve`
    /// needs git >= 2.37), so git import is refused rather than run unpinned.
    #[error("source_git_unsupported: {0}")]
    GitUnsupported(String),
}

/// Minimum git that understands `http.curloptResolve` (added in 2.37.0).
pub const MIN_GIT_FOR_PINNING: (u32, u32) = (2, 37);

/// Parse `git version X.Y[.Z...]` output into (major, minor).
pub fn parse_git_version(out: &str) -> Option<(u32, u32)> {
    let v = out.trim().strip_prefix("git version ")?;
    let mut it = v.split(|c: char| !c.is_ascii_digit());
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    Some((major, minor))
}

async fn ensure_git_supports_pinning() -> Result<(), FetchError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("--version")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), cmd.output())
        .await
        .map_err(|_| FetchError::GitUnsupported("`git --version` timed out".into()))?
        .map_err(|e| FetchError::GitUnsupported(format!("git CLI not available: {e}")))?;
    let text = String::from_utf8_lossy(&out.stdout);
    match parse_git_version(&text) {
        Some(v) if v >= MIN_GIT_FOR_PINNING => Ok(()),
        Some((a, b)) => Err(FetchError::GitUnsupported(format!(
            "git {a}.{b} cannot pin DNS (http.curloptResolve needs >= {}.{}); upgrade git to import from git",
            MIN_GIT_FOR_PINNING.0, MIN_GIT_FOR_PINNING.1
        ))),
        None => Err(FetchError::GitUnsupported(format!(
            "unrecognised `git --version` output: {}",
            text.trim()
        ))),
    }
}

/// `http.curloptResolve` value pinning `host:port` to the vetted addresses
/// (curl CURLOPT_RESOLVE syntax; IPv6 in brackets). `None` for IP-literal hosts,
/// which need no resolution.
pub fn curlopt_resolve_entry(url: &Url, addrs: &[SocketAddr]) -> Option<String> {
    let host = match url.host()? {
        url::Host::Domain(d) => d.to_string(),
        url::Host::Ipv4(_) | url::Host::Ipv6(_) => return None,
    };
    let port = url.port_or_known_default().unwrap_or(443);
    let mut ips: Vec<String> = Vec::new();
    for a in addrs {
        let ip = match a.ip() {
            IpAddr::V4(v4) => v4.to_string(),
            IpAddr::V6(v6) => format!("[{v6}]"),
        };
        if !ips.contains(&ip) {
            ips.push(ip);
        }
    }
    Some(format!("{host}:{port}:{}", ips.join(",")))
}

/// Validate a remote locator: https, has a host, no credentials, not a blocked name.
pub fn parse_remote_url(raw: &str) -> Result<Url, FetchError> {
    let url = Url::parse(raw.trim()).map_err(|e| FetchError::InvalidUrl(e.to_string()))?;
    if url.scheme() != "https" {
        return Err(FetchError::InvalidUrl(format!(
            "only https is allowed, got '{}'",
            url.scheme()
        )));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(FetchError::InvalidUrl("credentials in URL are not allowed".into()));
    }
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".into()))?
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") || host.ends_with(".internal") {
        return Err(FetchError::BlockedAddress(host));
    }
    match url.host() {
        Some(url::Host::Ipv4(ip)) if is_blocked_ip(IpAddr::V4(ip)) => return Err(FetchError::BlockedAddress(host)),
        Some(url::Host::Ipv6(ip)) if is_blocked_ip(IpAddr::V6(ip)) => return Err(FetchError::BlockedAddress(host)),
        _ => {}
    }
    Ok(url)
}

/// Whether an address is internal / non-public and therefore refused.
pub fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return is_blocked_v4(v4);
            }
            is_blocked_v6(v6)
        }
    }
}

fn is_blocked_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_private()
        || ip.is_link_local()
        || ip.is_broadcast()
        || ip.is_multicast()
        || ip.is_documentation()
        || o[0] == 0
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10 CGNAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24
        || (o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
        || o[0] >= 240 // reserved
}

fn is_blocked_v6(ip: Ipv6Addr) -> bool {
    let seg = ip.segments();
    ip.is_unspecified()
        || ip.is_loopback()
        || ip.is_multicast()
        || (seg[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
        || (seg[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        || (seg[0] & 0xffc0) == 0xfec0 // fec0::/10 site-local
        || (seg[0] == 0x2001 && seg[1] == 0x0db8) // documentation
        || (seg[0] == 0x0064 && seg[1] == 0xff9b) // NAT64 can reach v4 internals
        || ip.to_ipv4().is_some_and(|v4| seg[0..6].iter().all(|s| *s == 0) && is_blocked_v4(v4))
}

/// Resolve the URL host and require every address to be public.
pub async fn resolve_public(url: &Url) -> Result<Vec<SocketAddr>, FetchError> {
    let host = url
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("missing host".into()))?;
    let port = url.port_or_known_default().unwrap_or(443);
    let host_for_lookup = host.trim_start_matches('[').trim_end_matches(']').to_string();
    let addrs: Vec<SocketAddr> = tokio::net::lookup_host((host_for_lookup.as_str(), port))
        .await
        .map_err(|e| FetchError::Failed(format!("dns: {e}")))?
        .collect();
    if addrs.is_empty() {
        return Err(FetchError::Failed("dns: no addresses".into()));
    }
    if let Some(bad) = addrs.iter().find(|a| is_blocked_ip(a.ip())) {
        return Err(FetchError::BlockedAddress(format!("{host} -> {}", bad.ip())));
    }
    Ok(addrs)
}

/// Download `url` (https, public host, pinned addresses, no redirects) into memory,
/// refusing anything over [`MAX_FETCH_BYTES`].
pub async fn download_https(url: &Url) -> Result<Vec<u8>, FetchError> {
    let addrs = resolve_public(url).await?;
    let host = url.host_str().unwrap_or_default().to_string();
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .resolve_to_addrs(&host, &addrs)
        .timeout(FETCH_TIMEOUT)
        .build()
        .map_err(|e| FetchError::Failed(e.to_string()))?;
    let mut resp = client.get(url.clone()).send().await.map_err(|e| {
        if e.is_timeout() {
            FetchError::Timeout
        } else {
            FetchError::Failed(e.to_string())
        }
    })?;
    if !resp.status().is_success() {
        return Err(FetchError::Failed(format!("http status {}", resp.status())));
    }
    if resp.content_length().is_some_and(|n| n > MAX_FETCH_BYTES) {
        return Err(FetchError::TooLarge { limit: MAX_FETCH_BYTES });
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(|e| {
        if e.is_timeout() {
            FetchError::Timeout
        } else {
            FetchError::Failed(e.to_string())
        }
    })? {
        if body.len() as u64 + chunk.len() as u64 > MAX_FETCH_BYTES {
            return Err(FetchError::TooLarge { limit: MAX_FETCH_BYTES });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Validate a git ref name: no option injection, no `..`, conservative charset.
pub fn validate_git_ref(r: &str) -> Result<(), FetchError> {
    let ok = !r.is_empty()
        && r.len() <= 200
        && !r.starts_with('-')
        && !r.contains("..")
        && r.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c));
    if ok {
        Ok(())
    } else {
        Err(FetchError::InvalidUrl(format!("invalid ref '{r}'")))
    }
}

/// Validate a subpath inside the fetched tree: relative, no `..`, no root.
pub fn validate_subpath(sub: &str) -> Result<PathBuf, FetchError> {
    let p = Path::new(sub);
    if p.components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
    {
        Ok(p.to_path_buf())
    } else {
        Err(FetchError::InvalidUrl(format!("invalid subpath '{sub}'")))
    }
}

/// Hardened `git` arguments shared by clone / rev-parse.
fn hardened_git(cmd: &mut tokio::process::Command) {
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ASKPASS", "/bin/true")
        .env("GIT_PROTOCOL_FROM_USER", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        // A proxy would resolve the host itself and defeat address pinning.
        .env_remove("http_proxy")
        .env_remove("HTTP_PROXY")
        .env_remove("https_proxy")
        .env_remove("HTTPS_PROXY")
        .env_remove("all_proxy")
        .env_remove("ALL_PROXY")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.symlinks=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "submodule.recurse=false",
            "-c",
            "credential.helper=",
            "-c",
            "http.proxy=",
        ])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
}

/// Total size of regular files under `dir`, ignoring `.git`.
pub fn tree_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d)? {
            let entry = entry?;
            let ft = entry.file_type()?;
            if ft.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(entry.path());
                }
            } else if ft.is_file() {
                total += entry.metadata()?.len();
            }
        }
    }
    Ok(total)
}

/// Result of a shallow clone.
#[derive(Debug, Clone)]
pub struct ClonedRepo {
    pub dir: PathBuf,
    pub commit: Option<String>,
}

/// Shallow-clone `url` at `git_ref` into `dest` (must not exist) with the system git CLI.
pub async fn git_clone(url: &Url, git_ref: Option<&str>, dest: &Path) -> Result<ClonedRepo, FetchError> {
    if let Some(r) = git_ref {
        validate_git_ref(r)?;
    }
    ensure_git_supports_pinning().await?;
    // Resolve + vet ourselves, then pin git/curl to exactly those addresses so a
    // second (attacker-controlled) DNS answer cannot redirect it (DNS rebinding).
    let addrs = resolve_public(url).await?;
    let mut cmd = tokio::process::Command::new("git");
    hardened_git(&mut cmd);
    if let Some(entry) = curlopt_resolve_entry(url, &addrs) {
        cmd.arg("-c").arg(format!("http.curloptResolve={entry}"));
    }
    cmd.args([
        "clone",
        "--depth",
        "1",
        "--single-branch",
        "--no-tags",
        "--no-recurse-submodules",
    ]);
    if let Some(r) = git_ref {
        cmd.arg("--branch").arg(r);
    }
    cmd.arg("--").arg(url.as_str()).arg(dest);
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let out = tokio::time::timeout(FETCH_TIMEOUT, cmd.output())
        .await
        .map_err(|_| FetchError::Timeout)?
        .map_err(|e| FetchError::Failed(format!("git: {e}")))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err
            .lines()
            .last()
            .unwrap_or("git clone failed")
            .chars()
            .take(300)
            .collect::<String>();
        return Err(FetchError::Failed(first));
    }
    let size = tree_size(dest).map_err(|e| FetchError::Failed(e.to_string()))?;
    if size > MAX_FETCH_BYTES {
        return Err(FetchError::TooLarge { limit: MAX_FETCH_BYTES });
    }
    let mut rev = tokio::process::Command::new("git");
    hardened_git(&mut rev);
    rev.arg("-C").arg(dest).args(["rev-parse", "HEAD"]);
    let commit = match tokio::time::timeout(Duration::from_secs(10), rev.output()).await {
        Ok(Ok(o)) if o.status.success() => Some(String::from_utf8_lossy(&o.stdout).trim().to_string()),
        _ => None,
    };
    Ok(ClonedRepo {
        dir: dest.to_path_buf(),
        commit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_non_https_and_local_schemes() {
        for raw in [
            "file:///etc/passwd",
            "http://example.com/x",
            "ssh://git@example.com/r.git",
            "git://example.com/r.git",
            "/tmp/local",
            "git@github.com:o/r.git",
            "https://user:pw@example.com/r.git",
        ] {
            assert!(parse_remote_url(raw).is_err(), "{raw}");
        }
        assert!(parse_remote_url("https://github.com/o/r.git").is_ok());
    }

    #[test]
    fn refuses_internal_hosts() {
        for raw in [
            "https://localhost/x",
            "https://foo.localhost/x",
            "https://127.0.0.1/x",
            "https://10.1.2.3/x",
            "https://172.16.0.1/x",
            "https://192.168.1.1/x",
            "https://169.254.169.254/latest",
            "https://100.64.0.1/x",
            "https://0.0.0.0/x",
            "https://[::1]/x",
            "https://[fd00::1]/x",
            "https://[fe80::1]/x",
            "https://[::ffff:10.0.0.1]/x",
        ] {
            assert!(
                matches!(parse_remote_url(raw), Err(FetchError::BlockedAddress(_))),
                "{raw}"
            );
        }
        assert!(!is_blocked_ip("140.82.112.3".parse().unwrap()));
        assert!(!is_blocked_ip("2606:4700::1111".parse().unwrap()));
    }

    #[test]
    fn git_version_gate_and_resolve_entry() {
        assert_eq!(parse_git_version("git version 2.47.3\n"), Some((2, 47)));
        assert_eq!(parse_git_version("git version 2.39.5 (Apple Git-154)"), Some((2, 39)));
        assert_eq!(parse_git_version("git version 2.45.1.windows.1"), Some((2, 45)));
        assert!(parse_git_version("git version 2.36.0").unwrap() < MIN_GIT_FOR_PINNING);
        assert_eq!(parse_git_version("nope"), None);

        let url = Url::parse("https://example.com/a/b.git").unwrap();
        let addrs: Vec<SocketAddr> = vec![
            "93.184.216.34:443".parse().unwrap(),
            "[2606:2800:220:1::1]:443".parse().unwrap(),
            "93.184.216.34:443".parse().unwrap(),
        ];
        assert_eq!(
            curlopt_resolve_entry(&url, &addrs).as_deref(),
            Some("example.com:443:93.184.216.34,[2606:2800:220:1::1]")
        );
        let lit = Url::parse("https://93.184.216.34/x.git").unwrap();
        assert_eq!(curlopt_resolve_entry(&lit, &addrs), None);
    }

    #[test]
    fn validates_refs_and_subpaths() {
        assert!(validate_git_ref("main").is_ok());
        assert!(validate_git_ref("v1.2/x").is_ok());
        assert!(validate_git_ref("--upload-pack=evil").is_err());
        assert!(validate_git_ref("a..b").is_err());
        assert!(validate_subpath("skills/foo").is_ok());
        assert!(validate_subpath("../x").is_err());
        assert!(validate_subpath("/etc").is_err());
    }

    #[tokio::test]
    async fn resolve_rejects_loopback_name() {
        let url = Url::parse("https://127.0.0.1:8443/x").unwrap();
        assert!(matches!(resolve_public(&url).await, Err(FetchError::BlockedAddress(_))));
    }
}
