use async_trait::async_trait;
use reqwest::header::USER_AGENT;
use reqwest::Client;
use url::Url;

use crate::error::{GrokSearchError, Result};
use crate::sources::{get_json, SourceCaps, SourceExtractor, SourceType};

const UA: &str = "grok-search-rs/0.1 (https://github.com/Episkey-G/GrokSearch-rs)";

const EXCLUDED_NS: &[&str] = &[
    "Special",
    "Talk",
    "Category",
    "Help",
    "User",
    "Wikipedia",
    "File",
    "Template",
    "Portal",
    "Draft",
];

pub struct WikiRaw {
    pub title: String,
    pub extract: String,
    pub lang: String,
}

pub struct WikipediaExtractor;

fn lang_from_host(host: &str) -> &str {
    host.strip_suffix(".wikipedia.org").unwrap_or("en")
}

/// D-05: full article body (`exintro=false`) as clean plaintext
/// (`explaintext=true` strips HTML/nav server-side).
pub(crate) async fn fetch(client: &Client, url: &Url) -> Result<WikiRaw> {
    let host = url.host_str().unwrap_or("");
    let lang = lang_from_host(host).to_string();
    let title_param = &url.path()["/wiki/".len()..];
    let api_url = format!(
        "https://{lang}.wikipedia.org/w/api.php?action=query&prop=extracts&explaintext=true&exintro=false&titles={title_param}&format=json&redirects=1"
    );
    let headers = [(USER_AGENT, UA)];
    let json = get_json(client, &api_url, &headers, "wikipedia").await?;

    let pages = json
        .get("query")
        .and_then(|q| q.get("pages"))
        .and_then(|p| p.as_object())
        .ok_or_else(|| GrokSearchError::Provider("wikipedia: no pages".into()))?;
    let page = pages
        .values()
        .next()
        .ok_or_else(|| GrokSearchError::Provider("wikipedia: empty pages".into()))?;

    let title = page
        .get("title")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let extract = page
        .get("extract")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if extract.trim().is_empty() {
        return Err(GrokSearchError::Provider("wikipedia: empty extract".into()));
    }
    Ok(WikiRaw {
        title,
        extract,
        lang,
    })
}

pub fn render(raw: &WikiRaw, _caps: &SourceCaps) -> String {
    // explaintext=true already produced clean plaintext; max_chars truncation
    // is applied later by service.rs web_fetch.
    format!("# {}\n\n{}\n", raw.title, raw.extract)
}

#[async_trait]
impl SourceExtractor for WikipediaExtractor {
    fn matches(&self, url: &Url) -> bool {
        let host = url.host_str().unwrap_or("");
        if !host.ends_with(".wikipedia.org") {
            return false;
        }
        let path = url.path();
        if !path.starts_with("/wiki/") {
            return false;
        }
        let title = &path["/wiki/".len()..];
        if title.is_empty() {
            return false;
        }
        let ns = title.split(':').next().unwrap_or("");
        !EXCLUDED_NS.contains(&ns)
    }
    fn kind(&self) -> SourceType {
        SourceType::Wikipedia
    }
    async fn fetch_render(&self, client: &Client, url: &Url, caps: &SourceCaps) -> Result<String> {
        let raw = fetch(client, url).await?;
        Ok(render(&raw, caps))
    }
}
