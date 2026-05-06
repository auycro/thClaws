# Google Gemini provider

`GeminiProvider` (`providers/gemini.rs`, 1102 LOC) speaks Google's `generativelanguage.googleapis.com/v1beta` SSE format. The wire shape is different enough from Anthropic and OpenAI to need its own adapter rather than a config knob on the OpenAI-compat impl. One `ProviderKind` variant uses this impl: `Gemini`. Routing prefix: `gemini-` OR `gemma-` (Gemma open-weights are served via the same API).

**Source:** `crates/core/src/providers/gemini.rs`
**Constants:**
- `DEFAULT_BASE_URL = "https://generativelanguage.googleapis.com"` (note: NO trailing path — provider builds the full URL per request)

**Cross-references:**
- [`providers.md`](providers.md) — `Provider` trait, `StreamRequest`, `ProviderEvent`
- [`provider-anthropic.md`](provider-anthropic.md), [`provider-openai.md`](provider-openai.md) — wire-format contrast

---

## 1. What's distinctive

| Aspect | Anthropic | OpenAI | Gemini |
|---|---|---|---|
| URL shape | `/v1/messages` (model in body) | `/v1/chat/completions` (model in body) | `/v1beta/models/{model}:streamGenerateContent?alt=sse` (model in PATH) |
| Auth header | `x-api-key` | `Authorization: Bearer` | `x-goog-api-key` |
| Top-level history field | `messages` | `messages` | `contents` |
| Roles | `user` / `assistant` | `user` / `assistant` / `system` / `tool` | `user` / `model` (no `assistant`) |
| System prompt | top-level `system` content blocks | `messages[0].role="system"` | top-level `systemInstruction.parts[].text` |
| Content shape | `content: [...]` (block array) | `content: "string"` OR `content: [...]` (mixed) | `parts: [{text}|{functionCall}|{functionResponse}|{inlineData}]` |
| Tool defs | top-level `tools` array | `tools: [{type, function: {...}}]` | `tools: [{functionDeclarations: [...]}]` |
| Tool call shape | `content_block: {type: "tool_use", id, name, input}` | `tool_calls: [{id, function: {name, arguments}}]` | `parts: [{functionCall: {name, args}}]` (no id!) |
| Tool result shape | `content_block: {type: "tool_result", tool_use_id, content}` | `role: "tool"` message with `tool_call_id` | `parts: [{functionResponse: {name, response: {content}}}]` (matched by name, not id!) |
| Tool input streaming | yes (`partial_json`) | yes (`tool_calls[].function.arguments` deltas) | **NO — single chunk with full args** |
| SSE event lines | `event: ...` + `data: ...` | `data: ...` only, `[DONE]` terminator | `data: ...` only, NO terminator |
| SSE line separator | `\n\n` | `\n\n` | `\r\n\r\n` (CRLF) on this endpoint |

The big shape difference: **tool calls have no id**. Anthropic emits `toolu_01A`, OpenAI emits `call_abc`, Gemini emits nothing — it identifies functions by `name` alone, and pairs the response by name. This impl synthesizes a local id (`gemini-call-0`, `gemini-call-1`, ...) so the rest of the stack (which assumes id-based pairing) keeps working, then maintains an `id → name` map at request time to convert ToolResult blocks back into Gemini's `functionResponse: {name, response}` shape.

---

## 2. Struct + builder

```rust
pub struct GeminiProvider {
    client: Client,
    api_key: String,
    base_url: String,         // defaults to DEFAULT_BASE_URL (host only, no path)
}

impl GeminiProvider {
    pub fn new(api_key: impl Into<String>) -> Self;
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self;
}
```

Spartan compared to OpenAI's builder — no model-prefix-strip (Gemini ids pass through unchanged), no auth header override (always `x-goog-api-key`).

---

## 3. Request body construction (`build_body`)

```rust
{
  "contents": [...],
  "generationConfig": { "maxOutputTokens": 1024 },
  "systemInstruction": { "parts": [{"text": "..."}] },
  "tools": [{ "functionDeclarations": [{"name", "description", "parameters"}, ...] }]
}
```

### Messages → `contents`

```rust
fn messages_to_gemini(req: &StreamRequest) -> Vec<Value>
```

First pass: build `id_to_name: HashMap<String, String>` from every assistant message's `ContentBlock::ToolUse`. This is the lookup the second pass uses to resolve `functionResponse.name` from `ToolResult.tool_use_id`.

Second pass: walk messages, skip System role (goes in `systemInstruction`), map `User → "user"` and `Assistant → "model"`, build `parts: [...]` per message:

| `ContentBlock` | Becomes |
|---|---|
| `Text { text }` | `{text}` part (skipped if empty) |
| `Thinking { .. }` | (dropped — Gemini doesn't accept reasoning_content) |
| `Image { source: Base64 { media_type, data } }` | `{inlineData: {mimeType, data}}` part |
| `ToolUse { name, input, .. }` | `{functionCall: {name, args: input}}` (id is dropped — Gemini doesn't use it) |
| `ToolResult { tool_use_id, content, .. }` | `{functionResponse: {name: id_to_name[tool_use_id], response: {content: content.to_text()}}}` |

If `ToolResult.content` carries images (Read on a PNG), the impl ALSO pushes `{inlineData: {mimeType, data}}` parts into the same content. Gemini accepts mixed part types within one content block, and a vision-capable model decodes inlineData as images.

Empty `parts` arrays are skipped (the message itself is dropped if it produced nothing — pure-Thinking messages from non-Gemini providers, etc.).

### `systemInstruction` (most models) vs prepended user prompt (Gemma)

```rust
let is_gemma = req.model.starts_with("gemma-");
if is_gemma {
    // Gemma: inline system prompt as first user turn + add <thinking> wrap rule.
    // Skip systemInstruction (Gemma errors "Developer instruction is not enabled").
} else if let Some(sys) = &req.system {
    if !sys.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": sys}]});
    }
}
```

Gemma open-weights models served on the Gemini API don't support `systemInstruction` ("Developer instruction is not enabled") OR function calling ("Function calling is not enabled"). The provider detects `gemma-` prefix and:
1. Inlines the system prompt as the first user turn (prepended to `contents`)
2. Appends a thinking-format rule: *"wrap any internal reasoning, planning, or self-talk in `<thinking>...</thinking>` tags. Put ONLY the final user-facing answer outside those tags."*
3. SKIPS the `tools` field entirely

The `<thinking>` wrap rule lets the assembler's `<think>` tag splitter (in [`providers.md`](providers.md) §6) route Gemma's chain-of-thought to `Thinking` events instead of leaking it as `Text`.

### Tools → `functionDeclarations`

```rust
"tools": [{
    "functionDeclarations": [
        {"name": ..., "description": ..., "parameters": ...},
        ...
    ]
}]
```

Top-level `tools` is an array of one object containing `functionDeclarations`. Each declaration has `name` / `description` / `parameters` (same JSON schema as other providers). Tools are omitted entirely for Gemma.

### Sample minimal body

```rust
StreamRequest {
    model: "gemini-2.5-flash".into(),
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
    "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
    "generationConfig": {"maxOutputTokens": 1024},
    "systemInstruction": {"parts": [{"text": "you are helpful"}]}
}
```

---

## 4. Stream pipeline

```rust
async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
    let body = Self::build_body(&req);
    let url = format!(
        "{}/v1beta/models/{}:streamGenerateContent?alt=sse",
        self.base_url.trim_end_matches('/'),
        req.model
    );
    let resp = self.client.post(&url)
        .header("x-goog-api-key", &self.api_key)
        .header("content-type", "application/json")
        .json(&body)
        .send().await?;
    if !resp.status().is_success() { return Err(...); }   // body redacted

    let byte_stream = resp.bytes_stream();
    let is_gemma = req.model.starts_with("gemma-");
    let debug_log = open_debug_log(&body, &req.model);
    let raw_dump = super::RawDump::new(format!("gemini {}", req.model));

    Ok(Box::pin(try_stream! {
        let mut buffer = String::new();
        let mut state = ParseState::default();
        let mut think = ThinkFilter::new();   // Gemma <thinking> tag stripper
        while let Some(chunk) = byte_stream.next().await {
            let chunk = chunk?;
            if let Some(f) = log.as_mut() { f.write_all(&chunk); }   // optional debug log
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            // SSE framing: try \r\n\r\n first (Google emits CRLF), fall back to \n\n
            while let Some((boundary, sep_len)) = buffer.find("\r\n\r\n").map(|p| (p, 4))
                .or_else(|| buffer.find("\n\n").map(|p| (p, 2)))
            {
                let event_text: String = buffer.drain(..boundary + sep_len).collect();
                for event in parse_sse_event(event_text.trim_end_matches(['\n', '\r']), &mut state)? {
                    if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                    if is_gemma {
                        // Wrap <thinking>...</thinking> content in ANSI dim
                        // codes via ThinkFilter so it's visually demoted.
                        if let ProviderEvent::TextDelta(s) = event {
                            let transformed = think.push(&s);
                            if !transformed.is_empty() { yield ProviderEvent::TextDelta(transformed); }
                            continue;
                        }
                    }
                    yield event;
                }
            }
        }
        if is_gemma {
            let tail = think.flush();
            if !tail.is_empty() { yield ProviderEvent::TextDelta(tail); }
        }
        raw.flush();
    }))
}
```

Three notable subtleties:

1. **CRLF SSE framing.** Google's `streamGenerateContent` returns `\r\n\r\n` event separators (per HTTP spec); a plain `\n\n` search would silently buffer forever. The provider tries `\r\n\r\n` first and falls back to `\n\n` for completeness.
2. **Optional debug log.** `THCLAWS_DEBUG_GEMINI=1` writes raw chunks to `./.thclaws/logs/gemini-raw.log`; `THCLAWS_DEBUG_GEMINI=/path/to/file` writes to that exact path. Independent of `THCLAWS_SHOW_RAW`. Useful for diagnosing wire-format bugs without cluttering the terminal.
3. **`ThinkFilter` for Gemma only.** Wraps `<thinking>...</thinking>` content in ANSI dim codes inline (the assembler's `<think>` splitter handles routing to `Thinking` events, but the visual demotion happens here for Gemma's prose-style reasoning). Other Gemini models don't get this layer.

---

## 5. SSE parsing (`parse_sse_event`)

```rust
pub struct ParseState {
    pub seen_message_start: bool,
    pub next_tool_id: u64,         // monotonic counter for synthesized tool ids
}
```

Per-event:
1. Find the `data:` line, parse as JSON.
2. On first event, emit `MessageStart { model: data.modelVersion }`.
3. Walk `candidates[0].content.parts[]`:
   - `{text: "..."}` → `TextDelta(text)` (skipped if empty)
   - `{functionCall: {name, args}}` → emits THREE events: `ToolUseStart { id: "gemini-call-{n}", name }` + `ToolUseDelta { partial_json: args.to_string() }` + `ContentBlockStop`. The synthesized id increments per call. The full args JSON is dumped as a single delta — Gemini doesn't stream tool input.
4. If `candidates[0].finishReason` is set, emit `MessageStop { stop_reason: reason, usage: parsed_or_None }`.

### Usage

```json
"usageMetadata": {
    "promptTokenCount": 20,
    "candidatesTokenCount": 10,
    "totalTokenCount": 30
}
```

`Usage::input_tokens = promptTokenCount`, `output_tokens = candidatesTokenCount`. Cache fields stay `None` (Gemini implicit caching is server-side and not surfaced).

### Why three events for one functionCall

The downstream assembler ([`providers.md`](providers.md) §6) is a state machine that requires `ToolUseStart` + N×`ToolUseDelta` + `ContentBlockStop` to construct an `AssembledEvent::ToolUse`. Gemini delivers the full call atomically, so the provider fires the three-event pattern in a single iteration — `assemble` doesn't care that they happen in one tick.

---

## 6. `list_models`

```rust
async fn list_models(&self) -> Result<Vec<ModelInfo>> {
    let url = format!("{}/v1beta/models", self.base_url.trim_end_matches('/'));
    // GET → JSON {models: [{name, displayName, ...}]} → ModelInfo
}
```

Response shape: `{"models": [...]}` (NOT `data:` like OpenAI). Each entry has `name: "models/gemini-2.0-flash"` — the provider strips the `models/` prefix so users can paste IDs into `/model` directly. `displayName` is captured.

---

## 7. Tests

Per-file count of unit tests (~15+):
- `parse_sse_event` test cases for text, functionCall, mixed parts, finishReason, usageMetadata
- `messages_to_gemini` for role mapping, id_to_name lookup, image inlineData, multi-part tool results
- `build_body` for systemInstruction vs prepended-user (Gemma path), tools omission for Gemma, `<thinking>` rule append
- `ThinkFilter` for `<thinking>` open/close across chunk boundaries, ANSI wrapping, EOF flush
- End-to-end mock-server tests (wiremock) for both Gemini and Gemma model paths

---

## 8. Notable behaviors / gotchas

- **Tool ids are synthetic.** `gemini-call-{n}` where `n` increments per stream. They don't survive across streams (the counter is per-`ParseState`, which is per-stream). If your code persists tool ids, those references become invalid on the next turn — but the provider re-derives `id_to_name` from history every turn, so functionResponse pairing still works.
- **Tool input arrives as one chunk.** UI components that want to show tool-input typing animations get nothing for Gemini — the entire JSON arrives in a single `ToolUseDelta`. Display the call as "in progress" for the duration of the request rather than animating per-character.
- **No `[DONE]` terminator.** The byte stream simply ends when the response closes. The agent loop's `MessageStop` (from `finishReason`) is what triggers turn finalization.
- **Gemma is a parallel universe.** No system prompt, no tools, prepended user message with format rule. If your skill / agent depends on tool calls, Gemma WILL silently ignore them — the wire body has no `tools` field. Pick `gemini-*` for any tool workflow.
- **`promptTokenCount` includes the system prompt.** Anthropic separates system tokens via `cache_creation` / `cache_read`; Gemini lumps everything into `promptTokenCount`. The agent's usage tracking treats this as `input_tokens` regardless.
- **Auth header value is the bare key.** No `Bearer ` prefix. Don't accidentally wrap it.
- **The `?key=...` query-param auth alternative is not used.** This impl always uses the `x-goog-api-key` header. Gemini's `?key=` form would echo the key into 4xx error bodies — `redact_key` wouldn't catch it because `redact_key` operates on the response body, not the request URL. Header form keeps secrets out of error logs entirely.
- **Vision-capable models only for inlineData.** If you call a non-vision Gemini model with image-bearing content (Read on a PNG), the request 400s. The user must pick `gemini-1.5-*`, `gemini-2.x`, etc.
- **`list_models` is a separate URL shape.** `GET /v1beta/models` (no `:streamGenerateContent` suffix, no `?alt=sse`).

---

## 9. What's NOT supported

- **Vertex AI variant** — Google's enterprise endpoint (`{region}-aiplatform.googleapis.com/v1/projects/{project}/locations/{region}/publishers/google/models/{model}:streamGenerateContent`). Different URL, requires GCP IAM auth (OAuth2 access token, not API key). Would be a separate provider impl.
- **`tool_config.function_calling_config`** — fine-grained tool control (mode: AUTO / ANY / NONE, allowed_function_names). Not exposed.
- **`safetySettings`** — Gemini's per-category safety threshold knobs. Defaults are sent unmodified.
- **`generationConfig.temperature` / `topP` / `topK`** — only `maxOutputTokens` is set today.
- **Implicit caching** — Gemini's server-side prompt cache is automatic but not surfaced via the usage fields, so the agent can't tell when a cache hit happened.
- **File API** (`/v1beta/files`) — image inputs go inline as base64 in `inlineData` parts.
- **`code_execution` built-in tool** — would need a separate `tool_use` shape in the parser.
- **Multi-turn caching via `cachedContents`** (the explicit cache resource) — could amortize costs significantly for long system prompts but isn't wired.
