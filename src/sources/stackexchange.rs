use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use reqwest::Client;
use url::Url;

use crate::error::{GrokSearchError, Result};
use crate::sources::{get_json, SourceCaps, SourceExtractor, SourceType};

const UA: &str = "grok-search-rs/0.1 (https://github.com/Episkey-G/GrokSearch-rs)";

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SeComment {
    pub author: String,
    pub body: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SeAnswer {
    pub is_accepted: bool,
    pub score: i64,
    pub author: String,
    pub body: String,
    pub comments: Vec<SeComment>,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct SeRaw {
    pub title: String,
    pub body: String,
    pub site: String,
    pub answers: Vec<SeAnswer>,
}

pub struct StackExchangeExtractor;

/// api_site_parameter for a non-meta host.
fn base_site_param(host: &str) -> String {
    match host {
        "stackoverflow.com" => "stackoverflow".to_string(),
        "serverfault.com" => "serverfault".to_string(),
        "superuser.com" => "superuser".to_string(),
        "askubuntu.com" => "askubuntu".to_string(),
        "mathoverflow.net" => "mathoverflow".to_string(),
        other => other
            .strip_suffix(".stackexchange.com")
            .unwrap_or(other)
            .to_string(),
    }
}

fn site_from_host(host: &str) -> String {
    // Meta hosts (central `meta.stackexchange.com` and per-site
    // `meta.stackoverflow.com`, `meta.serverfault.com`, …) map to the
    // `meta.<base>` api_site_parameter — naive suffix stripping would yield a
    // bare `meta` and break the API call.
    if let Some(base) = host.strip_prefix("meta.") {
        if base == "stackexchange.com" {
            return "meta.stackexchange".to_string();
        }
        return format!("meta.{}", base_site_param(base));
    }
    base_site_param(host)
}

fn is_se_host(host: &str) -> bool {
    matches!(
        host,
        "stackoverflow.com"
            | "serverfault.com"
            | "superuser.com"
            | "askubuntu.com"
            | "mathoverflow.net"
            | "meta.stackoverflow.com"
            | "meta.serverfault.com"
            | "meta.superuser.com"
            | "meta.askubuntu.com"
    ) || host.ends_with(".stackexchange.com")
}

/// StackExchange answers page size. Defaults to 30/page; request
/// `max_answers + 1` (capped at the API's 100) so `render` sees every answer
/// within the cap and can emit its "more answers" marker. Mirrors the GitHub
/// comments paging approach.
fn answers_pagesize(max_answers: usize) -> usize {
    max_answers.saturating_add(1).min(100)
}

fn field_str(v: &serde_json::Value, primary: &str, fallback: &str) -> String {
    v.get(primary)
        .or_else(|| v.get(fallback))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn owner_name(v: &serde_json::Value) -> String {
    v.get("owner")
        .and_then(|o| o.get("display_name"))
        .and_then(|n| n.as_str())
        .unwrap_or("")
        .to_string()
}

/// Map an `/answers` (or embedded `answers`) JSON payload into `SeAnswer`s.
/// Tolerant of missing fields so a partial response still yields usable bodies.
fn parse_answers(json: &serde_json::Value) -> Vec<SeAnswer> {
    json.get("items")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .map(|a| SeAnswer {
                    is_accepted: a
                        .get("is_accepted")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                    score: a.get("score").and_then(|v| v.as_i64()).unwrap_or(0),
                    author: owner_name(a),
                    body: field_str(a, "body_markdown", "body"),
                    comments: a
                        .get("comments")
                        .and_then(|v| v.as_array())
                        .map(|carr| {
                            carr.iter()
                                .map(|c| SeComment {
                                    author: owner_name(c),
                                    body: field_str(c, "body_markdown", "body"),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) async fn fetch(client: &Client, url: &Url, max_answers: usize) -> Result<SeRaw> {
    let host = url.host_str().unwrap_or("");
    let site = site_from_host(host);
    let segs: Vec<&str> = url
        .path_segments()
        .map(|it| it.filter(|s| !s.is_empty()).collect())
        .unwrap_or_default();
    let id = segs.get(1).copied().unwrap_or_default();
    let headers = [(USER_AGENT, UA)];

    // `/questions/{id}` with `filter=withbody` returns the QUESTION body but
    // never the answers array, so the question call alone yields zero answers.
    let q_url =
        format!("https://api.stackexchange.com/2.3/questions/{id}?site={site}&filter=withbody");
    let q_json = get_json(client, &q_url, &headers, "stackexchange").await?;
    let item = q_json
        .get("items")
        .and_then(|i| i.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| GrokSearchError::Provider("stackexchange: empty items".into()))?;

    // Answers come from the dedicated endpoint, vote-sorted with bodies. This is
    // best-effort: a failed/rate-limited answers call still returns the question
    // rather than the whole specialist failing. NOTE: per-answer comments still
    // need a custom SE filter (via /filters/create) and remain out of scope; the
    // renderer degrades gracefully when comment lists are empty. Anonymous calls
    // are rate-limited (~300/day); a future key could lift that.
    let a_url = format!(
        "https://api.stackexchange.com/2.3/questions/{id}/answers?site={site}&filter=withbody&order=desc&sort=votes&pagesize={}",
        answers_pagesize(max_answers)
    );
    let answers = match get_json(client, &a_url, &headers, "stackexchange").await {
        Ok(a_json) => parse_answers(&a_json),
        Err(_) => Vec::new(),
    };

    Ok(SeRaw {
        title: item
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        body: field_str(item, "body_markdown", "body"),
        site,
        answers,
    })
}

pub fn render(raw: &SeRaw, caps: &SourceCaps) -> String {
    let mut answers: Vec<&SeAnswer> = raw.answers.iter().collect();
    answers.sort_by(|a, b| {
        b.is_accepted
            .cmp(&a.is_accepted)
            .then(b.score.cmp(&a.score))
    });
    let total = answers.len();

    let mut out = format!("# {}\n\n{}\n\n---\n\n", raw.title, raw.body);
    for ans in answers.iter().take(caps.max_answers) {
        if ans.is_accepted {
            out.push_str(&format!(
                "## ★ 采纳答案 (↑{})\n\n{}\n\n",
                ans.score, ans.body
            ));
            for c in ans.comments.iter().take(caps.max_comments) {
                out.push_str(&format!("> **{}:** {}\n", c.author, c.body));
            }
            if ans.comments.len() > caps.max_comments {
                let extra = ans.comments.len() - caps.max_comments;
                out.push_str(&format!("_还有 {extra} 条评论_\n"));
            }
            out.push('\n');
        } else {
            out.push_str(&format!(
                "## 答案 by {} (↑{})\n\n{}\n\n",
                ans.author, ans.score, ans.body
            ));
        }
    }
    if total > caps.max_answers {
        let extra = total - caps.max_answers;
        out.push_str(&format!("_还有 {extra} 条_\n"));
    }
    out
}

#[async_trait]
impl SourceExtractor for StackExchangeExtractor {
    fn matches(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        if !is_se_host(host) {
            return false;
        }
        let segs: Vec<&str> = match url.path_segments() {
            Some(it) => it.filter(|s| !s.is_empty()).collect(),
            None => return false,
        };
        segs.len() >= 2 && segs[0] == "questions" && segs[1].parse::<u64>().is_ok()
    }
    fn kind(&self) -> SourceType {
        SourceType::Stackexchange
    }
    async fn fetch_render(&self, client: &Client, url: &Url, caps: &SourceCaps) -> Result<String> {
        let raw = fetch(client, url, caps.max_answers).await?;
        Ok(render(&raw, caps))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_from_host_preserves_meta_stackexchange() {
        // Meta Stack Exchange's api_site_parameter is "meta.stackexchange",
        // not "meta" — stripping the suffix would break the specialist path.
        assert_eq!(
            site_from_host("meta.stackexchange.com"),
            "meta.stackexchange"
        );
    }

    #[test]
    fn site_from_host_strips_regular_stackexchange_subdomain() {
        assert_eq!(site_from_host("math.stackexchange.com"), "math");
        assert_eq!(site_from_host("stackoverflow.com"), "stackoverflow");
    }

    #[test]
    fn site_from_host_maps_per_site_meta_hosts() {
        // Per-site metas use api_site_parameter "meta.<base>".
        assert_eq!(
            site_from_host("meta.stackoverflow.com"),
            "meta.stackoverflow"
        );
        assert_eq!(site_from_host("meta.serverfault.com"), "meta.serverfault");
        assert_eq!(site_from_host("meta.askubuntu.com"), "meta.askubuntu");
    }

    #[test]
    fn is_se_host_accepts_per_site_meta_hosts() {
        assert!(is_se_host("meta.stackoverflow.com"));
        assert!(is_se_host("meta.superuser.com"));
        assert!(is_se_host("meta.stackexchange.com"));
        assert!(!is_se_host("example.com"));
    }

    #[test]
    fn answers_pagesize_requests_one_more_than_cap_capped_at_100() {
        assert_eq!(answers_pagesize(5), 6);
        assert_eq!(answers_pagesize(250), 100);
    }
}
