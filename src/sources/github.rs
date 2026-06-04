use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, USER_AGENT};
use reqwest::Client;
use url::Url;

use crate::error::Result;
use crate::sources::{get_json, SourceCaps, SourceExtractor, SourceType};

const UA: &str = "grok-search-rs/0.1 (https://github.com/Episkey-G/GrokSearch-rs)";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubRaw {
    pub title: String,
    pub state: String,
    pub merged: Option<bool>,
    pub author: String,
    pub body: String,
    pub labels: Vec<String>,
    pub comments: Vec<GithubComment>,
    pub is_pr: bool,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct GithubComment {
    pub author: String,
    pub body: String,
    pub created_at: String,
}

pub struct GithubIssueExtractor {
    pub token: Option<String>,
}

pub struct GithubPrExtractor {
    pub token: Option<String>,
}

fn matches_github(url: &Url, segment_kind: &str) -> bool {
    if url.host_str() != Some("github.com") {
        return false;
    }
    let segs: Vec<&str> = match url.path_segments() {
        Some(it) => it.filter(|s| !s.is_empty()).collect(),
        None => return false,
    };
    segs.len() == 4 && segs[2] == segment_kind && segs[3].parse::<u64>().is_ok()
}

/// Page size for any comment list. `/comments` endpoints default to 30 results
/// per page, which silently drops later comments and prevents the renderer's
/// "more comments" fold from ever firing. Request `max_comments + 1` so the
/// renderer can both show `max_comments` and detect there are more. GitHub caps
/// `per_page` at 100; callers needing more than that would require true page
/// iteration (out of scope — `source_max_comments` defaults to 30).
fn per_page(max_comments: usize) -> usize {
    max_comments.saturating_add(1).min(100)
}

/// Conversation (issue) comments — present on both issues and PRs.
fn comments_url(owner: &str, repo: &str, number: &str, max_comments: usize) -> String {
    format!(
        "https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments?per_page={}",
        per_page(max_comments)
    )
}

/// Inline PR review comments (code-review threads). Distinct from conversation
/// comments and often where the actionable discussion lives.
fn pr_review_comments_url(owner: &str, repo: &str, number: &str, max_comments: usize) -> String {
    format!(
        "https://api.github.com/repos/{owner}/{repo}/pulls/{number}/comments?per_page={}",
        per_page(max_comments)
    )
}

/// PR review summaries (APPROVE / REQUEST_CHANGES / COMMENT bodies).
fn pr_reviews_url(owner: &str, repo: &str, number: &str, max_comments: usize) -> String {
    format!(
        "https://api.github.com/repos/{owner}/{repo}/pulls/{number}/reviews?per_page={}",
        per_page(max_comments)
    )
}

fn str_field(v: &serde_json::Value, k: &str) -> String {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn login(v: &serde_json::Value) -> String {
    v.get("user")
        .and_then(|u| u.get("login"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

/// Map a `[...comments...]` array (issue or inline review comments) to
/// `GithubComment`s. Both shapes expose `user.login`, `body`, `created_at`.
fn parse_comments(json: &serde_json::Value) -> Vec<GithubComment> {
    json.as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| GithubComment {
                    author: login(c),
                    body: str_field(c, "body"),
                    created_at: str_field(c, "created_at"),
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Map a `[...reviews...]` array to `GithubComment`s, keeping only reviews that
/// carry a body (an APPROVE with no text adds no evidence). Reviews timestamp
/// with `submitted_at` rather than `created_at`.
fn parse_review_bodies(json: &serde_json::Value) -> Vec<GithubComment> {
    json.as_array()
        .map(|arr| {
            arr.iter()
                .map(|r| GithubComment {
                    author: login(r),
                    body: str_field(r, "body"),
                    created_at: str_field(r, "submitted_at"),
                })
                .filter(|c| !c.body.trim().is_empty())
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn fetch(
    client: &Client,
    url: &Url,
    token: Option<&str>,
    is_pr: bool,
    max_comments: usize,
) -> Result<GithubRaw> {
    let segs: Vec<String> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default();
    if segs.len() < 4 {
        return Err(crate::error::GrokSearchError::Parse(
            "github: unexpected URL shape".into(),
        ));
    }
    let (owner, repo, number) = (&segs[0], &segs[1], &segs[3]);

    let auth = token.map(|t| format!("Bearer {t}"));
    let mut headers: Vec<(reqwest::header::HeaderName, &str)> = vec![(USER_AGENT, UA)];
    if let Some(ref a) = auth {
        headers.push((AUTHORIZATION, a.as_str()));
    }

    let main_url = if is_pr {
        format!("https://api.github.com/repos/{owner}/{repo}/pulls/{number}")
    } else {
        format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}")
    };
    let comments_url = comments_url(owner, repo, number, max_comments);

    // For PRs, the conversation thread (`/issues/{n}/comments`) omits inline
    // code-review comments and review summaries — usually the actionable
    // feedback. Fetch those two extra endpoints concurrently and merge them.
    // They are best-effort: a failure degrades to empty rather than failing the
    // whole specialist (so a PR still renders its body + conversation comments).
    let (main, comments) = if is_pr {
        let review_comments_url = pr_review_comments_url(owner, repo, number, max_comments);
        let reviews_url = pr_reviews_url(owner, repo, number, max_comments);
        let (main_res, conv_res, review_res, reviews_res) = tokio::join!(
            get_json(client, &main_url, &headers, "github"),
            get_json(client, &comments_url, &headers, "github_comments"),
            get_json(
                client,
                &review_comments_url,
                &headers,
                "github_review_comments"
            ),
            get_json(client, &reviews_url, &headers, "github_reviews"),
        );
        let main = main_res?;
        let mut comments = parse_comments(&conv_res?);
        if let Ok(json) = review_res {
            comments.extend(parse_comments(&json));
        }
        if let Ok(json) = reviews_res {
            comments.extend(parse_review_bodies(&json));
        }
        // ISO-8601 timestamps sort lexicographically = chronologically.
        comments.sort_by(|a, b| a.created_at.cmp(&b.created_at));
        (main, comments)
    } else {
        let (main_res, conv_res) = tokio::join!(
            get_json(client, &main_url, &headers, "github"),
            get_json(client, &comments_url, &headers, "github_comments"),
        );
        (main_res?, parse_comments(&conv_res?))
    };

    let labels = main
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l.get("name").and_then(|n| n.as_str()).map(String::from))
                .collect()
        })
        .unwrap_or_default();

    Ok(GithubRaw {
        title: str_field(&main, "title"),
        state: str_field(&main, "state"),
        merged: main.get("merged").and_then(|v| v.as_bool()),
        author: main
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        body: str_field(&main, "body"),
        labels,
        comments,
        is_pr,
    })
}

pub fn render(raw: &GithubRaw, caps: &SourceCaps) -> String {
    let mut out = format!("# {}\n\n", raw.title);
    let merged_suffix = if raw.is_pr {
        match raw.merged {
            Some(true) => " (merged)",
            _ if raw.state == "closed" => " (closed, not merged)",
            _ => "",
        }
    } else {
        ""
    };
    out.push_str(&format!("**State:** {}{}\n", raw.state, merged_suffix));
    out.push_str(&format!("**Author:** {}\n", raw.author));
    if !raw.labels.is_empty() {
        out.push_str(&format!("**Labels:** {}\n", raw.labels.join(", ")));
    }
    out.push_str(&format!("\n{}\n\n## Comments\n\n", raw.body));
    for c in raw.comments.iter().take(caps.max_comments) {
        out.push_str(&format!(
            "### Comment by {} ({})\n\n{}\n\n",
            c.author, c.created_at, c.body
        ));
    }
    if raw.comments.len() > caps.max_comments {
        let extra = raw.comments.len() - caps.max_comments;
        out.push_str(&format!("_还有 {extra} 条评论_\n"));
    }
    out
}

#[async_trait]
impl SourceExtractor for GithubIssueExtractor {
    fn matches(&self, url: &Url) -> bool {
        matches_github(url, "issues")
    }
    fn kind(&self) -> SourceType {
        SourceType::GithubIssue
    }
    async fn fetch_render(&self, client: &Client, url: &Url, caps: &SourceCaps) -> Result<String> {
        let raw = fetch(client, url, self.token.as_deref(), false, caps.max_comments).await?;
        Ok(render(&raw, caps))
    }
}

#[async_trait]
impl SourceExtractor for GithubPrExtractor {
    fn matches(&self, url: &Url) -> bool {
        matches_github(url, "pull")
    }
    fn kind(&self) -> SourceType {
        SourceType::GithubPull
    }
    async fn fetch_render(&self, client: &Client, url: &Url, caps: &SourceCaps) -> Result<String> {
        let raw = fetch(client, url, self.token.as_deref(), true, caps.max_comments).await?;
        Ok(render(&raw, caps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_url_requests_one_more_than_cap() {
        // +1 over max_comments lets render() detect "more comments" and fold.
        let u = comments_url("o", "r", "5", 30);
        assert_eq!(
            u,
            "https://api.github.com/repos/o/r/issues/5/comments?per_page=31"
        );
    }

    #[test]
    fn comments_url_clamps_per_page_to_github_max() {
        let u = comments_url("o", "r", "5", 250);
        assert!(u.ends_with("?per_page=100"), "got: {u}");
    }

    #[test]
    fn pr_endpoints_target_pulls_paths() {
        assert!(pr_review_comments_url("o", "r", "7", 30)
            .starts_with("https://api.github.com/repos/o/r/pulls/7/comments?per_page=31"));
        assert!(pr_reviews_url("o", "r", "7", 30)
            .starts_with("https://api.github.com/repos/o/r/pulls/7/reviews?per_page=31"));
    }

    #[test]
    fn parse_review_bodies_skips_empty_and_maps_submitted_at() {
        let json = serde_json::json!([
            { "user": { "login": "alice" }, "body": "needs changes", "submitted_at": "2024-01-02T00:00:00Z" },
            { "user": { "login": "bob" }, "body": "   ", "submitted_at": "2024-01-03T00:00:00Z" },
            { "user": { "login": "carol" }, "body": "", "submitted_at": "2024-01-04T00:00:00Z" }
        ]);
        let out = parse_review_bodies(&json);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].author, "alice");
        assert_eq!(out[0].created_at, "2024-01-02T00:00:00Z");
    }

    #[test]
    fn parse_comments_maps_user_login_and_timestamps() {
        let json = serde_json::json!([
            { "user": { "login": "dave" }, "body": "hi", "created_at": "2024-01-01T00:00:00Z" }
        ]);
        let out = parse_comments(&json);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].author, "dave");
        assert_eq!(out[0].body, "hi");
        assert_eq!(out[0].created_at, "2024-01-01T00:00:00Z");
    }
}
