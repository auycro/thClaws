//! WebSearch — multi-backend web search tool.
//!
//! Auto-selects the best available backend from env vars:
//!   1. Tavily (`TAVILY_API_KEY`) — clean JSON, best quality
//!   2. Brave Search (`BRAVE_SEARCH_API_KEY`) — clean JSON, good quality
//!   3. DuckDuckGo HTML scrape — no key needed, good enough fallback

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// Hard timeout for WebSearch backends. M6.23 BUG WT1: same root cause
/// as WebFetch — pre-fix `reqwest::Client::new()` had no timeout. 30s
/// is enough for the slowest healthy search backend (DDG HTML scrape
/// can be slow); pathological hangs cap out instead of stalling the
/// agent indefinitely.
const SEARCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebSearchTool {
    client: reqwest::Client,
    engine: String,
}

impl WebSearchTool {
    /// `engine`: `"auto"` (detect from env), `"tavily"`, `"brave"`, `"duckduckgo"`/`"ddg"`.
    pub fn new(engine: &str) -> Self {
        // M6.23 BUG WT1: explicit timeout on the shared client; all
        // three backends (Tavily/Brave/DDG) inherit it.
        let client = reqwest::Client::builder()
            .timeout(SEARCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self {
            client,
            engine: engine.to_string(),
        }
    }

    /// Returns `Some(key)` if `backend` is selected (by config or auto-detect)
    /// AND the env var is set. Returns `None` to signal "try next backend".
    fn try_key(&self, backend: &str, env_var: &str) -> Option<String> {
        let dominated = match self.engine.as_str() {
            // Explicit config: only try the one named backend.
            "tavily" => backend == "tavily",
            "brave" => backend == "brave",
            "duckduckgo" | "ddg" => return None, // forced DDG, skip keyed backends
            // Auto: try in priority order (caller loops tavily → brave → ddg).
            _ => true,
        };
        if !dominated {
            return None;
        }
        std::env::var(env_var).ok()
    }

    async fn search_tavily(&self, query: &str, max: usize, key: &str) -> Result<String> {
        let body = json!({
            "api_key": key,
            "query": query,
            "max_results": max,
            "include_answer": true,
        });
        let resp = self
            .client
            .post("https://api.tavily.com/search")
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::Tool(format!("tavily: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!("tavily HTTP {status}: {text}")));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("tavily json: {e}")))?;

        let mut parts: Vec<String> = Vec::new();

        if let Some(answer) = v.get("answer").and_then(Value::as_str) {
            if !answer.is_empty() {
                parts.push(format!("Answer: {answer}"));
            }
        }

        if let Some(results) = v.get("results").and_then(Value::as_array) {
            for (i, r) in results.iter().take(max).enumerate() {
                let title = r.get("title").and_then(Value::as_str).unwrap_or("");
                let url = r.get("url").and_then(Value::as_str).unwrap_or("");
                let content = r.get("content").and_then(Value::as_str).unwrap_or("");
                parts.push(format!("{}. {} ({})\n   {}", i + 1, title, url, content));
            }
        }

        if parts.is_empty() {
            Ok("No results found.".into())
        } else {
            Ok(parts.join("\n\n"))
        }
    }

    async fn search_brave(&self, query: &str, max: usize, key: &str) -> Result<String> {
        let resp = self
            .client
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", query), ("count", &max.to_string())])
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .map_err(|e| Error::Tool(format!("brave: {e}")))?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            return Err(Error::Tool(format!("brave HTTP {status}: {text}")));
        }

        let v: Value = resp
            .json()
            .await
            .map_err(|e| Error::Tool(format!("brave json: {e}")))?;

        let results = v.pointer("/web/results").and_then(Value::as_array);

        match results {
            Some(arr) if !arr.is_empty() => {
                let lines: Vec<String> = arr
                    .iter()
                    .take(max)
                    .enumerate()
                    .map(|(i, r)| {
                        let title = r.get("title").and_then(Value::as_str).unwrap_or("");
                        let url = r.get("url").and_then(Value::as_str).unwrap_or("");
                        let desc = r.get("description").and_then(Value::as_str).unwrap_or("");
                        format!("{}. {} ({})\n   {}", i + 1, title, url, desc)
                    })
                    .collect();
                Ok(lines.join("\n\n"))
            }
            _ => Ok("No results found.".into()),
        }
    }

    async fn search_ddg(&self, query: &str, max: usize) -> Result<String> {
        let resp = self
            .client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .header("user-agent", "thclaws/0.1")
            .send()
            .await
            .map_err(|e| Error::Tool(format!("duckduckgo: {e}")))?;

        let html = resp
            .text()
            .await
            .map_err(|e| Error::Tool(format!("duckduckgo body: {e}")))?;

        let link_re =
            regex::Regex::new(r#"class="result__a"[^>]*href="([^"]+)"[^>]*>([^<]+)</a>"#).unwrap();
        let snippet_re = regex::Regex::new(r#"class="result__snippet"[^>]*>([^<]+)"#).unwrap();

        let links: Vec<(String, String)> = link_re
            .captures_iter(&html)
            .take(max)
            .map(|c| (c[1].to_string(), c[2].trim().to_string()))
            .collect();

        let snippets: Vec<String> = snippet_re
            .captures_iter(&html)
            .take(max)
            .map(|c| c[1].trim().to_string())
            .collect();

        if links.is_empty() {
            return Ok("No results found.".into());
        }

        let lines: Vec<String> = links
            .iter()
            .enumerate()
            .map(|(i, (url, title))| {
                let snippet = snippets.get(i).map(String::as_str).unwrap_or("");
                format!("{}. {} ({})\n   {}", i + 1, title, url, snippet)
            })
            .collect();

        Ok(lines.join("\n\n"))
    }
}

impl Default for WebSearchTool {
    fn default() -> Self {
        Self::new("auto")
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "WebSearch"
    }

    fn description(&self) -> &'static str {
        "Search the web for information. Auto-selects the best available \
         backend: Tavily (TAVILY_API_KEY), Brave (BRAVE_SEARCH_API_KEY), \
         or DuckDuckGo (no key needed). Returns titles, URLs, and snippets."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "description": "Max results (default 5)"}
            },
            "required": ["query"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let query = req_str(&input, "query")?;
        let max = input
            .get("max_results")
            .and_then(Value::as_u64)
            .unwrap_or(5) as usize;

        // Resolve backend + key. If the configured engine's key is missing,
        // fall back to the next available backend rather than panicking.
        let (backend, result) = if let Some(key) = self.try_key("tavily", "TAVILY_API_KEY") {
            ("tavily", self.search_tavily(query, max, &key).await)
        } else if let Some(key) = self.try_key("brave", "BRAVE_SEARCH_API_KEY") {
            ("brave", self.search_brave(query, max, &key).await)
        } else {
            ("duckduckgo", self.search_ddg(query, max).await)
        };

        result.map(|r| format!("[{backend}] {r}"))
    }
}
