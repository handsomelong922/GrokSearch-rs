use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::cache::SourceCache;
use crate::config::{AuthMode, Config};
use crate::credentials::{OAuthCredential, StaticApiKeyCredential};
use crate::error::{GrokSearchError, Result};
use crate::model::search::{
    ContentBlock, SearchFilters, SearchMessage, SearchRequest, SearchResponse, SearchTool,
};
use crate::model::source::{merge_sources, Source};
use crate::model::tool::{GetSourcesOutput, WebFetchOutput, WebSearchInput, WebSearchOutput};
use crate::providers::firecrawl::FirecrawlProvider;
use crate::providers::grok::GrokResponsesProvider;
use crate::providers::tavily::TavilyProvider;

#[async_trait]
pub trait AiProvider: Send + Sync {
    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse>;
}

#[async_trait]
pub trait SourceProvider: Send + Sync {
    async fn search_sources(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>>;
    async fn fetch(&self, url: &str) -> Result<String>;
    async fn map(&self, url: &str, max_results: usize) -> Result<Vec<Source>>;
}

#[async_trait]
impl AiProvider for GrokResponsesProvider {
    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        GrokResponsesProvider::search(self, request).await
    }
}

#[async_trait]
impl AiProvider for crate::providers::openai_compatible::OpenAICompatProvider {
    async fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        crate::providers::openai_compatible::OpenAICompatProvider::search(self, request).await
    }
}

#[async_trait]
impl SourceProvider for TavilyProvider {
    async fn search_sources(
        &self,
        query: &str,
        max_results: usize,
        filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        self.search(query, max_results, filters).await
    }

    async fn fetch(&self, url: &str) -> Result<String> {
        self.extract(url).await
    }

    async fn map(&self, url: &str, max_results: usize) -> Result<Vec<Source>> {
        self.map(url, max_results).await
    }
}

#[async_trait]
impl SourceProvider for FirecrawlProvider {
    async fn search_sources(
        &self,
        query: &str,
        max_results: usize,
        _filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        // Firecrawl search has no structured recency/domain filter; ignore filters.
        FirecrawlProvider::search(self, query, max_results).await
    }

    async fn fetch(&self, url: &str) -> Result<String> {
        FirecrawlProvider::scrape(self, url).await
    }

    async fn map(&self, url: &str, max_results: usize) -> Result<Vec<Source>> {
        FirecrawlProvider::search(self, url, max_results).await
    }
}

#[derive(Clone)]
pub struct SearchService {
    config: Config,
    ai: Arc<dyn AiProvider>,
    /// Model name written into every `SearchRequest` produced by the service.
    /// Resolved once from `config` at construction so each transport gets the
    /// model it actually understands: `grok_model` for Responses, and
    /// `openai_compatible_model` (falling back to `grok_model`) for the
    /// chat-completions transport. Per-call overrides via `WebSearchInput.model`
    /// still win.
    default_model: String,
    sources: Option<Arc<dyn SourceProvider>>,
    fallback_sources: Option<Arc<dyn SourceProvider>>,
    cache: Arc<Mutex<SourceCache>>,
    /// Shared reqwest client for the sources pipeline (same instance handed to
    /// providers). Stored here because resolve_content needs direct GET access.
    http_client: reqwest::Client,
    /// Specialist extractor router. Empty in Phase 1. Behind `Arc` so
    /// `SearchService: Clone` still holds (the router is not `Clone`).
    source_router: Arc<crate::sources::SourceRouter>,
}

impl SearchService {
    pub fn new(config: Config) -> Result<Self> {
        use crate::config::Transport;
        let http = crate::providers::http::build_client(config.timeout);

        let ai: Arc<dyn AiProvider> = match config.transport {
            Transport::Responses => {
                let credential: Arc<dyn crate::credentials::CredentialProvider> =
                    match config.grok_auth_mode {
                        AuthMode::ApiKey => Arc::new(StaticApiKeyCredential::new(
                            config
                                .grok_api_key
                                .clone()
                                .ok_or(GrokSearchError::MissingConfig("GROK_SEARCH_API_KEY"))?,
                        )),
                        AuthMode::OAuth => {
                            let auth_path = config
                                .grok_auth_file
                                .clone()
                                .or_else(crate::config::auth_path)
                                .ok_or_else(|| {
                                    GrokSearchError::OAuth(
                                        "oauth_auth_path_unavailable: set GROK_SEARCH_AUTH_FILE"
                                            .to_string(),
                                    )
                                })?;
                            Arc::new(OAuthCredential::new(http.clone(), auth_path))
                        }
                    };
                Arc::new(GrokResponsesProvider::with_credential_client(
                    http.clone(),
                    config.grok_api_url.clone(),
                    credential,
                    config.web_search_enabled,
                    config.x_search_enabled,
                ))
            }
            Transport::ChatCompletions => {
                let url = config
                    .openai_compatible_api_url
                    .clone()
                    .ok_or(GrokSearchError::MissingConfig("OPENAI_COMPATIBLE_API_URL"))?;
                let key = config
                    .openai_compatible_api_key
                    .clone()
                    .ok_or(GrokSearchError::MissingConfig("OPENAI_COMPATIBLE_API_KEY"))?;
                let model = config
                    .openai_compatible_model
                    .clone()
                    .unwrap_or_else(|| config.grok_model.clone());
                if config.x_search_enabled {
                    eprintln!(
                        "grok-search-rs: x_search_enabled is ignored when using OPENAI_COMPATIBLE_* transport"
                    );
                }
                Arc::new(
                    crate::providers::openai_compatible::OpenAICompatProvider::with_client(
                        http.clone(),
                        url,
                        key,
                        model,
                        config.web_search_enabled,
                    ),
                )
            }
        };

        let sources = if config.tavily_enabled {
            config.tavily_api_key.clone().map(|key| {
                Arc::new(TavilyProvider::with_client(
                    http.clone(),
                    config.tavily_api_url.clone(),
                    key,
                )) as Arc<dyn SourceProvider>
            })
        } else {
            None
        };

        let fallback_sources = if config.firecrawl_enabled {
            config.firecrawl_api_key.clone().map(|key| {
                Arc::new(FirecrawlProvider::with_client(
                    http.clone(),
                    config.firecrawl_api_url.clone(),
                    key,
                )) as Arc<dyn SourceProvider>
            })
        } else {
            None
        };

        let source_router = Arc::new(crate::sources::SourceRouter::from_config(&config));
        Ok(Self {
            cache: Arc::new(Mutex::new(SourceCache::new(config.cache_size))),
            default_model: resolve_default_model(&config),
            config,
            ai,
            sources,
            fallback_sources,
            http_client: http.clone(),
            source_router,
        })
    }

    pub fn fake_with_sources() -> Self {
        let config = Config::from_env_map([
            ("GROK_SEARCH_API_KEY", "fake-grok"),
            ("TAVILY_API_KEY", "fake-tavily"),
        ]);
        Self {
            cache: Arc::new(Mutex::new(SourceCache::new(256))),
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: Some(Arc::new(FakeSourceProvider)),
            fallback_sources: None,
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        }
    }

    /// Unified test factory: override AI / primary / fallback providers and
    /// inject extra env vars. Use `fake_with_sources()` for the trivial case.
    pub fn fake_custom<I, K, V>(
        ai: Option<Arc<dyn AiProvider>>,
        primary: Arc<dyn SourceProvider>,
        fallback: Option<Arc<dyn SourceProvider>>,
        overrides: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        let mut vars = vec![
            ("GROK_SEARCH_API_KEY".to_string(), "fake-grok".to_string()),
            ("TAVILY_API_KEY".to_string(), "fake-tavily".to_string()),
        ];
        if fallback.is_some() {
            vars.push((
                "FIRECRAWL_API_KEY".to_string(),
                "fake-firecrawl".to_string(),
            ));
        }
        vars.extend(
            overrides
                .into_iter()
                .map(|(key, value)| (key.into(), value.into())),
        );
        let config = Config::from_env_map(vars);

        Self {
            cache: Arc::new(Mutex::new(SourceCache::new(256))),
            default_model: resolve_default_model(&config),
            config,
            ai: ai.unwrap_or_else(|| Arc::new(FakeAiProvider)),
            sources: Some(primary),
            fallback_sources: fallback,
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        }
    }

    /// Test factory that injects a populated [`crate::sources::SourceRouter`] so
    /// fallback behavior can be exercised with fake extractors. Mirrors
    /// `fake_custom`'s provider wiring.
    pub fn fake_with_router(
        primary: Arc<dyn SourceProvider>,
        fallback: Option<Arc<dyn SourceProvider>>,
        router: crate::sources::SourceRouter,
    ) -> Self {
        let mut vars = vec![
            ("GROK_SEARCH_API_KEY".to_string(), "fake-grok".to_string()),
            ("TAVILY_API_KEY".to_string(), "fake-tavily".to_string()),
        ];
        if fallback.is_some() {
            vars.push((
                "FIRECRAWL_API_KEY".to_string(),
                "fake-firecrawl".to_string(),
            ));
        }
        let config = Config::from_env_map(vars);
        Self {
            cache: Arc::new(Mutex::new(SourceCache::new(256))),
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: Some(primary),
            fallback_sources: fallback,
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(router),
        }
    }

    pub async fn web_search(&self, input: WebSearchInput) -> Result<WebSearchOutput> {
        // D-02: single global deadline shared by Grok + supplemental fetch + enrichment.
        let deadline = tokio::time::Instant::now() + self.config.timeout;
        let include_content = input.include_content.unwrap_or(true);

        let mut uuid_buf = [0u8; uuid::fmt::Simple::LENGTH];
        let session_id = {
            let encoded = Uuid::new_v4().simple().encode_lower(&mut uuid_buf);
            encoded[..12].to_string()
        };
        let effective_extra_sources = input
            .extra_sources
            .unwrap_or(self.config.default_extra_sources);

        let filters = SearchFilters {
            recency_days: input.recency_days,
            include_domains: input.include_domains.clone(),
            exclude_domains: input.exclude_domains.clone(),
        };

        // Speculative fan-out: fetch enough sources to satisfy whichever path
        // (enrichment or fallback) the Grok response routes us into. The
        // speculative call fires concurrently with Grok via tokio::join!, so
        // total latency is roughly max(Grok, Tavily) instead of the sum. The
        // single source call is then sliced to either `effective_extra_sources`
        // (enrichment) or `self.config.fallback_sources` (fallback), preserving
        // the legacy "exactly one source provider call per web_search" contract.
        let speculative_count = effective_extra_sources.max(self.config.fallback_sources);
        let request = self.build_search_request(&input, &[]);

        let grok_future = self.ai.search(&request);
        let speculative_future =
            self.fetch_raw_extra_sources(&input.query, speculative_count, &filters);
        let (grok_result, (raw_sources, raw_origin)) =
            tokio::join!(grok_future, speculative_future);

        let response = match grok_result {
            Ok(response) => response,
            Err(_) => {
                return self
                    .finalize_fallback(
                        deadline,
                        session_id,
                        SearchResponse {
                            content: String::new(),
                            sources: Vec::new(),
                        },
                        raw_sources,
                        raw_origin,
                        "grok_provider_error",
                    )
                    .await;
            }
        };

        if let Some(reason) = grok_unverifiable_reason(&response) {
            return self
                .finalize_fallback(
                    deadline,
                    session_id,
                    response,
                    raw_sources,
                    raw_origin,
                    reason,
                )
                .await;
        }

        let mut enrichment = raw_sources;
        enrichment.truncate(effective_extra_sources);
        let enrichment = with_provider(enrichment, enrichment_label(raw_origin));
        let merged = merge_sources(response.sources, enrichment);
        // SRCH-04 dual gate (zero-regression): skip enrichment when the caller
        // opted out OR there are no supplemental sources. Gating on
        // include_content alone would leave content populated at extra_sources=0
        // and break the legacy "summary + source list" shape.
        let merged = if include_content && effective_extra_sources > 0 {
            enrich_sources(
                merged,
                deadline,
                &self.http_client,
                &self.source_router,
                crate::sources::SourceCaps {
                    max_answers: self.config.source_max_answers,
                    max_comments: self.config.source_max_comments,
                },
                self.config.enrich_concurrency,
                self.config.enrich_max_chars,
            )
            .await
        } else {
            merged
        };

        let merged_arc = Arc::new(merged);
        let sources_count = merged_arc.len();
        self.cache
            .lock()
            .await
            .set(session_id.clone(), merged_arc.clone());

        Ok(WebSearchOutput {
            session_id,
            content: response.content,
            sources_count,
            sources: Arc::try_unwrap(merged_arc).unwrap_or_else(|arc| (*arc).clone()),
            search_provider: "grok_responses".to_string(),
            fallback_used: false,
            fallback_reason: None,
        })
    }

    /// Fetch sources from the primary source provider (or fall through to
    /// firecrawl) without applying a path-specific provider label. The
    /// returned Vec carries each provider's native label ("tavily"/"firecrawl");
    /// the caller re-labels via `with_provider` once the path (enrichment vs
    /// fallback) is known.
    async fn fetch_raw_extra_sources(
        &self,
        query: &str,
        count: usize,
        filters: &SearchFilters,
    ) -> (Vec<Source>, RawSourceOrigin) {
        if count == 0 {
            return (Vec::new(), RawSourceOrigin::None);
        }
        if let Some(provider) = &self.sources {
            if let Ok(sources) = provider.search_sources(query, count, filters).await {
                if !sources.is_empty() {
                    return (sources, RawSourceOrigin::Primary);
                }
            }
        }
        if let Some(provider) = &self.fallback_sources {
            if let Ok(sources) = provider.search_sources(query, count, filters).await {
                if !sources.is_empty() {
                    return (sources, RawSourceOrigin::Fallback);
                }
            }
        }
        (Vec::new(), RawSourceOrigin::None)
    }

    async fn finalize_fallback(
        &self,
        deadline: tokio::time::Instant,
        session_id: String,
        response: SearchResponse,
        raw_sources: Vec<Source>,
        raw_origin: RawSourceOrigin,
        reason: &str,
    ) -> Result<WebSearchOutput> {
        let mut fallback = raw_sources;
        fallback.truncate(self.config.fallback_sources);
        let fallback = with_provider(fallback, fallback_label(raw_origin));

        // D-03: the degraded path enriches unconditionally — one-hand evidence
        // is most valuable when there is no verifiable summary. No extra_sources
        // gate here (that gate is the normal web_search path's concern, SRCH-04).
        let fallback = enrich_sources(
            fallback,
            deadline,
            &self.http_client,
            &self.source_router,
            crate::sources::SourceCaps {
                max_answers: self.config.source_max_answers,
                max_comments: self.config.source_max_comments,
            },
            self.config.enrich_concurrency,
            self.config.enrich_max_chars,
        )
        .await;

        let fallback_arc = Arc::new(fallback);
        let sources_count = fallback_arc.len();
        self.cache
            .lock()
            .await
            .set(session_id.clone(), fallback_arc.clone());

        let content = if response.content.trim().is_empty() {
            format!(
                "Grok Responses search did not return a verifiable answer. Source fallback returned {sources_count} source(s); evaluate them directly rather than treating any text as a verified answer."
            )
        } else {
            format!(
                "Grok Responses returned an answer without verifiable search sources, so source fallback returned {sources_count} source(s). Original Grok answer was not treated as verified; evaluate the listed sources directly."
            )
        };

        Ok(WebSearchOutput {
            session_id,
            content,
            sources_count,
            sources: Arc::try_unwrap(fallback_arc).unwrap_or_else(|arc| (*arc).clone()),
            search_provider: "source_fallback".to_string(),
            fallback_used: true,
            fallback_reason: Some(reason.to_string()),
        })
    }

    pub async fn get_sources(&self, session_id: &str) -> Result<GetSourcesOutput> {
        let sources = self
            .cache
            .lock()
            .await
            .get(session_id)
            .ok_or_else(|| GrokSearchError::NotFound(format!("session_id={session_id}")))?;
        let sources_count = sources.len();
        Ok(GetSourcesOutput {
            session_id: session_id.to_string(),
            sources_count,
            sources: Arc::try_unwrap(sources).unwrap_or_else(|arc| (*arc).clone()),
        })
    }

    pub async fn web_fetch(&self, url: &str, max_chars: Option<usize>) -> Result<WebFetchOutput> {
        let effective_limit = max_chars.or(self.config.fetch_max_chars);

        let (content, source_type, fallback_reason) = match url::Url::parse(url) {
            Ok(parsed) => {
                match crate::sources::resolve_content(
                    &self.http_client,
                    &parsed,
                    self.source_router.as_ref(),
                    &crate::sources::SourceCaps {
                        max_answers: self.config.source_max_answers,
                        max_comments: self.config.source_max_comments,
                    },
                )
                .await
                {
                    // Specialist succeeded — keep its content and source type.
                    Ok((content, kind)) => (content, kind, None),
                    // No specialist matched: go generic silently (D-01).
                    Err(reason) if reason == crate::sources::NO_SPECIALIST_MATCH => {
                        let generic = self.web_fetch_raw(url).await?;
                        (generic, crate::sources::SourceType::Generic, None)
                    }
                    // Specialist matched but failed/empty: surface the reason (D-01).
                    Err(reason) => {
                        let generic = self.web_fetch_raw(url).await?;
                        (generic, crate::sources::SourceType::Generic, Some(reason))
                    }
                }
            }
            // Malformed URL is not a specialist failure — go generic, no reason.
            Err(_) => {
                let generic = self.web_fetch_raw(url).await?;
                (generic, crate::sources::SourceType::Generic, None)
            }
        };

        Ok(apply_fetch_limit(
            url,
            content,
            effective_limit,
            source_type,
            fallback_reason,
        ))
    }

    async fn web_fetch_raw(&self, url: &str) -> Result<String> {
        if let Some(provider) = &self.sources {
            if let Ok(content) = provider.fetch(url).await {
                if !content.trim().is_empty() {
                    return Ok(content);
                }
            }
        }

        if let Some(provider) = &self.fallback_sources {
            return provider.fetch(url).await;
        }

        Err(GrokSearchError::MissingConfig(
            "TAVILY_API_KEY or FIRECRAWL_API_KEY",
        ))
    }

    pub async fn web_map(&self, url: &str, max_results: usize) -> Result<Vec<Source>> {
        self.sources
            .as_ref()
            .ok_or(GrokSearchError::MissingConfig("TAVILY_API_KEY"))?
            .map(url, max_results)
            .await
    }

    /// Runtime diagnostics with live connectivity probes against each configured backend.
    /// Returns provider availability flags, masked config, and per-provider reachability.
    pub async fn doctor(&self) -> serde_json::Value {
        use crate::config::Transport;
        let grok_probe = self.probe_grok().await;
        let tavily_probe = match &self.sources {
            Some(provider) => probe_source(provider.as_ref(), "https://example.com").await,
            None => Probe::skipped("TAVILY_API_KEY not configured"),
        };
        let firecrawl_probe = match &self.fallback_sources {
            Some(provider) => probe_source(provider.as_ref(), "https://example.com").await,
            None => Probe::skipped("FIRECRAWL_API_KEY not configured"),
        };

        // Surface the AI transport that the service actually dispatches to so
        // doctor() stays truthful when callers point us at an OpenAI-compatible
        // gateway. The legacy "grok" node name is preserved for backward
        // compatibility, but its fields are now sourced from `default_model`
        // and the transport-appropriate API URL — never silently from
        // `grok_model` / `grok_api_url` on the chat-completions path.
        let (provider_label, ai_api_url, ai_x_search_enabled) = match self.config.transport {
            Transport::Responses => (
                "grok_responses",
                self.config.grok_api_url.as_str(),
                self.config.x_search_enabled,
            ),
            Transport::ChatCompletions => (
                "openai_compatible",
                self.config
                    .openai_compatible_api_url
                    .as_deref()
                    .unwrap_or(""),
                // x_search is silently ignored on the chat-completions transport
                // (the gateway has no equivalent); report it as disabled rather
                // than leaking a misleading config flag.
                false,
            ),
        };

        serde_json::json!({
            "provider": provider_label,
            "transport": provider_label,
            "grok": {
                "api_url": ai_api_url,
                "model": self.default_model,
                "auth_mode": match self.config.grok_auth_mode {
                    AuthMode::ApiKey => "api_key",
                    AuthMode::OAuth => "oauth",
                },
                "auth_file": self.config
                    .grok_auth_file
                    .clone()
                    .or_else(crate::config::auth_path)
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| "unavailable".to_string()),
                "web_search_enabled": self.config.web_search_enabled,
                "x_search_enabled": ai_x_search_enabled,
                "reachable": grok_probe.ok,
                "detail": grok_probe.detail,
            },
            "tavily": {
                "api_url": self.config.tavily_api_url,
                "enabled": self.config.tavily_enabled,
                "reachable": tavily_probe.ok,
                "detail": tavily_probe.detail,
            },
            "firecrawl": {
                "api_url": self.config.firecrawl_api_url,
                "enabled": self.config.firecrawl_enabled,
                "reachable": firecrawl_probe.ok,
                "detail": firecrawl_probe.detail,
            },
            "default_extra_sources": self.config.default_extra_sources,
            "fallback_sources": self.config.fallback_sources,
            "cache_size": self.config.cache_size,
            "timeout_seconds": self.config.timeout.as_secs(),
            "github_token": self.config.github_token_status(),
            "redacted": self.config.redacted_diagnostics()
        })
    }

    async fn probe_grok(&self) -> Probe {
        // Mirror the real search shape so the probe doesn't fail the
        // adapter's "web_search tool intent" pre-check.
        let mut tools = Vec::new();
        if self.config.web_search_enabled {
            tools.push(SearchTool::web_search());
        }
        let request = SearchRequest {
            model: self.default_model.clone(),
            system: None,
            messages: vec![SearchMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::text("ping")],
            }],
            tools,
        };
        match self.ai.search(&request).await {
            Ok(_) => Probe::ok("grok responded"),
            Err(err) => Probe::failed(err.to_string()),
        }
    }

    fn build_search_request(
        &self,
        input: &WebSearchInput,
        extra_sources: &[Source],
    ) -> SearchRequest {
        let mut content = input.query.clone();
        if let Some(platform) = input.platform.as_deref().filter(|value| !value.is_empty()) {
            content.push_str("\n\nFocus platform: ");
            content.push_str(platform);
        }
        if let Some(days) = input.recency_days {
            content.push_str(&format!(
                "\n\nRestrict evidence to sources published within the last {days} day(s)."
            ));
        }
        if !input.include_domains.is_empty() {
            content.push_str("\n\nPrefer sources from: ");
            content.push_str(&input.include_domains.join(", "));
        }
        if !input.exclude_domains.is_empty() {
            content.push_str("\n\nDo not cite sources from: ");
            content.push_str(&input.exclude_domains.join(", "));
        }
        if !extra_sources.is_empty() {
            content.push_str("\n\nAdditional sources:\n");
            for source in extra_sources {
                content.push_str("- ");
                content.push_str(&source.url);
                if let Some(title) = &source.title {
                    content.push_str(" | ");
                    content.push_str(title);
                }
                content.push('\n');
            }
        }

        SearchRequest {
            model: input
                .model
                .clone()
                .unwrap_or_else(|| self.default_model.clone()),
            system: Some("Answer concisely with factual claims grounded in web search sources. Prefer primary sources. If sources are weak or unavailable, say so.".to_string()),
            messages: vec![SearchMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::text(content)],
            }],
            tools: vec![SearchTool::web_search()],
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum RawSourceOrigin {
    None,
    Primary,
    Fallback,
}

/// Pick the model the active transport actually understands. Responses speaks
/// Grok-native model names (`grok_model`); the chat-completions gateway speaks
/// whatever `OPENAI_COMPATIBLE_MODEL` declares, falling back to `grok_model`
/// only when the operator hasn't set one. Resolved once at service
/// construction so every outgoing `SearchRequest` carries the right default
/// — preventing the chat path from silently shipping a Grok-only ID.
fn resolve_default_model(config: &Config) -> String {
    use crate::config::Transport;
    match config.transport {
        Transport::Responses => config.grok_model.clone(),
        Transport::ChatCompletions => config
            .openai_compatible_model
            .clone()
            .unwrap_or_else(|| config.grok_model.clone()),
    }
}

fn enrichment_label(origin: RawSourceOrigin) -> &'static str {
    match origin {
        RawSourceOrigin::Primary => "tavily_enrichment",
        RawSourceOrigin::Fallback => "firecrawl_enrichment",
        RawSourceOrigin::None => "tavily_enrichment",
    }
}

fn fallback_label(origin: RawSourceOrigin) -> &'static str {
    match origin {
        RawSourceOrigin::Primary => "tavily_fallback",
        RawSourceOrigin::Fallback => "firecrawl_enrichment",
        RawSourceOrigin::None => "tavily_fallback",
    }
}

fn grok_unverifiable_reason(response: &SearchResponse) -> Option<&'static str> {
    if response.content.trim().is_empty() {
        return Some("grok_content_empty");
    }
    if response.sources.is_empty() {
        return Some("grok_sources_empty");
    }
    None
}

fn apply_fetch_limit(
    url: &str,
    mut content: String,
    max_chars: Option<usize>,
    source_type: crate::sources::SourceType,
    fallback_reason: Option<String>,
) -> WebFetchOutput {
    let Some(limit) = max_chars else {
        let original_length = content.chars().count();
        return WebFetchOutput {
            url: url.to_string(),
            content,
            original_length,
            truncated: false,
            source_type,
            fallback_reason,
        };
    };

    let mut count = 0usize;
    let mut cutoff: Option<usize> = None;
    for (byte_idx, _) in content.char_indices() {
        if count == limit {
            cutoff = Some(byte_idx);
            break;
        }
        count += 1;
    }

    match cutoff {
        Some(byte_idx) => {
            let extra = content[byte_idx..].chars().count();
            content.truncate(byte_idx);
            WebFetchOutput {
                url: url.to_string(),
                content,
                original_length: limit + extra,
                truncated: true,
                source_type,
                fallback_reason,
            }
        }
        None => WebFetchOutput {
            url: url.to_string(),
            content,
            original_length: count,
            truncated: false,
            source_type,
            fallback_reason,
        },
    }
}

/// Concurrently back-fill `Source.content` for every source via the Phase 1
/// `resolve_content` pipeline. Bounded by `concurrency` (Semaphore) and the
/// shared `deadline` (D-02: per-source `timeout_at`, not an independent budget).
/// Every source ends with `content = Some(..)` — real markdown (truncated to
/// `max_chars`) on success, or a deterministic `_Failed to retrieve: ..._` note
/// on any failure/timeout/invalid-url (D-05: never None, never empty). Source
/// order is preserved.
async fn enrich_sources(
    sources: Vec<Source>,
    deadline: tokio::time::Instant,
    client: &reqwest::Client,
    router: &Arc<crate::sources::SourceRouter>,
    caps: crate::sources::SourceCaps,
    concurrency: usize,
    max_chars: usize,
) -> Vec<Source> {
    let sem = Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set: tokio::task::JoinSet<(usize, Option<String>)> = tokio::task::JoinSet::new();

    for (idx, source) in sources.iter().enumerate() {
        let permit = Arc::clone(&sem);
        let url_str = source.url.clone();
        let client = client.clone();
        let router = Arc::clone(router);
        let caps = caps.clone();

        set.spawn(async move {
            // acquire is micro-second scale for concurrency<=5; deadline
            // enforcement applies to the resolve_content call itself.
            let _permit = permit.acquire_owned().await.ok();
            let content = match url::Url::parse(&url_str) {
                Err(_) => Some(format!(
                    "_Failed to retrieve: invalid_url_\n\nSource: {url_str}"
                )),
                Ok(parsed) => {
                    let future = crate::sources::resolve_content(&client, &parsed, &router, &caps);
                    match tokio::time::timeout_at(deadline, future).await {
                        Ok(Ok((md, _kind))) => {
                            let truncated: String = md.chars().take(max_chars).collect();
                            Some(truncated)
                        }
                        Ok(Err(reason)) => Some(format!(
                            "_Failed to retrieve: {reason}_\n\nSource: {url_str}"
                        )),
                        Err(_elapsed) => Some(format!(
                            "_Failed to retrieve: timeout_\n\nSource: {url_str}"
                        )),
                    }
                }
            };
            (idx, content)
        });
    }

    let mut results: Vec<(usize, Option<String>)> = Vec::with_capacity(sources.len());
    while let Some(res) = set.join_next().await {
        if let Ok(pair) = res {
            results.push(pair);
        }
    }

    results.sort_by_key(|(idx, _)| *idx);
    let mut out = sources;
    for (idx, content) in results {
        out[idx].content = content;
    }
    out
}

fn with_provider(
    mut sources: Vec<Source>,
    provider: impl Into<std::borrow::Cow<'static, str>>,
) -> Vec<Source> {
    let provider = provider.into();
    for source in &mut sources {
        source.provider = provider.clone();
    }
    sources
}

struct Probe {
    ok: bool,
    detail: String,
}

impl Probe {
    fn ok(detail: impl Into<String>) -> Self {
        Self {
            ok: true,
            detail: detail.into(),
        }
    }
    fn failed(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
    fn skipped(detail: impl Into<String>) -> Self {
        Self {
            ok: false,
            detail: detail.into(),
        }
    }
}

async fn probe_source(provider: &dyn SourceProvider, sample_url: &str) -> Probe {
    // Use a short keyword search as a lightweight liveness signal.
    let filters = SearchFilters::default();
    match provider.search_sources("ping", 1, &filters).await {
        Ok(_) => Probe::ok(format!("reachable (sample probe via {sample_url} ok)")),
        Err(err) => Probe::failed(err.to_string()),
    }
}

struct FakeAiProvider;

#[async_trait]
impl AiProvider for FakeAiProvider {
    async fn search(&self, _request: &SearchRequest) -> Result<SearchResponse> {
        Ok(SearchResponse {
            content: "OpenAI published a verifiable update.".to_string(),
            sources: vec![
                Source::new("https://openai.com/news", "grok_responses").with_title("OpenAI News")
            ],
        })
    }
}

struct FakeSourceProvider;

#[async_trait]
impl SourceProvider for FakeSourceProvider {
    async fn search_sources(
        &self,
        _query: &str,
        max_results: usize,
        _filters: &SearchFilters,
    ) -> Result<Vec<Source>> {
        Ok((0..max_results)
            .map(|idx| {
                Source::new(format!("https://example.com/source-{idx}"), "tavily")
                    .with_title(format!("Source {idx}"))
            })
            .collect())
    }

    async fn fetch(&self, url: &str) -> Result<String> {
        Ok(format!("Fetched content from {url}"))
    }

    async fn map(&self, url: &str, max_results: usize) -> Result<Vec<Source>> {
        Ok((0..max_results)
            .map(|idx| Source::new(format!("{url}/page-{idx}"), "tavily"))
            .collect())
    }
}

#[cfg(test)]
mod transport_dispatch_tests {
    use super::*;
    use crate::config::Transport;

    #[test]
    fn service_constructs_for_chat_completions_transport() {
        let config = Config::from_env_map([
            ("OPENAI_COMPATIBLE_API_URL", "https://example.com/v1"),
            ("OPENAI_COMPATIBLE_API_KEY", "sk-fake"),
            ("OPENAI_COMPATIBLE_MODEL", "grok-4.3-fast"),
            ("TAVILY_API_KEY", "fake-tavily"),
        ]);
        assert_eq!(config.transport, Transport::ChatCompletions);
        let svc = SearchService::new(config).expect("service should build");
        // Smoke: just ensure construction doesn't blow up. The actual provider
        // type is hidden behind Arc<dyn AiProvider>; we verify behavior in the
        // ignored e2e probe (Task 7) and adapter unit tests (Tasks 3-4).
        drop(svc);
    }

    #[test]
    fn service_rejects_chat_completions_without_url() {
        let config = Config::from_env_map([("OPENAI_COMPATIBLE_API_KEY", "sk-fake")]);
        // url missing -> falls back to Responses transport, which then needs
        // GROK_SEARCH_API_KEY which is also missing -> MissingConfig.
        assert!(SearchService::new(config).is_err());
    }

    #[test]
    fn default_model_follows_chat_completions_when_compat_model_set() {
        // Reproduces the regression: SearchService::build_search_request used
        // to stamp `grok_model` into every SearchRequest, masking
        // OPENAI_COMPATIBLE_MODEL on the chat-completions transport.
        let config = Config::from_env_map([
            ("OPENAI_COMPATIBLE_API_URL", "https://example.com/v1"),
            ("OPENAI_COMPATIBLE_API_KEY", "sk-fake"),
            ("OPENAI_COMPATIBLE_MODEL", "gpt-4o-mini"),
            ("GROK_SEARCH_MODEL", "grok-4-1-fast-reasoning"),
        ]);
        assert_eq!(config.transport, Transport::ChatCompletions);
        assert_eq!(resolve_default_model(&config), "gpt-4o-mini");
    }

    #[test]
    fn default_model_falls_back_to_grok_model_when_compat_model_missing() {
        let config = Config::from_env_map([
            ("OPENAI_COMPATIBLE_API_URL", "https://example.com/v1"),
            ("OPENAI_COMPATIBLE_API_KEY", "sk-fake"),
            ("GROK_SEARCH_MODEL", "grok-4-1-fast-reasoning"),
        ]);
        assert_eq!(config.transport, Transport::ChatCompletions);
        assert_eq!(resolve_default_model(&config), "grok-4-1-fast-reasoning");
    }

    #[test]
    fn default_model_uses_grok_model_on_responses_transport() {
        let config = Config::from_env_map([
            ("GROK_SEARCH_API_KEY", "xai-fake"),
            ("GROK_SEARCH_MODEL", "grok-4-1-fast-reasoning"),
            ("OPENAI_COMPATIBLE_MODEL", "gpt-4o-mini"),
        ]);
        assert_eq!(config.transport, Transport::Responses);
        assert_eq!(resolve_default_model(&config), "grok-4-1-fast-reasoning");
    }

    #[tokio::test]
    async fn doctor_reports_openai_compatible_transport_fields() {
        // Regression: doctor() used to hardcode "grok_responses" / grok_model /
        // grok_api_url, masking what the service actually dispatches to on the
        // chat-completions transport. Now it must reflect compat config.
        let config = Config::from_env_map([
            ("OPENAI_COMPATIBLE_API_URL", "https://compat.example/v1"),
            ("OPENAI_COMPATIBLE_API_KEY", "sk-fake"),
            ("OPENAI_COMPATIBLE_MODEL", "gpt-4o-mini"),
            ("GROK_SEARCH_MODEL", "grok-4-1-fast-reasoning"),
            // X-search is silently ignored on this transport — doctor must
            // report the effective behavior (false), not the raw env flag.
            ("GROK_SEARCH_X_SEARCH", "true"),
        ]);
        assert_eq!(config.transport, Transport::ChatCompletions);

        // Hand-build the service with fake AI to avoid any real HTTP from
        // probe_grok during doctor().
        let svc = SearchService {
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: None,
            fallback_sources: None,
            cache: Arc::new(Mutex::new(SourceCache::new(16))),
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        };

        let report = svc.doctor().await;
        assert_eq!(report["provider"], "openai_compatible");
        assert_eq!(report["transport"], "openai_compatible");
        assert_eq!(report["grok"]["api_url"], "https://compat.example/v1");
        assert_eq!(report["grok"]["model"], "gpt-4o-mini");
        assert_eq!(report["grok"]["x_search_enabled"], false);
    }

    #[tokio::test]
    async fn doctor_still_reports_grok_responses_on_responses_transport() {
        let config = Config::from_env_map([
            ("GROK_SEARCH_API_KEY", "xai-fake"),
            ("GROK_SEARCH_MODEL", "grok-4-1-fast-reasoning"),
        ]);
        assert_eq!(config.transport, Transport::Responses);

        let svc = SearchService {
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: None,
            fallback_sources: None,
            cache: Arc::new(Mutex::new(SourceCache::new(16))),
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        };

        let report = svc.doctor().await;
        assert_eq!(report["provider"], "grok_responses");
        assert_eq!(report["grok"]["model"], "grok-4-1-fast-reasoning");
    }

    #[tokio::test]
    async fn doctor_reports_github_token_status() {
        // With GITHUB_TOKEN set -> "set", and the raw value never leaks.
        let config = Config::from_env_map([
            ("GROK_SEARCH_API_KEY", "xai-fake"),
            ("GITHUB_TOKEN", "ghp_test"),
        ]);
        let svc = SearchService {
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: None,
            fallback_sources: None,
            cache: Arc::new(Mutex::new(SourceCache::new(16))),
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        };
        let report = svc.doctor().await;
        assert_eq!(report["github_token"], "set");
        // No-leak: the full report must not contain the token value anywhere.
        assert!(
            !report.to_string().contains("ghp_test"),
            "token value leaked into doctor report: {report}"
        );

        // Without GITHUB_TOKEN -> "unset".
        let config_unset = Config::from_env_map([("GROK_SEARCH_API_KEY", "xai-fake")]);
        let svc_unset = SearchService {
            default_model: resolve_default_model(&config_unset),
            config: config_unset,
            ai: Arc::new(FakeAiProvider),
            sources: None,
            fallback_sources: None,
            cache: Arc::new(Mutex::new(SourceCache::new(16))),
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(crate::sources::SourceRouter::default()),
        };
        let report_unset = svc_unset.doctor().await;
        assert_eq!(report_unset["github_token"], "unset");
    }

    #[tokio::test]
    async fn fake_with_router_constructs_and_clones() {
        let svc = SearchService::fake_with_router(
            Arc::new(FakeSourceProvider),
            None,
            crate::sources::SourceRouter::default(),
        );
        // SearchService derives Clone; storing Arc<SourceRouter> must preserve it.
        let _clone = svc.clone();
    }
}

#[cfg(test)]
mod enrich_tests {
    use super::*;
    use crate::sources::{SourceCaps, SourceExtractor, SourceRouter, SourceType};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;
    use url::Url;

    /// Always-matching extractor that records peak concurrency and returns a
    /// fixed body after a visibility sleep.
    struct CountingExtractor {
        peak: Arc<AtomicUsize>,
        current: Arc<AtomicUsize>,
        sleep_ms: u64,
    }
    #[async_trait]
    impl SourceExtractor for CountingExtractor {
        fn matches(&self, _url: &Url) -> bool {
            true
        }
        fn kind(&self) -> SourceType {
            SourceType::Wikipedia
        }
        async fn fetch_render(
            &self,
            _c: &reqwest::Client,
            _u: &Url,
            _caps: &SourceCaps,
        ) -> Result<String> {
            let n = self.current.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(n, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(self.sleep_ms)).await;
            self.current.fetch_sub(1, Ordering::SeqCst);
            Ok("content".to_string())
        }
    }

    /// URL-discriminating failure extractor: matches ONLY urls containing
    /// `fail_url_marker`, so a router can route one source here and the rest to
    /// CountingExtractor (true fault isolation).
    struct MarkerErrExtractor {
        fail_url_marker: String,
    }
    #[async_trait]
    impl SourceExtractor for MarkerErrExtractor {
        fn matches(&self, url: &Url) -> bool {
            url.as_str().contains(&self.fail_url_marker)
        }
        fn kind(&self) -> SourceType {
            SourceType::GithubIssue
        }
        async fn fetch_render(
            &self,
            _c: &reqwest::Client,
            _u: &Url,
            _caps: &SourceCaps,
        ) -> Result<String> {
            Err(crate::error::GrokSearchError::Provider(
                "always_fails".to_string(),
            ))
        }
    }

    /// Returns an oversized body to exercise the per-source char cap.
    struct OversizeExtractor {
        len: usize,
    }
    #[async_trait]
    impl SourceExtractor for OversizeExtractor {
        fn matches(&self, _url: &Url) -> bool {
            true
        }
        fn kind(&self) -> SourceType {
            SourceType::Wikipedia
        }
        async fn fetch_render(
            &self,
            _c: &reqwest::Client,
            _u: &Url,
            _caps: &SourceCaps,
        ) -> Result<String> {
            Ok("x".repeat(self.len))
        }
    }

    /// Hangs far past any test deadline — used to trigger the timeout note.
    struct HangingExtractor;
    #[async_trait]
    impl SourceExtractor for HangingExtractor {
        fn matches(&self, _url: &Url) -> bool {
            true
        }
        fn kind(&self) -> SourceType {
            SourceType::Wikipedia
        }
        async fn fetch_render(
            &self,
            _c: &reqwest::Client,
            _u: &Url,
            _caps: &SourceCaps,
        ) -> Result<String> {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok("never".to_string())
        }
    }

    /// Build a SearchService with fake AI + fake supplemental provider and a
    /// caller-supplied router/config. Mirrors the doctor_* struct-literal tests.
    fn service_with(config: Config, router: SourceRouter) -> SearchService {
        SearchService {
            default_model: resolve_default_model(&config),
            config,
            ai: Arc::new(FakeAiProvider),
            sources: Some(Arc::new(FakeSourceProvider)),
            fallback_sources: None,
            cache: Arc::new(Mutex::new(SourceCache::new(64))),
            http_client: crate::providers::http::build_client(std::time::Duration::from_secs(30)),
            source_router: Arc::new(router),
        }
    }

    fn enrich_config() -> Config {
        Config::from_env_map([
            ("GROK_SEARCH_API_KEY", "fake-grok"),
            ("TAVILY_API_KEY", "fake-tavily"),
        ])
    }

    fn base_input() -> WebSearchInput {
        WebSearchInput {
            query: "q".to_string(),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn counting_extractor_self_test() {
        // Sanity: the helper itself records concurrency.
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak: Arc::clone(&peak),
            current: Arc::clone(&current),
            sleep_ms: 5,
        })]);
        let svc = service_with(enrich_config(), router);
        let _ = svc.web_search(base_input()).await.expect("web_search");
        assert!(peak.load(Ordering::SeqCst) >= 1);
    }

    #[tokio::test]
    async fn web_search_inline_default_fills_content() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak,
            current,
            sleep_ms: 0,
        })]);
        let svc = service_with(enrich_config(), router);
        let out = svc.web_search(base_input()).await.expect("web_search");

        assert!(!out.sources.is_empty());
        for s in &out.sources {
            let c = s.content.as_deref().unwrap_or("");
            assert!(!c.is_empty(), "every source must have non-empty content");
        }
    }

    #[tokio::test]
    async fn enrich_concurrency_is_bounded() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak: Arc::clone(&peak),
            current: Arc::clone(&current),
            sleep_ms: 25, // wide enough window for overlap to register
        })]);
        let mut config = enrich_config();
        config.enrich_concurrency = 2;
        let svc = service_with(config, router);

        let _ = svc.web_search(base_input()).await.expect("web_search");
        // 4 sources, concurrency 2 → peak must never exceed 2.
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak={}",
            peak.load(Ordering::SeqCst)
        );
    }

    #[tokio::test]
    async fn enrich_truncates_to_max_chars() {
        let router =
            SourceRouter::with_extractors(vec![Box::new(OversizeExtractor { len: 20_000 })]);
        let svc = service_with(enrich_config(), router); // default enrich_max_chars = 15000
        let out = svc.web_search(base_input()).await.expect("web_search");

        for s in &out.sources {
            let len = s.content.as_deref().map(|c| c.chars().count()).unwrap_or(0);
            assert!(len <= 15_000, "content len {len} exceeds cap");
            assert!(len > 0);
        }
    }

    #[tokio::test]
    async fn enrich_fault_isolation_one_fails_rest_ok() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![
            Box::new(MarkerErrExtractor {
                fail_url_marker: "openai.com".to_string(),
            }),
            Box::new(CountingExtractor {
                peak,
                current,
                sleep_ms: 0,
            }),
        ]);
        let svc = service_with(enrich_config(), router);
        let out = svc
            .web_search(base_input())
            .await
            .expect("web_search returns Ok despite one failure");

        let failed = out
            .sources
            .iter()
            .find(|s| s.url.contains("openai.com"))
            .expect("grok source present");
        let passed = out
            .sources
            .iter()
            .find(|s| s.url.contains("example.com"))
            .expect("supplemental source present");

        assert!(
            failed
                .content
                .as_deref()
                .unwrap_or("")
                .starts_with("_Failed to retrieve:"),
            "failing source must carry a failure note, got: {:?}",
            failed.content
        );
        let pc = passed.content.as_deref().unwrap_or("");
        assert!(
            !pc.is_empty() && !pc.starts_with("_Failed to retrieve:"),
            "passing source must carry real content, got: {pc:?}"
        );
    }

    #[tokio::test]
    async fn enrich_timeout_yields_note_not_error() {
        let router = SourceRouter::with_extractors(vec![Box::new(HangingExtractor)]);
        let mut config = enrich_config();
        config.timeout = Duration::from_millis(50); // deadline fires fast
        let svc = service_with(config, router);

        let out = svc
            .web_search(base_input())
            .await
            .expect("web_search returns Ok on timeout");
        for s in &out.sources {
            assert!(
                s.content.as_deref().unwrap_or("").contains("timeout"),
                "expected timeout note, got: {:?}",
                s.content
            );
        }
    }

    #[tokio::test]
    async fn include_content_false_omits_content_field() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak,
            current,
            sleep_ms: 0,
        })]);
        let svc = service_with(enrich_config(), router);

        let mut input = base_input();
        input.include_content = Some(false);
        let out = svc.web_search(input).await.expect("web_search");

        for s in &out.sources {
            assert!(s.content.is_none());
            let value = serde_json::to_value(s).unwrap();
            assert!(
                value.get("content").is_none(),
                "JSON must omit the content key, not emit null"
            );
        }
    }

    #[tokio::test]
    async fn extra_sources_zero_suppresses_inline() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak,
            current,
            sleep_ms: 0,
        })]);
        let svc = service_with(enrich_config(), router);

        let mut input = base_input();
        input.extra_sources = Some(0); // effective_extra_sources == 0 → dual gate suppresses enrich
        let out = svc.web_search(input).await.expect("web_search");

        for s in &out.sources {
            assert!(
                s.content.is_none(),
                "extra_sources=0 must keep the legacy no-content shape"
            );
        }
    }

    #[tokio::test]
    async fn get_sources_inherits_enriched_content() {
        let peak = Arc::new(AtomicUsize::new(0));
        let current = Arc::new(AtomicUsize::new(0));
        let router = SourceRouter::with_extractors(vec![Box::new(CountingExtractor {
            peak,
            current,
            sleep_ms: 0,
        })]);
        let svc = service_with(enrich_config(), router);

        let out = svc.web_search(base_input()).await.expect("web_search");
        let again = svc.get_sources(&out.session_id).await.expect("get_sources");

        assert_eq!(out.sources.len(), again.sources.len());
        for (a, b) in out.sources.iter().zip(again.sources.iter()) {
            assert_eq!(a.url, b.url);
            assert_eq!(
                a.content, b.content,
                "get_sources must reuse the cached enriched content"
            );
        }
    }
}
