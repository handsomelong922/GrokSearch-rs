use grok_search_rs::sources::github::{
    render as gh_render, GithubIssueExtractor, GithubPrExtractor, GithubRaw,
};
use grok_search_rs::sources::{SourceCaps, SourceExtractor};
use url::Url;

fn issue_fixture() -> GithubRaw {
    serde_json::from_str(include_str!("fixtures/sources/github_issue.json")).unwrap()
}

fn pr_fixture() -> GithubRaw {
    serde_json::from_str(include_str!("fixtures/sources/github_pr.json")).unwrap()
}

#[test]
fn github_issue_render_shows_title_and_state() {
    let out = gh_render(&issue_fixture(), &SourceCaps::default());
    assert!(out.contains("Fix segfault in parser"), "title: {out}");
    assert!(out.contains("open"), "state: {out}");
}

#[test]
fn github_issue_render_shows_labels() {
    let out = gh_render(&issue_fixture(), &SourceCaps::default());
    assert!(out.contains("bug"));
    assert!(out.contains("good first issue"));
}

#[test]
fn github_issue_render_folds_comments_at_cap() {
    let caps = SourceCaps {
        max_answers: 5,
        max_comments: 2,
    };
    let out = gh_render(&issue_fixture(), &caps);
    assert!(out.contains("还有 1 条评论"), "fold: {out}");
}

#[test]
fn github_pr_render_shows_merged_state() {
    let out = gh_render(&pr_fixture(), &SourceCaps::default());
    assert!(out.contains("merged"), "pr: {out}");
}

#[test]
fn github_matcher_strict_positive_and_negative() {
    let issue = GithubIssueExtractor { token: None };
    let pr = GithubPrExtractor { token: None };
    let m = |u: &str| Url::parse(u).unwrap();

    assert!(issue.matches(&m("https://github.com/owner/repo/issues/42")));
    assert!(pr.matches(&m("https://github.com/owner/repo/pull/7")));

    for neg in [
        "https://github.com/",
        "https://github.com/owner/repo/issues",
        "https://github.com/owner/repo/pull/7/files",
        "https://gist.github.com/user/abc",
        "https://github.com/owner/repo/discussions/1",
        "https://github.com/owner/repo/blob/main/README.md",
    ] {
        assert!(!issue.matches(&m(neg)), "issue should reject {neg}");
        assert!(!pr.matches(&m(neg)), "pr should reject {neg}");
    }
}
