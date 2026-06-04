use async_trait::async_trait;
use percent_encoding::percent_decode_str;
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

/// Build the action-API query URL for `title_param` (the raw `/wiki/<...>` path
/// suffix). The title is percent-decoded then re-encoded as a proper query
/// value, so titles containing `&`, `=`, `?`, spaces, etc. (e.g. `AT&T`) are not
/// spliced raw into the query string. Pure (no I/O) so it can be unit-tested.
fn build_api_url(lang: &str, title_param: &str) -> String {
    let title = percent_decode_str(title_param).decode_utf8_lossy();
    let mut api = Url::parse(&format!("https://{lang}.wikipedia.org/w/api.php"))
        .expect("wikipedia api base is a valid URL");
    api.query_pairs_mut()
        .append_pair("action", "query")
        .append_pair("prop", "extracts")
        .append_pair("explaintext", "true")
        .append_pair("exintro", "false")
        .append_pair("titles", &title)
        .append_pair("format", "json")
        .append_pair("redirects", "1");
    api.into()
}

/// D-05: full article body (`exintro=false`) as clean plaintext
/// (`explaintext=true` strips HTML/nav server-side).
pub(crate) async fn fetch(client: &Client, url: &Url) -> Result<WikiRaw> {
    let host = url.host_str().unwrap_or("");
    let lang = lang_from_host(host).to_string();
    let title_param = &url.path()["/wiki/".len()..];
    let api_url = build_api_url(&lang, title_param);
    let headers = [(USER_AGENT, UA)];
    let json = get_json(client, &api_url, &headers, "wikipedia").await?;
    parse_page(&json, &lang)
}

/// Parse the Wikipedia action-API `query.pages` response into a WikiRaw.
/// Pure (no I/O) so it can be unit-tested offline against a fixture.
pub fn parse_page(json: &serde_json::Value, lang: &str) -> Result<WikiRaw> {
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
        lang: lang.to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_api_url_encodes_query_delimiters_in_title() {
        // A raw `&` in the title must not split into a separate query param.
        let u = build_api_url("en", "AT&T");
        assert!(u.contains("titles=AT%26T"), "got: {u}");
        assert!(!u.contains("titles=AT&T"), "raw ampersand leaked: {u}");
    }

    #[test]
    fn build_api_url_does_not_double_encode_preencoded_title() {
        // Already percent-encoded path slice decodes once, re-encodes once.
        let u = build_api_url("en", "AT%26T");
        assert!(u.contains("titles=AT%26T"), "got: {u}");
        assert!(!u.contains("AT%2526T"), "double-encoded: {u}");
    }
}
