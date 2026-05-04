use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::time::Duration;

/// Hard timeout for the WebFetch HTTP request. M6.23 BUG WT1: pre-fix
/// the client had no timeout, so a hanging server / slow DNS would
/// stall the agent indefinitely (and the agent's cancel token wasn't
/// observed at the .send().await boundary). 30s is generous for normal
/// pages while preventing pathological hangs.
const WEB_FETCH_TIMEOUT: Duration = Duration::from_secs(30);

pub struct WebFetchTool {
    client: reqwest::Client,
}

impl WebFetchTool {
    pub fn new() -> Self {
        // M6.23 BUG WT1: explicit timeout. `reqwest::Client::new()` has
        // no default; we add one so a slow upstream can't take down
        // the agent. Falls back to no-timeout client if the builder
        // fails (extremely unlikely; defensive).
        let client = reqwest::Client::builder()
            .timeout(WEB_FETCH_TIMEOUT)
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());
        Self { client }
    }
}

impl Default for WebFetchTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "WebFetch"
    }

    fn description(&self) -> &'static str {
        "Fetch a URL via HTTP GET and return the response body as text. \
         Use for reading web pages, APIs, or downloading text content. \
         Truncates response at `max_bytes` (default 100 KB)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "The URL to fetch"},
                "max_bytes": {"type": "integer", "description": "Max response size in bytes (default 102400)"}
            },
            "required": ["url"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let url = req_str(&input, "url")?;
        let max_bytes = input
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(102_400) as usize;

        let resp = self
            .client
            .get(url)
            .header("user-agent", "thclaws/0.1")
            .send()
            .await
            .map_err(|e| Error::Tool(format!("fetch {url}: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Tool(format!("fetch {url}: HTTP {status}")));
        }

        let text = resp
            .text()
            .await
            .map_err(|e| Error::Tool(format!("read body {url}: {e}")))?;

        if text.len() > max_bytes {
            let mut cut = max_bytes;
            while cut > 0 && !text.is_char_boundary(cut) {
                cut -= 1;
            }
            Ok(format!(
                "{}\n... [truncated at {} bytes, {} total]",
                &text[..cut],
                max_bytes,
                text.len()
            ))
        } else {
            Ok(text)
        }
    }
}
