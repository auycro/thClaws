# Anthropic Messages API provider

`AnthropicProvider` (`providers/anthropic.rs`, 806 LOC) speaks the official Anthropic Messages SSE format. Three `ProviderKind` variants resolve to this single impl with different URL/auth: `Anthropic` (api.anthropic.com), `OllamaAnthropic` (Ollama's `/v1/messages` shim), `AzureAIFoundry` (Azure deployment hosting Anthropic models).

This is the most feature-rich wire format — it's the only one that supports prompt caching (`cache_control: ephemeral`), extended thinking (`thinking.budget_tokens`), and granular content blocks (`tool_use` + `tool_result` + `image` + `text` + `thinking` all first-class). Most other providers normalize *toward* this shape.

**Source:** `crates/core/src/providers/anthropic.rs`
**Constants:**
- `DEFAULT_API_URL = "https://api.anthropic.com/v1/messages"`
- `API_VERSION = "2023-06-01"` — sent as `anthropic-version` header on every request

**Cross-references:**
- [`providers.md`](providers.md) — `Provider` trait, `StreamRequest`, `ProviderEvent`
- [`provider-gateway.md`](provider-gateway.md) — when EE gateway is active, this provider is replaced

---

## 1. Wire format

SSE event flow for a text-only turn:

```
event: message_start
data: {"type":"message_start","message":{"model":"claude-sonnet-4-5",...}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hello"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" world"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":5,"output_tokens":2}}

event: message_stop
data: {"type":"message_stop"}
```

Tool-use turn adds, between `content_block_start(text)` close and `message_delta`:

```
event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_01A","name":"read_file"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"pa"}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"th\":\"/tmp/x\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}
```

Tool input is **streamed** as `partial_json` chunks — assembler concatenates them and parses to JSON at `content_block_stop`. Concatenation can split UTF-8 / quote pairs across chunks; the assembler buffers until close before parsing.

### Event-name → `ProviderEvent` mapping

| SSE `type` | Mapped to |
|---|---|
| `message_start` | `MessageStart { model: message.model }` |
| `content_block_start` w/ `tool_use` | `ToolUseStart { id, name }` |
| `content_block_start` w/ `text` | (no event — implicit) |
| `content_block_delta` w/ `text_delta` | `TextDelta(text)` |
| `content_block_delta` w/ `input_json_delta` | `ToolUseDelta { partial_json }` |
| `content_block_stop` | `ContentBlockStop` |
| `message_delta` | `MessageStop { stop_reason: delta.stop_reason, usage }` |
| `message_stop` | (ignored — `message_delta` already carries the final state) |
| `ping` | (ignored) |
| anything else | (ignored) |

The `content_block_start` for `text` doesn't emit a `ProviderEvent` — the assembler in [`providers.md`](providers.md) opens its `BlockState::Text` lazily on the first `TextDelta` instead.

---

## 2. Struct + builder

```rust
pub struct AnthropicProvider {
    client: Client,                       // reqwest::Client
    api_key: String,
    base_url: String,                     // defaults to DEFAULT_API_URL
    api_key_header: Option<String>,       // None → "x-api-key"
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self;
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self;
    pub fn with_api_key_header(mut self, name: impl Into<String>) -> Self;
}
```

`auth_header_name()` returns `api_key_header.as_deref().unwrap_or("x-api-key")`. Per the doc-comment on `api_key_header`, **Azure AI Foundry also accepts `x-api-key`** so the dispatch in `build_provider` doesn't need to call `with_api_key_header` for Azure — the default works for all three siblings.

---

## 3. Request body construction (`build_body`)

```rust
{
  "model": "<stripped>",
  "max_tokens": 1024,
  "messages": [...],
  "stream": true,
  "system": [{"type": "text", "text": "...", "cache_control": {"type": "ephemeral"}}],
  "thinking": {"type": "enabled", "budget_tokens": 1024},
  "tools": [{...tool_def..., "cache_control": {"type": "ephemeral"}}, ...]
}
```

### Model prefix stripping

```rust
let model = req.model
    .strip_prefix("oa/")          // OllamaAnthropic
    .or_else(|| req.model.strip_prefix("azure/"))    // AzureAIFoundry
    .unwrap_or(&req.model);       // Anthropic — no prefix
```

So `oa/qwen3-coder` → `qwen3-coder`; `azure/<deployment>` → `<deployment>`; `claude-sonnet-4-6` passes through unchanged.

### Messages

`Role::System` messages are FILTERED OUT (`.filter(|m| !matches!(m.role, Role::System))`) — Anthropic puts the system prompt at the top level, not in the messages array. The system *content* is consumed from `req.system`, not from history.

Each remaining message becomes:
```json
{ "role": "user" | "assistant", "content": <m.content as-is> }
```

`m.content` is `Vec<ContentBlock>` — Anthropic's wire format is the canonical one, so `ContentBlock` serializes 1:1: text, image, tool_use, tool_result, thinking blocks all pass through.

### Prompt caching breakpoints (Anthropic-only)

Anthropic allows up to 4 `cache_control` markers per request. The provider sets three by default:

1. **System prompt** — wrapped in `[{type: "text", text: sys, cache_control: {type: "ephemeral"}}]`. Cached because it's byte-stable across turns (until composer rebuilds it).
2. **Last tool definition** — `tools[arr.len()-1].cache_control = ephemeral`. The whole tool schema block becomes a cached prefix.
3. **Second-to-last message** — when `messages.len() >= 3`, `cache_control` is added to the last content block of the message at index `len()-2`. This is the rolling-conversation breakpoint: that message is byte-stable across the next call (the newest message is the live user turn, uncached by definition).

Skipped when history < 3 messages — Anthropic's minimum cacheable prefix is 1024 tokens, so 1-2 messages rarely qualify and the breakpoint slot is better preserved for later turns.

### Thinking

```rust
if let Some(budget) = req.thinking_budget {
    if budget > 0 {
        body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
    }
}
```

Only emitted when `budget > 0`. Anthropic's extended-thinking models (Claude 4.x with `thinking_budget` set) use this to allocate a token budget for reasoning the user can later inspect. The reasoning text round-trips back to the model on subsequent turns via the `Thinking` content block (assembler folds `text_delta` inside thinking blocks into `Thinking` events).

### Tool definitions

`req.tools: Vec<ToolDef>` serializes directly via `json!(req.tools)` — `ToolDef` is shaped to match Anthropic's `{name, description, input_schema}` form. The last array entry gets the third `cache_control` breakpoint.

### Sample minimal body

```rust
StreamRequest {
    model: "claude-sonnet-4-5".into(),
    system: Some("you are helpful".into()),
    messages: vec![Message::user("hi")],
    tools: vec![],
    max_tokens: 1024,
    thinking_budget: None,
}
```
produces:
```json
{
    "model": "claude-sonnet-4-5",
    "max_tokens": 1024,
    "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
    "stream": true,
    "system": [{"type": "text", "text": "you are helpful", "cache_control": {"type": "ephemeral"}}]
}
```

`tools` and `thinking` keys are absent when empty/none.

---

## 4. Stream pipeline

```rust
async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
    let body = Self::build_body(&req);
    let resp = self.client.post(&self.base_url)
        .header(self.auth_header_name(), &self.api_key)
        .header("anthropic-version", API_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send().await?;
    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(Error::Provider(format!(
            "http {status}: {}", super::redact_key(&text, &self.api_key)
        )));
    }
    let byte_stream = resp.bytes_stream();
    let raw_dump = super::RawDump::new(format!("anthropic {}", req.model));

    Ok(Box::pin(try_stream! {
        let mut buffer = String::new();
        while let Some(chunk) = byte_stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(boundary) = buffer.find("\n\n") {
                let event_text: String = buffer.drain(..boundary + 2).collect();
                if let Some(ev) = parse_sse_event(event_text.trim_end_matches('\n'))? {
                    if let ProviderEvent::TextDelta(ref s) = ev { raw.push(s); }
                    yield ev;
                }
            }
        }
        raw.flush();
    }))
}
```

Key details:

- **HTTP error handling** — non-2xx response body is read in full, passed through `redact_key` to scrub the API key, and surfaced as `Error::Provider("http {status}: {body}")`. The agent loop's retry layer treats this as transient (default — only `Error::Config` skips retry).
- **SSE framing** — chunks accumulate into a `String` buffer; events are split on `\n\n`. The buffer is drained in-place to avoid reallocating per event.
- **`parse_sse_event`** is per-event (one or more `data:` lines, terminated by blank line). Returns `Ok(None)` for events we deliberately skip (`ping`, `message_stop` marker — `message_delta` already carried the stop info).
- **`RawDump`** captures only `TextDelta` payloads (not thinking, not tool input). Flushed at end of stream or on Drop.

### `parse_sse_event`

```rust
pub fn parse_sse_event(raw: &str) -> Result<Option<ProviderEvent>>
```

Walks each line of the event, finds the last `data:` (or `data: `) line, parses it as JSON, switches on `type`. The `event:` line (e.g. `event: message_start`) is ignored — Anthropic mirrors the type into the JSON body.

---

## 5. `list_models`

```rust
async fn list_models(&self) -> Result<Vec<ModelInfo>> {
    // /v1/messages → /v1/models
    let models_url = self.base_url
        .rsplit_once("/messages")
        .map(|(base, _)| format!("{base}/models"))
        .unwrap_or_else(|| format!("{}/models", self.base_url.trim_end_matches('/')));
    // GET with same auth headers, parse data[] array
}
```

URL transform: replaces the trailing `/messages` segment of `base_url` with `/models`. For the default URL this yields `https://api.anthropic.com/v1/models`. The fallback (`base_url + "/models"`) handles non-standard base URLs that don't end in `/messages`.

Response shape: `{"data": [{"id": "...", "display_name": "..."}, ...]}`. Sorted by `id` ascending. `display_name` is captured when present (Settings UI shows it; falls back to `id`).

---

## 6. The three sibling variants

### `Anthropic`

```rust
ProviderKind::Anthropic => Ok(Arc::new(AnthropicProvider::new(api_key)))
```

Default URL, `x-api-key` auth, full feature set. Routing prefix: `claude-`. Aliases: `sonnet`/`opus`/`haiku`.

### `OllamaAnthropic`

```rust
ProviderKind::OllamaAnthropic => {
    let base = std::env::var("OLLAMA_BASE_URL")
        .unwrap_or_else(|_| crate::providers::ollama::DEFAULT_BASE_URL.to_string());
    let url = format!("{}/v1/messages", base.trim_end_matches('/'));
    Ok(Arc::new(AnthropicProvider::new("ollama").with_base_url(url)))
}
```

Routing prefix: `oa/` (e.g. `oa/qwen3-coder`). Auth header value is the literal string `"ollama"` (Ollama's `/v1/messages` endpoint accepts anything as `x-api-key`; convention is `"ollama"`). The `oa/` prefix is stripped in `build_body`.

**Capability notes:**
- No prompt caching (Ollama ignores `cache_control` blocks)
- No extended thinking (Ollama ignores `thinking` field)
- Local network only — `OLLAMA_BASE_URL` defaults to `http://localhost:11434`

### `AzureAIFoundry`

```rust
ProviderKind::AzureAIFoundry => {
    let endpoint = std::env::var("AZURE_AI_FOUNDRY_ENDPOINT")
        .map_err(|_| Error::Config("AZURE_AI_FOUNDRY_ENDPOINT not set".into()))?;
    let messages_url = format!("{}/anthropic/v1/messages", endpoint.trim_end_matches('/'));
    Ok(Arc::new(AnthropicProvider::new(api_key).with_base_url(messages_url)))
}
```

Routing prefix: `azure/<deployment>` (e.g. `azure/my-claude-sonnet-deployment`). The `azure/` prefix is stripped in `build_body` and what remains is sent as `model` — Azure expects the deployment name there.

URL form: `{$AZURE_AI_FOUNDRY_ENDPOINT}/anthropic/v1/messages`. Endpoint is per-resource (e.g. `https://my-resource.services.ai.azure.com`). Azure AI Foundry exposes Anthropic models on this path with the `api-key:` header — but per the source comment, Azure also accepts `x-api-key`, so no header override is needed.

API key from `AZURE_AI_FOUNDRY_API_KEY`. **Endpoint required** — there's no default URL; `build_provider` errors with a clear "AZURE_AI_FOUNDRY_ENDPOINT not set — add it in Settings or export the env var" if missing.

---

## 7. Testing

`anthropic::tests` — 14 tests exercising body construction, parser, and end-to-end mock streams.

**Parser:**
- `parse_message_start`
- `parse_text_delta`
- `parse_tool_use_start`
- `parse_input_json_delta`
- `parse_content_block_stop`
- `parse_message_delta_with_usage`
- `parse_ignores_ping_and_message_stop_marker`
- `parse_ignores_event_with_no_data_line`

**Body:**
- `build_body_puts_system_at_top_level_and_excludes_from_messages`
- `build_body_caches_second_to_last_message_when_history_long_enough`
- `build_body_skips_message_cache_when_history_too_short`
- `build_body_message_cache_is_byte_stable_across_calls` — guards against silent cache busts
- `build_body_omits_empty_system_and_tools`
- `build_body_preserves_tool_result_blocks`

**End-to-end (wiremock):**
- `stream_end_to_end_with_mock_server` — full SSE replay → events
- `stream_with_tool_use_assembles_to_turn_result` — partial_json across 3 chunks combines correctly
- `list_models_parses_data_array`
- `stream_surfaces_http_errors` — 401 propagates as `Error::Provider("http 401: ...")`

---

## 8. Notable behaviors / gotchas

- **`message_delta` carries the final usage**, not `message_stop`. The provider impl maps `message_delta` to `MessageStop` (downstream agent loop uses `MessageStop` to finalize the turn) and ignores the literal `message_stop` SSE event.
- **System messages in history are dropped.** If history contains `Message { role: System, ... }` entries (rare but possible if a user manipulated history), they don't make it into the request — Anthropic forbids `system` role inside the `messages` array.
- **Cache breakpoint slot allocation** — when adding a 4th breakpoint (e.g. for a per-tool result), be aware the third is already taken by the rolling-message rule. Anthropic enforces a hard limit of 4.
- **Cache control on the last tool ONLY** — adding it per-tool wouldn't help (Anthropic uses prefix caching, so the marker on the last tool covers all preceding tools too).
- **`api_key_header = None` doesn't mean "no auth"** — it means "use default `x-api-key`". The auth header is always sent. For OllamaAnthropic, the value is `"ollama"`; Ollama's shim accepts anything.
- **No request retry inside the provider.** Retry on transient errors is the agent loop's responsibility (`Agent::run_turn` has `max_retries` with exponential backoff).
- **No structured `ThinkingDelta` from Anthropic.** Extended thinking text streams as `text_delta` inside a `thinking` content block — the agent's history persistence logic detects this. Some downstream providers (DeepSeek/o-series) have a separate `reasoning_content` field; Anthropic doesn't.

---

## 9. What's NOT supported

- **No Anthropic Vertex / Bedrock URL variants in this provider.** Both technically speak the same wire format but require AWS SigV4 / GCP IAM auth that this `AnthropicProvider` doesn't implement. Adding them would be a new provider impl (or a new auth-header strategy on this one) — not a config tweak.
- **No `tool_choice` field** — every request lets the model decide whether to use a tool. The `Provider` trait doesn't expose `tool_choice`; if needed it would be a new `StreamRequest` field.
- **No batch / async messages API** (the offline batch endpoint at `/v1/messages/batches`). This provider is streaming-only.
- **No file uploads (`/v1/files`)** — image inputs go inline as base64 in `ContentBlock::Image { source: ImageSource::Base64 { ... } }`.
