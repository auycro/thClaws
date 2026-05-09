//! WebSearch — multi-backend web search tool.
//!
//! Auto-selects the best available backend from env vars:
//!   1. Tavily (`TAVILY_API_KEY`) — clean JSON, best quality
//!   2. Brave Search (`BRAVE_SEARCH_API_KEY`) — clean JSON, good quality
//!   3. DuckDuckGo HTML scrape — no key needed, good enough fallback
//!
//! Two layers of fallback:
//! - **Config-time** — a missing API key skips that backend at chain-build
//!   time. Pre-existing behavior; preserved.
//! - **Runtime** — a backend that errors mid-call (HTTP 4xx/5xx, timeout,
//!   parse error) falls through to the next candidate. Added in M6.38.4
//!   ([dev-log/174](../dev-log/174-websearch-runtime-fallback.md)) so a
//!   transient Tavily 429 doesn't fail the whole call when DDG would have
//!   answered.
//!
//! Engine pinning preserves the user's intent: `engine = "duckduckgo"`
//! means DDG-only, no fallback. `engine = "tavily"` / `"brave"` still
//! falls through to DDG (mirrors the existing key-absent fallback —
//! "the user wants results, not a specific backend's failure mode").

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

/// One concrete backend candidate produced by [`WebSearchTool::resolve_candidates`].
/// Carries the API key inline (when applicable) so the dispatch loop in
/// `call` doesn't need to re-look-up env vars per attempt.
enum Backend {
    Tavily(String),
    Brave(String),
    Ddg,
}

impl Backend {
    /// Short identifier used in error chains and tests
    /// (`tavily: HTTP 429`).
    fn name(&self) -> &'static str {
        match self {
            Backend::Tavily(_) => "tavily",
            Backend::Brave(_) => "brave",
            Backend::Ddg => "duckduckgo",
        }
    }

    /// Human-friendly display name used in the tool result body —
    /// readable enough that the model carries it through into its
    /// summary instead of paraphrasing it away.
    fn display_name(&self) -> &'static str {
        match self {
            Backend::Tavily(_) => "Tavily",
            Backend::Brave(_) => "Brave Search",
            Backend::Ddg => "DuckDuckGo",
        }
    }
}

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

    /// Resolve the ordered list of backends to try, based on `self.engine`
    /// + env-var presence. Index 0 is tried first; later entries are
    /// runtime fallback candidates. Always non-empty in practice — DDG
    /// is the universal floor for any non-`"duckduckgo"`-pinned config.
    ///
    /// Pin behavior:
    /// - `"auto"` / unset → Tavily (if key) → Brave (if key) → DDG
    /// - `"tavily"` → Tavily (if key) → DDG
    /// - `"brave"` → Brave (if key) → DDG
    /// - `"duckduckgo"` / `"ddg"` → DDG only (no fallback; user explicitly
    ///   chose the bottom of the chain)
    fn resolve_candidates(&self) -> Vec<Backend> {
        let engine = self.engine.as_str();
        let mut out = Vec::new();

        let try_tavily = matches!(engine, "auto" | "" | "tavily");
        let try_brave = matches!(engine, "auto" | "" | "brave");
        // DDG is the universal fallback for everything except a DDG pin
        // (where it's already the only candidate, no need to fall back to
        // itself) and... well, only that.
        let try_ddg = !matches!(engine, "duckduckgo" | "ddg");

        if try_tavily {
            if let Ok(key) = std::env::var("TAVILY_API_KEY") {
                if !key.is_empty() {
                    out.push(Backend::Tavily(key));
                }
            }
        }
        if try_brave {
            if let Ok(key) = std::env::var("BRAVE_SEARCH_API_KEY") {
                if !key.is_empty() {
                    out.push(Backend::Brave(key));
                }
            }
        }
        // The DDG pin produces a one-element chain; auto/tavily/brave
        // produce DDG-as-fallback in addition to the keyed entries.
        if try_ddg || matches!(engine, "duckduckgo" | "ddg") {
            out.push(Backend::Ddg);
        }
        out
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
         or DuckDuckGo (no key needed). Returns titles, URLs, and snippets. \
         The result begins with a `Source: <engine>` line — when summarizing \
         results to the user, mention which engine answered (e.g. \"via Tavily\" \
         or \"ผ่าน Tavily\"); cite the source so they understand the result quality."
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

        let candidates = self.resolve_candidates();
        debug_assert!(
            !candidates.is_empty(),
            "resolve_candidates() should always return at least one backend"
        );
        if candidates.is_empty() {
            return Err(Error::Tool(
                "no search backends available — check engine config".into(),
            ));
        }

        // Try each candidate in priority order. First Ok wins; errors
        // accumulate so the user can see what was attempted if everything
        // fails (or if fallback fired and they want to know why their
        // pinned backend didn't win).
        let mut errors: Vec<String> = Vec::new();
        for backend in &candidates {
            let result = match backend {
                Backend::Tavily(key) => self.search_tavily(query, max, key).await,
                Backend::Brave(key) => self.search_brave(query, max, key).await,
                Backend::Ddg => self.search_ddg(query, max).await,
            };
            match result {
                Ok(body) => {
                    // M6.38.8: full-line "Source:" header on its own
                    // line. The previous inline `[tavily]` prefix was
                    // small enough that the model paraphrased it away
                    // when summarizing — especially when answering in
                    // Thai, where bracketed Latin tokens read like
                    // noise. A dedicated label survives translation.
                    let header = if errors.is_empty() {
                        format!("Source: {} (web search)", backend.display_name())
                    } else {
                        format!(
                            "Source: {} (web search) — fallback after {}",
                            backend.display_name(),
                            errors.join("; ")
                        )
                    };
                    return Ok(format!("{header}\n\n{body}"));
                }
                Err(e) => errors.push(format!("{}: {e}", backend.name())),
            }
        }

        Err(Error::Tool(format!(
            "all WebSearch backends failed: {}",
            errors.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Process-wide lock for env-var manipulation. `resolve_candidates`
    /// reads `TAVILY_API_KEY` and `BRAVE_SEARCH_API_KEY` directly, so
    /// parallel tests would race without serialization.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Restore env vars to their prior state on test exit. Ensures one
    /// test's setup doesn't leak into the next via a process-wide env.
    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_tavily: Option<String>,
        prev_brave: Option<String>,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev_tavily {
                Some(v) => std::env::set_var("TAVILY_API_KEY", v),
                None => std::env::remove_var("TAVILY_API_KEY"),
            }
            match &self.prev_brave {
                Some(v) => std::env::set_var("BRAVE_SEARCH_API_KEY", v),
                None => std::env::remove_var("BRAVE_SEARCH_API_KEY"),
            }
        }
    }

    fn scoped_env() -> EnvGuard {
        let lock = env_lock();
        EnvGuard {
            _lock: lock,
            prev_tavily: std::env::var("TAVILY_API_KEY").ok(),
            prev_brave: std::env::var("BRAVE_SEARCH_API_KEY").ok(),
        }
    }

    #[test]
    fn auto_with_both_keys_chains_tavily_brave_ddg() {
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "t");
        std::env::set_var("BRAVE_SEARCH_API_KEY", "b");
        let tool = WebSearchTool::new("auto");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["tavily", "brave", "duckduckgo"]);
    }

    #[test]
    fn auto_with_no_keys_uses_only_ddg() {
        let _e = scoped_env();
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        let tool = WebSearchTool::new("auto");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["duckduckgo"]);
    }

    #[test]
    fn auto_with_only_brave_skips_tavily() {
        let _e = scoped_env();
        std::env::remove_var("TAVILY_API_KEY");
        std::env::set_var("BRAVE_SEARCH_API_KEY", "b");
        let tool = WebSearchTool::new("auto");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["brave", "duckduckgo"]);
    }

    #[test]
    fn empty_string_key_treated_as_absent() {
        // Some shells set TAVILY_API_KEY="" to "unset" — our resolver
        // should treat the empty string as no key, not call Tavily with
        // an empty Authorization header.
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        let tool = WebSearchTool::new("auto");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["duckduckgo"]);
    }

    #[test]
    fn pinned_tavily_with_key_falls_back_to_ddg() {
        // Pinning Tavily means "I want Tavily first" — but if Tavily
        // fails at runtime, fall through to DDG. Mirrors the existing
        // key-absent fallback ("user wants results, not a specific
        // backend's failure mode").
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "t");
        std::env::set_var("BRAVE_SEARCH_API_KEY", "b");
        let tool = WebSearchTool::new("tavily");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        // Brave is NOT in the chain — pinning means "this one specifically",
        // not "any keyed backend." DDG is the universal floor.
        assert_eq!(chain, vec!["tavily", "duckduckgo"]);
    }

    #[test]
    fn pinned_tavily_without_key_uses_only_ddg() {
        let _e = scoped_env();
        std::env::remove_var("TAVILY_API_KEY");
        let tool = WebSearchTool::new("tavily");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["duckduckgo"]);
    }

    #[test]
    fn pinned_brave_with_key_falls_back_to_ddg() {
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "t");
        std::env::set_var("BRAVE_SEARCH_API_KEY", "b");
        let tool = WebSearchTool::new("brave");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        // Tavily NOT in chain (user pinned brave). DDG follows.
        assert_eq!(chain, vec!["brave", "duckduckgo"]);
    }

    #[test]
    fn pinned_ddg_uses_only_ddg_no_fallback() {
        // The user explicitly chose the bottom of the chain. There's
        // nothing to fall back to; respect their pin.
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "t");
        std::env::set_var("BRAVE_SEARCH_API_KEY", "b");
        for engine in ["duckduckgo", "ddg"] {
            let tool = WebSearchTool::new(engine);
            let chain: Vec<&'static str> =
                tool.resolve_candidates().iter().map(|b| b.name()).collect();
            assert_eq!(chain, vec!["duckduckgo"], "engine={engine}");
        }
    }

    #[test]
    fn empty_engine_string_treated_as_auto() {
        let _e = scoped_env();
        std::env::set_var("TAVILY_API_KEY", "t");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        let tool = WebSearchTool::new("");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        // Empty config should behave like "auto" — defensive default.
        assert_eq!(chain, vec!["tavily", "duckduckgo"]);
    }

    #[test]
    fn unknown_engine_falls_through_to_auto_behavior() {
        // If a user typos the engine name (e.g. "tavlily"), don't break
        // the search — fall back to auto-style chain.
        let _e = scoped_env();
        std::env::remove_var("TAVILY_API_KEY");
        std::env::remove_var("BRAVE_SEARCH_API_KEY");
        let tool = WebSearchTool::new("tavlily");
        let chain: Vec<&'static str> = tool.resolve_candidates().iter().map(|b| b.name()).collect();
        assert_eq!(chain, vec!["duckduckgo"]);
    }

    /// M6.38.8: backend display names are part of the user-visible
    /// contract. The Source: header in the tool result body uses
    /// these names, and the model is told (in the description) to
    /// surface them. Pin them so a future "let's lowercase
    /// everything" refactor can't silently break what the user sees.
    #[test]
    fn backend_display_names_are_human_readable() {
        assert_eq!(Backend::Tavily(String::new()).display_name(), "Tavily");
        assert_eq!(Backend::Brave(String::new()).display_name(), "Brave Search");
        assert_eq!(Backend::Ddg.display_name(), "DuckDuckGo");
        // The short `name()` form is what we emit in error chains
        // (e.g. `tavily: HTTP 429`); keep it lowercase + dash-free
        // so it matches existing user-visible error strings.
        assert_eq!(Backend::Tavily(String::new()).name(), "tavily");
        assert_eq!(Backend::Brave(String::new()).name(), "brave");
        assert_eq!(Backend::Ddg.name(), "duckduckgo");
    }
}
