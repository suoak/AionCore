//! M8 E6-7: [`GithubIssueSource`] over the user's own `gh` CLI login.
//!
//! The import only runs `gh issue list` (read-only). The one write, 3.0 C8's
//! [`GhCliIssueComments`], runs `gh issue comment` and only after the user
//! confirmed the exact text (see `board::issue_flow`). The runner scrubs the
//! environment of WorkMate secrets and disables prompts; no token is read or
//! stored here. Errors are redacted before they reach the caller.

use std::path::PathBuf;
use std::time::Duration;

use aionui_conversation::board::github_import::{GithubIssue, GithubIssueSource, GithubIssueSourceError};
use aionui_review::delivery_cli::{CliRunError, HostCli, redact_output};
use serde::Deserialize;

const GH_TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct GhCliIssueSource {
    cli: HostCli,
    cwd: PathBuf,
}

impl GhCliIssueSource {
    pub(crate) fn new() -> Self {
        Self::with_cli(HostCli::new("gh"))
    }

    pub(crate) fn with_cli(cli: HostCli) -> Self {
        Self {
            cli,
            cwd: std::env::temp_dir(),
        }
    }
}

/// `gh issue list -R owner/name --state open --limit N [--search Q] --json …`.
pub(crate) fn issue_list_argv(repo: &str, query: Option<&str>, limit: u32) -> Vec<String> {
    let mut argv: Vec<String> = vec![
        "issue".into(),
        "list".into(),
        "--repo".into(),
        repo.to_owned(),
        "--state".into(),
        "open".into(),
        "--limit".into(),
        limit.to_string(),
        "--json".into(),
        "number,title,body,labels,url".into(),
    ];
    if let Some(query) = query {
        // `--search=Q` keeps a query starting with `-` from being read as a flag.
        argv.push(format!("--search={query}"));
    }
    argv
}

#[derive(Deserialize)]
struct GhLabel {
    name: String,
}

#[derive(Deserialize)]
struct GhIssue {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    labels: Vec<GhLabel>,
    #[serde(default)]
    url: String,
}

pub(crate) fn parse_issue_list(stdout: &str) -> Result<Vec<GithubIssue>, GithubIssueSourceError> {
    let issues: Vec<GhIssue> = serde_json::from_str(stdout.trim())
        .map_err(|error| GithubIssueSourceError::Failed(format!("unexpected gh issue list output: {error}")))?;
    Ok(issues
        .into_iter()
        .map(|issue| GithubIssue {
            number: issue.number,
            title: issue.title,
            body: issue.body.unwrap_or_default(),
            labels: issue.labels.into_iter().map(|label| label.name).collect(),
            url: issue.url,
        })
        .collect())
}

pub(crate) fn classify_failure(repo: &str, output: &str) -> GithubIssueSourceError {
    let lower = output.to_ascii_lowercase();
    if lower.contains("gh auth login") || lower.contains("not logged in") || lower.contains("authentication required") {
        GithubIssueSourceError::NotAuthenticated
    } else if lower.contains("could not resolve to a repository") || lower.contains("http 404") {
        GithubIssueSourceError::RepoNotFound(repo.to_owned())
    } else {
        let message = redact_output(output);
        GithubIssueSourceError::Failed(if message.is_empty() {
            "gh issue list failed".to_owned()
        } else {
            message
        })
    }
}

#[async_trait::async_trait]
impl GithubIssueSource for GhCliIssueSource {
    async fn list_open_issues(
        &self,
        repo: &str,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<GithubIssue>, GithubIssueSourceError> {
        let argv = issue_list_argv(repo, query, limit);
        let output = self
            .cli
            .run(&argv, &self.cwd, None, GH_TIMEOUT)
            .await
            .map_err(|error| match error {
                CliRunError::NotInstalled(_) => GithubIssueSourceError::CliMissing,
                CliRunError::Timeout(_) => GithubIssueSourceError::Timeout,
                CliRunError::Io { message, .. } => GithubIssueSourceError::Failed(redact_output(&message)),
            })?;
        if !output.success() {
            return Err(classify_failure(repo, &output.combined()));
        }
        parse_issue_list(&output.stdout)
    }
}

/// 3.0 C8: posts a confirmed comment (`gh issue comment N --repo R --body-file -`).
pub(crate) struct GhCliIssueComments {
    cli: HostCli,
    cwd: PathBuf,
}

impl GhCliIssueComments {
    pub(crate) fn new() -> Self {
        Self::with_cli(HostCli::new("gh"))
    }

    pub(crate) fn with_cli(cli: HostCli) -> Self {
        Self {
            cli,
            cwd: std::env::temp_dir(),
        }
    }
}

/// The body goes through stdin, never argv.
pub(crate) fn issue_comment_argv(repo: &str, number: u64) -> Vec<String> {
    vec![
        "issue".into(),
        "comment".into(),
        number.to_string(),
        "--repo".into(),
        repo.to_owned(),
        "--body-file".into(),
        "-".into(),
    ]
}

fn comment_error(repo: &str, error: GithubIssueSourceError) -> aionui_conversation::ConversationError {
    use aionui_conversation::ConversationError;
    match error {
        GithubIssueSourceError::Failed(message) => ConversationError::BadGateway {
            reason: format!("github_comment_failed: {message}"),
        },
        GithubIssueSourceError::Timeout => ConversationError::Timeout {
            reason: "github_comment_timeout: gh issue comment timed out".into(),
        },
        GithubIssueSourceError::RepoNotFound(_) => ConversationError::NotFoundReason {
            reason: format!("github_repo_not_found: {repo}"),
        },
        other => other.into(),
    }
}

#[async_trait::async_trait]
impl aionui_conversation::board::issue_flow::IssueCommentPort for GhCliIssueComments {
    async fn post_issue_comment(
        &self,
        _user_id: &str,
        repo: &str,
        number: u64,
        body: &str,
    ) -> Result<Option<String>, aionui_conversation::ConversationError> {
        let argv = issue_comment_argv(repo, number);
        let output = self
            .cli
            .run(&argv, &self.cwd, Some(body.as_bytes().to_vec()), GH_TIMEOUT)
            .await
            .map_err(|error| {
                comment_error(
                    repo,
                    match error {
                        CliRunError::NotInstalled(_) => GithubIssueSourceError::CliMissing,
                        CliRunError::Timeout(_) => GithubIssueSourceError::Timeout,
                        CliRunError::Io { message, .. } => GithubIssueSourceError::Failed(redact_output(&message)),
                    },
                )
            })?;
        if !output.success() {
            return Err(comment_error(repo, classify_failure(repo, &output.combined())));
        }
        Ok(output
            .stdout
            .lines()
            .map(str::trim)
            .find(|line| line.starts_with("https://"))
            .map(str::to_owned))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn issue_comments_send_the_body_on_stdin() {
        use aionui_conversation::board::issue_flow::IssueCommentPort;
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let env = vec![("PATH".into(), path)];
        let fake = dir.path().join("gh");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/argv\"\ncat > \"$(dirname \"$0\")/stdin\"\necho https://github.com/a/b/issues/7#issuecomment-1\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let comments = GhCliIssueComments::with_cli(HostCli::new(&fake).with_base_env(env.clone()));
        let body = "Fixed in #8; $(rm -rf /) `x` --repo other/repo";
        let url = comments.post_issue_comment("u", "a/b", 7, body).await.unwrap();
        assert_eq!(url.as_deref(), Some("https://github.com/a/b/issues/7#issuecomment-1"));
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert_eq!(argv.trim(), "issue comment 7 --repo a/b --body-file -");
        assert_eq!(std::fs::read_to_string(dir.path().join("stdin")).unwrap(), body);

        let missing = GhCliIssueComments::with_cli(HostCli::new(dir.path().join("no-gh")).with_base_env(env));
        let err = missing.post_issue_comment("u", "a/b", 7, "x").await.unwrap_err();
        assert!(err.to_string().contains("github_cli_missing"), "{err}");
    }

    #[test]
    fn argv_is_read_only_issue_list() {
        let argv = issue_list_argv("suoak/aioncore", Some("-label:wontfix"), 30);
        assert_eq!(&argv[..2], ["issue", "list"]);
        assert!(argv.windows(2).any(|w| w == ["--repo", "suoak/aioncore"]));
        assert!(argv.windows(2).any(|w| w == ["--state", "open"]));
        assert!(argv.windows(2).any(|w| w == ["--limit", "30"]));
        assert_eq!(argv.last().unwrap(), "--search=-label:wontfix");
        assert_eq!(issue_list_argv("a/b", None, 5).len(), 10);
    }

    #[test]
    fn parses_gh_json() {
        let issues = parse_issue_list(
            r#"[{"number":3,"title":"Crash","body":null,"labels":[{"name":"bug","color":"f00"}],"url":"https://github.com/a/b/issues/3"}]"#,
        )
        .unwrap();
        assert_eq!(
            issues,
            vec![GithubIssue {
                number: 3,
                title: "Crash".into(),
                body: String::new(),
                labels: vec!["bug".into()],
                url: "https://github.com/a/b/issues/3".into(),
            }]
        );
        assert!(parse_issue_list("not json").is_err());
    }

    #[test]
    fn failures_are_classified_and_redacted() {
        assert_eq!(
            classify_failure("a/b", "To get started with GitHub CLI, please run:  gh auth login"),
            GithubIssueSourceError::NotAuthenticated
        );
        assert_eq!(
            classify_failure("a/b", "GraphQL: Could not resolve to a Repository with the name 'a/b'."),
            GithubIssueSourceError::RepoNotFound("a/b".into())
        );
        match classify_failure("a/b", "boom ghp_abcdefghijklmnopqrstuvwxyz0123") {
            GithubIssueSourceError::Failed(message) => {
                assert!(!message.contains("ghp_"), "{message}");
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_gh_and_maps_missing_cli() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = std::env::var_os("PATH").unwrap_or_default();
        let env = vec![("PATH".into(), path)];

        let missing = GhCliIssueSource::with_cli(HostCli::new(dir.path().join("no-gh")).with_base_env(env.clone()));
        assert_eq!(
            missing.list_open_issues("a/b", None, 1).await.unwrap_err(),
            GithubIssueSourceError::CliMissing
        );

        let fake = dir.path().join("gh");
        std::fs::write(
            &fake,
            "#!/bin/sh\necho \"$@\" > \"$(dirname \"$0\")/argv\"\necho '[{\"number\":1,\"title\":\"T\",\"body\":\"B\",\"labels\":[],\"url\":\"u\"}]'\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        let source = GhCliIssueSource::with_cli(HostCli::new(&fake).with_base_env(env));
        let issues = source.list_open_issues("a/b", Some("label:bug"), 2).await.unwrap();
        assert_eq!(issues.len(), 1);
        let argv = std::fs::read_to_string(dir.path().join("argv")).unwrap();
        assert!(
            argv.starts_with("issue list --repo a/b --state open --limit 2"),
            "{argv}"
        );
    }
}
