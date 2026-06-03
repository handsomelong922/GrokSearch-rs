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

pub(crate) async fn fetch(
    client: &Client,
    url: &Url,
    token: Option<&str>,
    is_pr: bool,
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
    let comments_url =
        format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments");

    let (main_res, comments_res) = tokio::join!(
        get_json(client, &main_url, &headers, "github"),
        get_json(client, &comments_url, &headers, "github_comments"),
    );
    let main = main_res?;
    let comments_json = comments_res?;

    let str_field = |v: &serde_json::Value, k: &str| {
        v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string()
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
    let comments = comments_json
        .as_array()
        .map(|arr| {
            arr.iter()
                .map(|c| GithubComment {
                    author: c
                        .get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string(),
                    body: str_field(c, "body"),
                    created_at: str_field(c, "created_at"),
                })
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
        let raw = fetch(client, url, self.token.as_deref(), false).await?;
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
        let raw = fetch(client, url, self.token.as_deref(), true).await?;
        Ok(render(&raw, caps))
    }
}
