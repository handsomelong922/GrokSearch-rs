use crate::error::{GrokSearchError, Result};
use crate::model::source::Source;
use crate::providers::http::{build_client, post_json_with_status};
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct KeyRing {
    keys: Vec<String>,
    cursor: AtomicUsize,
}

impl KeyRing {
    fn parse(raw: &str) -> Self {
        let mut keys: Vec<String> = raw
            .split(',')
            .map(str::trim)
            .filter(|segment| !segment.is_empty())
            .map(str::to_string)
            .collect();
        if keys.is_empty() {
            keys.push(raw.to_string());
        }
        Self {
            keys,
            cursor: AtomicUsize::new(0),
        }
    }

    fn len(&self) -> usize {
        self.keys.len()
    }

    fn start(&self) -> usize {
        self.cursor.fetch_add(1, Ordering::Relaxed) % self.keys.len()
    }

    fn key(&self, index: usize) -> &str {
        &self.keys[index % self.keys.len()]
    }
}

fn is_key_scoped_status(status: u16) -> bool {
    matches!(status, 401 | 402 | 403 | 429)
}

#[derive(Clone)]
pub struct FirecrawlProvider {
    client: Client,
    api_url: String,
    keys: Arc<KeyRing>,
}

impl FirecrawlProvider {
    pub fn new(api_url: impl Into<String>, api_key: impl Into<String>, timeout: Duration) -> Self {
        Self::with_client(build_client(timeout), api_url, api_key)
    }

    /// Construct with an externally provided `reqwest::Client`. Used by
    /// `SearchService::new` to share one tuned client across providers.
    pub fn with_client(
        client: Client,
        api_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            client,
            api_url: api_url.into().trim_end_matches('/').to_string(),
            keys: Arc::new(KeyRing::parse(&api_key.into())),
        }
    }

    pub async fn search(&self, query: &str, max_results: usize) -> Result<Vec<Source>> {
        let raw = self
            .post("search", &json!({ "query": query, "limit": max_results }))
            .await?;
        Ok(normalize_firecrawl_results(&raw))
    }

    pub async fn scrape(&self, url: &str) -> Result<String> {
        let raw = self
            .post("scrape", &json!({ "url": url, "formats": ["markdown"] }))
            .await?;
        let content = raw
            .get("data")
            .and_then(|data| data.get("markdown").or_else(|| data.get("content")))
            .or_else(|| raw.get("markdown"))
            .or_else(|| raw.get("content"))
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|text| !text.trim().is_empty());

        content.ok_or_else(|| {
            GrokSearchError::Provider("Firecrawl scrape returned empty content".to_string())
        })
    }

    async fn post(&self, path: &str, body: &Value) -> Result<Value> {
        let endpoint = format!("{}/{}", self.api_url, path.trim_start_matches('/'));
        let attempts = self.keys.len();
        let start = self.keys.start();
        let mut last_error = None;
        for offset in 0..attempts {
            let key = self.keys.key(start + offset);
            match post_json_with_status(&self.client, &endpoint, key, body, "Firecrawl").await {
                Ok(value) => return Ok(value),
                Err(failure) => {
                    let key_scoped = failure.status.is_some_and(is_key_scoped_status);
                    if key_scoped && offset + 1 < attempts {
                        eprintln!(
                            "grok-search-rs: Firecrawl key {}/{} hit HTTP {}; rotating to next key",
                            (start + offset) % attempts + 1,
                            attempts,
                            failure.status.unwrap_or_default(),
                        );
                        last_error = Some(failure.error);
                        continue;
                    }
                    return Err(failure.error);
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            GrokSearchError::Provider("Firecrawl request failed with no attempts".to_string())
        }))
    }
}

pub fn normalize_firecrawl_results(raw: &Value) -> Vec<Source> {
    raw.get("data")
        .or_else(|| raw.get("results"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            if let Some(url) = item.as_str() {
                return Some(Source::new(url, "firecrawl"));
            }
            let url = item.get("url").and_then(Value::as_str)?;
            let mut source = Source::new(url, "firecrawl");
            if let Some(title) = item.get("title").and_then(Value::as_str) {
                source = source.with_title(title);
            }
            if let Some(description) = item
                .get("description")
                .or_else(|| item.get("markdown"))
                .or_else(|| item.get("content"))
                .and_then(Value::as_str)
            {
                source = source.with_description(description);
            }
            if let Some(published_date) = item.get("published_date").and_then(Value::as_str) {
                source = source.with_published_date(published_date);
            }
            Some(source)
        })
        .collect()
}
