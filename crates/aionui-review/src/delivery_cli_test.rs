use super::*;

#[test]
fn remote_host_handles_common_url_forms() {
    assert_eq!(remote_host("https://github.com/o/r.git").as_deref(), Some("github.com"));
    assert_eq!(
        remote_host("https://user:tok@GitHub.com/o/r").as_deref(),
        Some("github.com")
    );
    assert_eq!(
        remote_host("ssh://git@gitlab.example.com:2222/g/r.git").as_deref(),
        Some("gitlab.example.com")
    );
    assert_eq!(remote_host("git@github.com:o/r.git").as_deref(), Some("github.com"));
    assert_eq!(remote_host("/srv/git/r.git"), None);
    assert_eq!(remote_host("file:///srv/git/r.git"), None);
    assert_eq!(remote_host("C:\\repos\\r.git"), None);
    assert_eq!(remote_host("../r.git"), None);
}

#[test]
fn provider_by_host() {
    assert_eq!(provider_for_host("github.com"), HostProvider::Github);
    assert_eq!(provider_for_host("gitlab.com"), HostProvider::Gitlab);
    assert_eq!(provider_for_host("gitlab.corp.local"), HostProvider::Gitlab);
    assert_eq!(provider_for_host("gitea.corp.local"), HostProvider::Other);
}

#[test]
fn gh_argv_passes_body_on_stdin() {
    let args = PrCreateArgs {
        branch: "feat/x",
        base: "main",
        title: "--title-like; rm -rf",
        body: "body $(x)",
        draft: true,
    };
    let (argv, stdin) = create_argv(HostProvider::Github, &args).unwrap();
    assert_eq!(
        argv,
        [
            "pr",
            "create",
            "--head",
            "feat/x",
            "--base",
            "main",
            "--title",
            "--title-like; rm -rf",
            "--body-file",
            "-",
            "--draft"
        ]
    );
    assert_eq!(stdin.unwrap(), b"body $(x)");
    assert!(!argv.iter().any(|a| a.contains("$(x)")));
}

#[test]
fn glab_argv_is_non_interactive() {
    let args = PrCreateArgs {
        branch: "feat/x",
        base: "main",
        title: "T",
        body: "B",
        draft: false,
    };
    let (argv, stdin) = create_argv(HostProvider::Gitlab, &args).unwrap();
    assert_eq!(argv[..2], ["mr", "create"]);
    assert!(argv.contains(&"--yes".to_owned()));
    assert!(!argv.contains(&"--draft".to_owned()));
    assert!(stdin.is_none());
    assert!(create_argv(HostProvider::Other, &args).is_none());
}

#[test]
fn parses_pr_and_mr_urls() {
    let gh = "Creating pull request for feat/x into main in o/r\n\nhttps://github.com/o/r/pull/42\n";
    assert_eq!(
        parse_pr_url(gh),
        Some(("https://github.com/o/r/pull/42".into(), "42".into()))
    );
    let glab = "!7 T (feat/x)\n https://gitlab.com/g/r/-/merge_requests/7.\n";
    assert_eq!(
        parse_pr_url(glab),
        Some(("https://gitlab.com/g/r/-/merge_requests/7".into(), "7".into()))
    );
    assert_eq!(parse_pr_url("no url here"), None);
}

#[test]
fn redacts_tokens_and_userinfo() {
    let raw =
        "fatal: https://me:secret@github.com/o/r token ghp_abcdefghijklmnopqrstuvwxyz0123 Bearer abcdefghijklmnop";
    let red = redact_output(raw);
    assert!(!red.contains("secret"));
    assert!(!red.contains("ghp_abc"));
    assert!(!red.contains("abcdefghijklmnop"));
    assert!(red.contains("https://github.com/o/r"));
}
