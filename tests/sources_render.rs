use grok_search_rs::sources::arxiv::{render as arxiv_render, ArxivExtractor};
use grok_search_rs::sources::github::{
    render as gh_render, GithubIssueExtractor, GithubPrExtractor, GithubRaw,
};
use grok_search_rs::sources::stackexchange::{render as se_render, SeRaw, StackExchangeExtractor};
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

fn se_fixture() -> SeRaw {
    serde_json::from_str(include_str!("fixtures/sources/stackexchange.json")).unwrap()
}

#[test]
fn se_render_accepted_answer_marked_and_first() {
    let out = se_render(&se_fixture(), &SourceCaps::default());
    let star = out.find("★ 采纳答案").expect("accepted marker present");
    let dave = out.find("dave").expect("non-accepted author present");
    assert!(star < dave, "accepted answer must come before non-accepted");
}

#[test]
fn se_render_folds_extra_answers_at_cap() {
    let caps = SourceCaps {
        max_answers: 2,
        max_comments: 30,
    };
    let out = se_render(&se_fixture(), &caps);
    assert!(out.contains("还有 2 条"), "fold: {out}");
}

#[test]
fn se_render_accepted_answer_includes_comments() {
    let out = se_render(&se_fixture(), &SourceCaps::default());
    assert!(out.contains("Also `list[::-1]`"), "accepted comment: {out}");
}

#[test]
fn se_render_other_answers_have_no_comments() {
    let out = se_render(&se_fixture(), &SourceCaps::default());
    let after_first_non_accepted = out.split("## 答案").nth(1).unwrap_or("");
    assert!(
        !after_first_non_accepted.contains("> **"),
        "non-accepted answers must render no comments"
    );
}

#[test]
fn se_matcher_full_network_positive() {
    let se = StackExchangeExtractor;
    let m = |u: &str| Url::parse(u).unwrap();
    for pos in [
        "https://stackoverflow.com/questions/1234/how-do-i",
        "https://serverfault.com/questions/99",
        "https://superuser.com/questions/5678",
        "https://askubuntu.com/questions/111",
        "https://mathoverflow.net/questions/222",
        "https://math.stackexchange.com/questions/333",
        "https://codereview.stackexchange.com/questions/444",
    ] {
        assert!(se.matches(&m(pos)), "should match {pos}");
    }
}

#[test]
fn se_matcher_non_question_negative() {
    let se = StackExchangeExtractor;
    let m = |u: &str| Url::parse(u).unwrap();
    for neg in [
        "https://stackoverflow.com/users/42/alice",
        "https://stackoverflow.com/tags/rust",
        "https://stackoverflow.com/questions",
    ] {
        assert!(!se.matches(&m(neg)), "should reject {neg}");
    }
}

const ARXIV_FIXTURE: &str = include_str!("fixtures/sources/arxiv_atom.xml");

#[test]
fn arxiv_parse_atom_returns_title() {
    let raw = ArxivExtractor::parse_atom(ARXIV_FIXTURE).expect("parse");
    assert_eq!(raw.title, "Attention Is All You Need");
}

#[test]
fn arxiv_parse_atom_lists_all_authors() {
    let raw = ArxivExtractor::parse_atom(ARXIV_FIXTURE).expect("parse");
    assert!(raw.authors.iter().any(|a| a == "Ashish Vaswani"));
    assert!(raw.authors.len() >= 2);
}

#[test]
fn arxiv_parse_atom_returns_categories() {
    let raw = ArxivExtractor::parse_atom(ARXIV_FIXTURE).expect("parse");
    assert!(raw.categories.iter().any(|c| c == "cs.CL"));
}

#[test]
fn arxiv_parse_atom_returns_pdf_link() {
    let raw = ArxivExtractor::parse_atom(ARXIV_FIXTURE).expect("parse");
    assert!(raw.pdf_link.contains("pdf"), "pdf_link: {}", raw.pdf_link);
}

#[test]
fn arxiv_render_shows_title_and_pdf_link() {
    let raw = ArxivExtractor::parse_atom(ARXIV_FIXTURE).expect("parse");
    let out = arxiv_render(&raw, &SourceCaps::default());
    assert!(out.contains("# Attention Is All You Need"));
    assert!(out.contains("[PDF]"));
    assert!(out.contains("[Abstract]"));
}

#[test]
fn arxiv_matcher_positive_abs_and_pdf() {
    let ax = ArxivExtractor;
    let m = |u: &str| Url::parse(u).unwrap();
    assert!(ax.matches(&m("https://arxiv.org/abs/1706.03762")));
    assert!(ax.matches(&m("https://arxiv.org/pdf/1706.03762")));
    assert!(ax.matches(&m("https://arxiv.org/abs/2106.09685v2")));
}

#[test]
fn arxiv_matcher_negative_non_paper_paths() {
    let ax = ArxivExtractor;
    let m = |u: &str| Url::parse(u).unwrap();
    assert!(!ax.matches(&m("https://arxiv.org/")));
    assert!(!ax.matches(&m("https://arxiv.org/search/")));
    assert!(!ax.matches(&m("https://export.arxiv.org/api/query?id_list=1706.03762")));
}
