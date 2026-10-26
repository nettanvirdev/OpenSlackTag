//! `web_search`, backed by Brave Search, Tavily, or a self-hosted SearXNG.

use std::time::Duration;

use anyhow::{Result, bail};
use async_trait::async_trait;
use openslacktag_core::error::{Error, Result as ToolResult};
use openslacktag_core::tool::{PermissionRequest, Tool, ToolContext, ToolOutcome, ToolSource};
use serde_json::{Value, json};

const DEFAULT_RESULTS: u64 = 6;
const MAX_RESULTS: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchProvider {
    Brave,
    Tavily,
    Searxng,
}

#[derive(Debug, Clone)]
pub struct SearchConfig {
    pub provider: SearchProvider,
    pub api_key: Option<String>,
    /// Endpoint override; required for SearXNG (the instance's base URL).
    pub url: Option<String>,
}

impl SearchConfig {
    pub fn new(provider: &str, api_key: Option<String>, url: Option<String>) -> Result<Self> {
        let provider = match provider.to_lowercase().as_str() {
            "brave" => SearchProvider::Brave,
            "tavily" => SearchProvider::Tavily,
            "searxng" => SearchProvider::Searxng,
            other => bail!("SEARCH_PROVIDER must be brave, tavily or searxng, not {other:?}"),
        };
        match provider {
            SearchProvider::Searxng if url.is_none() => {
                bail!("SearXNG needs SEARCH_URL, your instance's address")
            }
            SearchProvider::Brave | SearchProvider::Tavily if api_key.is_none() => {
                bail!("{provider:?} needs SEARCH_API_KEY")
            }
            _ => {}
        }
        Ok(Self {
            provider,
            api_key,
            url,
        })
    }
}

#[derive(Debug)]
pub struct WebSearch {
    config: SearchConfig,
    http: reqwest::Client,
}

impl WebSearch {
    pub fn new(config: SearchConfig) -> Self {
        Self {
            config,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(20))
                .build()
                .unwrap_or_default(),
        }
    }

    /// Results as (title, url, snippet).
    pub async fn search(&self, query: &str, count: u64) -> Result<Vec<(String, String, String)>> {
        let key = self.config.api_key.clone().unwrap_or_default();
        let (body, list, snippet): (Value, &str, &str) = match self.config.provider {
            SearchProvider::Brave => {
                let url = self
                    .config
                    .url
                    .clone()
                    .unwrap_or_else(|| "https://api.search.brave.com/res/v1/web/search".into());
                let body = self
                    .http
                    .get(url)
                    .header("X-Subscription-Token", key)
                    .header("Accept", "application/json")
                    .query(&[("q", query), ("count", &count.to_string())])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                (body, "/web/results", "description")
            }
            SearchProvider::Tavily => {
                let url = self
                    .config
                    .url
                    .clone()
                    .unwrap_or_else(|| "https://api.tavily.com/search".into());
                let body = self
                    .http
                    .post(url)
                    .bearer_auth(key)
                    .json(&json!({"query": query, "max_results": count}))
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                (body, "/results", "content")
            }
            SearchProvider::Searxng => {
                let base = self.config.url.clone().unwrap_or_default();
                let body = self
                    .http
                    .get(format!("{}/search", base.trim_end_matches('/')))
                    .query(&[("q", query), ("format", "json")])
                    .send()
                    .await?
                    .error_for_status()?
                    .json()
                    .await?;
                (body, "/results", "content")
            }
        };
        Ok(body
            .pointer(list)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .take(count as usize)
            .filter_map(|r| {
                Some((
                    r["title"].as_str().unwrap_or_default().to_string(),
                    r["url"].as_str()?.to_string(),
                    r[snippet].as_str().unwrap_or_default().to_string(),
                ))
            })
            .collect())
    }
}

#[async_trait]
impl Tool for WebSearch {
    fn id(&self) -> &str {
        "web_search"
    }

    fn description(&self) -> &str {
        "Search the web. Returns titles, URLs and snippets; read the promising pages with web_fetch before \
         relying on them, and cite them."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "count": {"type": "integer", "minimum": 1, "maximum": MAX_RESULTS}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn source(&self) -> ToolSource {
        ToolSource::Builtin
    }

    fn permission(&self, args: &Value) -> PermissionRequest {
        PermissionRequest::new("web_search", args["query"].as_str().unwrap_or("*"))
    }

    fn render(&self, args: &Value) -> Option<String> {
        args["query"].as_str().map(str::to_string)
    }

    async fn execute(&self, args: Value, _ctx: &ToolContext) -> ToolResult<ToolOutcome> {
        let query = args["query"].as_str().unwrap_or_default().trim();
        if query.is_empty() {
            return Err(Error::InvalidInput("query is empty".into()));
        }
        let count = args["count"]
            .as_u64()
            .unwrap_or(DEFAULT_RESULTS)
            .clamp(1, MAX_RESULTS);
        let results = self
            .search(query, count)
            .await
            .map_err(|e| Error::Network(format!("search failed: {e:#}")))?;
        if results.is_empty() {
            return Ok(ToolOutcome::text("No results."));
        }
        let text = results
            .iter()
            .enumerate()
            .map(|(i, (title, url, snippet))| {
                format!(
                    "{}. {title}\n   {url}\n   {}",
                    i + 1,
                    crate::one_line(snippet, 300)
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        Ok(ToolOutcome::text(text))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use axum::routing::{get, post};
    use axum::{Json, Router};

    async fn serve(app: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn each_provider_parses_its_own_shape() {
        let base = serve(
            Router::new()
                .route(
                    "/brave",
                    get(|| async { Json(json!({"web": {"results": [{"title": "B", "url": "https://b.example", "description": "brave hit"}]}})) }),
                )
                .route(
                    "/tavily",
                    post(|| async { Json(json!({"results": [{"title": "T", "url": "https://t.example", "content": "tavily hit"}]})) }),
                )
                .route(
                    "/search",
                    get(|| async { Json(json!({"results": [{"title": "S", "url": "https://s.example", "content": "searx hit"}, {"title": "no url"}]})) }),
                ),
        )
        .await;
        let brave = WebSearch::new(
            SearchConfig::new("brave", Some("k".into()), Some(format!("{base}/brave"))).unwrap(),
        );
        assert_eq!(brave.search("q", 5).await.unwrap()[0].2, "brave hit");
        let tavily = WebSearch::new(
            SearchConfig::new("tavily", Some("k".into()), Some(format!("{base}/tavily"))).unwrap(),
        );
        assert_eq!(
            tavily.search("q", 5).await.unwrap()[0].1,
            "https://t.example"
        );
        let searx = WebSearch::new(SearchConfig::new("searxng", None, Some(base)).unwrap());
        let hits = searx.search("q", 5).await.unwrap();
        assert_eq!(hits.len(), 1, "results without a URL are dropped");
        assert_eq!(hits[0].0, "S");
    }

    #[test]
    fn config_needs_what_each_provider_needs() {
        assert!(SearchConfig::new("brave", None, None).is_err());
        assert!(SearchConfig::new("searxng", None, None).is_err());
        assert!(SearchConfig::new("google", Some("k".into()), None).is_err());
        assert!(SearchConfig::new("Tavily", Some("k".into()), None).is_ok());
    }
}
