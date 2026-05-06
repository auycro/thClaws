# Ollama provider

Two variants share a near-identical NDJSON wire format but enough auth/feature differences to warrant separate impls:

- `OllamaProvider` (`providers/ollama.rs`, 930 LOC) — **local**, no auth, configurable endpoint, leak-detection for small models
- `OllamaCloudProvider` (`providers/ollama_cloud.rs`, 420 LOC) — **hosted**, Bearer auth, fixed endpoint, native thinking + image support

Two `ProviderKind` variants: `Ollama` (`ollama/` prefix) and `OllamaCloud` (`ollama-cloud/` prefix). Note: `OllamaAnthropic` (`oa/` prefix) is a THIRD path that uses `AnthropicProvider` against Ollama's Anthropic-compat shim — it's covered in [`provider-anthropic.md`](provider-anthropic.md), NOT here.

**Source:**
- `crates/core/src/providers/ollama.rs`
- `crates/core/src/providers/ollama_cloud.rs`

**Constants:**
- `ollama::DEFAULT_BASE_URL = "http://localhost:11434"`
- Ollama Cloud: hard-coded `"https://ollama.com/api/chat"` and `"https://ollama.com/v1/models"`

**Cross-references:**
- [`providers.md`](providers.md) — `Provider` trait, `StreamRequest`, `ProviderEvent`
- [`provider-anthropic.md`](provider-anthropic.md) — `OllamaAnthropic` sibling at `/v1/messages`
- [`provider-openai.md`](provider-openai.md) — wire-format contrast (Ollama is NDJSON not SSE)

---

## 1. NDJSON wire format

Both variants use the same line-delimited JSON stream:

```
{"model":"llama3.2","message":{"role":"assistant","content":"Hello"},"done":false}
{"model":"llama3.2","message":{"role":"assistant","content":" world"},"done":false}
{"model":"llama3.2","message":{"role":"assistant","content":""},"done":true,"done_reason":"stop","prompt_eval_count":5,"eval_count":2}
```

- One complete JSON object per line, separated by `\n`.
- **No `data:` prefix, no `event:` lines, no `[DONE]` terminator.** Final line has `"done": true`.
- Tool calls are NOT streamed — the entire `function.arguments` object arrives in one `message.tool_calls` payload, with `done: false` continuing.
- Usage metadata (`prompt_eval_count`, `eval_count`, `done_reason`) lives on the final `done: true` line.

### Wire-element → `ProviderEvent` mapping

| Source | Mapped to |
|---|---|
| First line parsed (any) | `MessageStart { model: chunk.model }` (gated by `seen_message_start`) |
| `message.content: "..."` (non-empty) | `TextDelta(content)` (or buffered for leak-detect, see §4) |
| `message.thinking: "..."` (non-empty, Ollama Cloud) | `ThinkingDelta(thinking)` |
| `message.tool_calls[]` | per call: `ToolUseStart { id, name }` + `ToolUseDelta { partial_json: args }` + `ContentBlockStop` (3 events fired together) |
| `done: true` | (flush leak-detect buffer if pending) → `MessageStop { stop_reason: done_reason, usage }` |

The 3-event tool-call pattern matches Gemini's approach — full args arrive in one parse cycle and the assembler folds them into a single `AssembledEvent::ToolUse`.

---

## 2. `OllamaProvider` — local

```rust
pub struct OllamaProvider {
    client: Client,
    base_url: String,    // defaults to "http://localhost:11434"
}

impl OllamaProvider {
    pub fn new() -> Self;
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self;
    fn model_name(req_model: &str) -> &str {
        req_model.strip_prefix("ollama/").unwrap_or(req_model)
    }
    pub async fn show(&self, model: &str) -> Result<(u32, &'static str)>;  // /api/show probe
}
```

**No auth field.** No API key needed; the local Ollama daemon trusts anyone reaching the URL. The `build_provider` dispatch in [`providers.md`](providers.md) §4 builds this provider in Stage B (auth-less).

### `show(model)` — context-window probe

```rust
pub async fn show(&self, model: &str) -> Result<(u32, &'static str)>
```

`POST /api/show` returns the model's metadata. The provider extracts the context window in priority order:

1. `parameters.num_ctx` (the value this Ollama instance will actually accept per turn) — returns `(n, "num_ctx")`
2. Fallback: any `model_info["<arch>.context_length"]` (the model's native ceiling) — returns `(n, "native")`

Used by `model_catalogue::refresh_from_remote` to populate the local model cache so the GUI Settings dropdown shows accurate context limits.

### `/api/chat` request body

```rust
{
  "model": "<post-prefix-strip>",
  "messages": [...],
  "stream": true,
  "tools": [{"type": "function", "function": {"name", "description", "parameters"}}, ...]
}
```

`max_tokens`, `thinking_budget` are NOT sent — Ollama uses model-side defaults. (If you need a token cap, override on the Ollama side via the `Modelfile` or `parameters.num_predict`.)

### Message conversion

Each `Message { role, content: Vec<ContentBlock> }`:
- `Text { text }` → joined into `content` string
- `Thinking { .. }` → DROPPED (local Ollama's `/api/chat` has no `thinking` field; only Ollama Cloud does)
- `Image { .. }` → DROPPED (local Ollama's vision support uses a separate `images: [...]` field that this impl doesn't plumb; OllamaCloud DOES support it — see §3)
- `ToolUse { name, input, .. }` → pushed to `tool_calls: [{function: {name, arguments: input}}]` (no `id` — Ollama relies on order)
- `ToolResult { content, .. }` → emitted as a separate `{role: "tool", content: content.to_text()}` message AFTER the parent message

System prompt prepended as `messages[0] = {role: "system", content: sys}`.

**No `tool_call_id`** — Ollama's contract is "tool messages follow the assistant message in order, matched by position." If a model emits N parallel tool calls, the next N tool messages match in order. The provider's emission preserves order.

---

## 3. `OllamaCloudProvider` — hosted

```rust
pub struct OllamaCloudProvider {
    client: Client,
    api_key: String,
}

impl OllamaCloudProvider {
    pub fn new(api_key: String) -> Self;
    fn model_name(req_model: &str) -> &str {
        req_model.strip_prefix("ollama-cloud/").unwrap_or(req_model)
    }
    fn think_value(model: &str) -> Value;   // see below
}
```

URLs are hard-coded:
- `POST https://ollama.com/api/chat` for streaming
- `GET https://ollama.com/v1/models` for `list_models`

No `with_base_url` — not user-configurable. `OLLAMA_CLOUD_API_KEY` env is required.

### `/api/chat` request body

```rust
{
  "model": "<post-prefix-strip>",
  "messages": [...],
  "stream": true,
  "think": true | "high",
  "tools": [...]
}
```

The extra `think` field is what enables thinking-model output:
```rust
fn think_value(model: &str) -> Value {
    if model.starts_with("gpt-oss") { json!("high") }   // GPT-OSS expects "low"|"medium"|"high"
    else { json!(true) }                                 // other thinking models accept boolean
}
```

GPT-OSS-family models (e.g. `gpt-oss-120b-cloud`) require `think: "low"|"medium"|"high"`; everything else accepts `think: true`. The provider hard-codes `"high"` for GPT-OSS — there's no way to override per-call.

### Message conversion (vs local Ollama)

Two additional capabilities beyond local:

- **`Thinking { content }` → `message.thinking: "..."`** (sibling field on the assistant message). Required by the server: if the model emitted reasoning_content on a prior turn and you don't echo it back, the server 400s.
- **`Image { source: Base64 { data, .. } }` → `message.images: [base64...]`** array. Strips a `data:image/...;base64,` prefix if present (the canonical `ImageSource::Base64` shape doesn't include it, but defensive). Vision models receive the images for analysis.

```rust
let mut msg = json!({"role": role});
if has_images {
    msg["content"] = json!(content);
    msg["images"] = json!(images);
} else if has_text || has_tools {
    msg["content"] = json!(content);
}
if has_thinking { msg["thinking"] = json!(thinking_text); }
if has_tools { msg["tool_calls"] = json!(tool_calls); }
```

Tool calls + results work identically to local Ollama (no `tool_call_id`, separate `role: "tool"` messages in order).

---

## 4. Leak-detect mode (`OllamaProvider` only)

Some small Ollama models (notably `qwen2.5-coder`) emit tool calls as a markdown-fenced JSON object in `message.content` instead of via the structured `tool_calls` channel — see issue #50. Without intervention, the agent loop would see the call as plain text and never dispatch the tool.

```rust
pub struct ParseState {
    pub seen_message_start: bool,
    pub tool_names: Vec<String>,        // names from req.tools — passed at stream start
    pub buffered_text: String,
    pub buffering: bool,                // entered when first content chunk starts with ``` or {
    pub seen_content: bool,
}
```

Strategy:
1. On first non-empty content chunk for the turn, if tools are advertised AND the chunk starts with ``` or `{`, enter `buffering = true` mode. Held back from the streaming output.
2. Continue accumulating into `buffered_text`.
3. On `done: true`:
   - Try `try_extract_leaked_tool_call(&buffered, &tool_names)`:
     - Strip optional ` ```json ` / ` ``` ` fences
     - Parse as `{"name": "...", "arguments": {...}}`
     - Validate that `name` matches a registered tool
   - If matched: emit `ToolUseStart { id: "ollama-leaked-call", name } + ToolUseDelta { partial_json } + ContentBlockStop` as if it had streamed via `tool_calls`.
   - If not matched: emit the buffered text as a single `TextDelta` (preserves the original output).

Buffering only on suspicious prefixes preserves streaming UX for ordinary replies — most chunks bypass the buffer entirely.

`OllamaCloudProvider` does NOT have this layer — its hosted models reliably emit `tool_calls` properly.

---

## 5. Stream pipeline (both variants)

```rust
let resp = self.client.post(<url>).json(&body).send().await?;
let byte_stream = resp.bytes_stream();

Ok(Box::pin(try_stream! {
    let mut buffer = String::new();
    let mut state = ParseState::with_tools(tool_names);  // (Ollama) or default (Cloud)
    while let Some(chunk) = byte_stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk?));
        // NDJSON: split on \n
        while let Some(newline) = buffer.find('\n') {
            let line: String = buffer.drain(..newline + 1).collect();
            let line = line.trim();
            if line.is_empty() { continue; }
            for event in parse_line(line, &mut state)? {
                if let ProviderEvent::TextDelta(ref s) = event { raw.push(s); }
                yield event;
            }
        }
    }
    // Flush trailing buffered line if no \n terminator
    if !buffer.trim().is_empty() {
        for event in parse_line(buffer.trim(), &mut state)? { yield event; }
    }
    raw.flush();
}))
```

Same byte-stream → buffer → split-on-`\n` → `parse_line` shape for both. The only differences:
- Ollama Cloud sends `Authorization: Bearer <key>` header
- Ollama Cloud's `parse_line` doesn't have leak-detect state (no `tool_names` tracking)
- Ollama Cloud handles `message.thinking` field

---

## 6. `list_models`

### Local Ollama

```rust
let url = format!("{}/api/tags", self.base_url.trim_end_matches('/'));
// GET → JSON {models: [{name, ...}]} → ModelInfo with "ollama/" prefix re-applied
```

Returns models the local daemon has pulled (`ollama pull <name>` populates this). IDs prefixed with `ollama/` so users can paste straight into `/model`. `display_name` not captured.

### Ollama Cloud

```rust
let url = "https://ollama.com/v1/models";
// GET with Bearer auth → JSON {data: [{id, ...}]} → ModelInfo with "ollama-cloud/" prefix re-applied
```

Lists all cloud-hosted models the API key can access.

---

## 7. Testing

`ollama::tests` — extensive (~14+ tests):
- `parse_text_stream_emits_message_start_deltas_and_stop` — basic NDJSON happy path
- `parse_tool_call_emits_use_delta_and_stop` — tool_calls pattern
- Leak-detect mode tests — recognised leaked call, unrecognised name passes through as text, fenced JSON extraction
- `ParseState::with_tools` registers tool names correctly
- Multi-turn assembly via `assemble` + `collect_turn`
- Wiremock end-to-end tests

`ollama_cloud::tests` — fewer (~5+):
- Message conversion with thinking + images
- `think_value` for GPT-OSS vs other models
- Bearer auth on requests

---

## 8. Notable behaviors / gotchas

### Both

- **Tool ids are local synthesis or Ollama-provided.** When the server provides `id`, use it; otherwise synthesize `ollama-call-{i}`. Either way, ids don't survive across streams (per-`ParseState` counter).
- **No `[DONE]` terminator.** `done: true` line ends the stream; subsequent bytes are unexpected.
- **Tool input arrives whole.** Same UI implication as Gemini: no per-character animation.
- **No request retry inside the provider.**

### Local `Ollama`

- **No auth.** Anyone who can reach `http://localhost:11434` can drive the model. Don't expose Ollama on a public interface without putting it behind something that authenticates.
- **No image support on the wire.** `Image` blocks in history are dropped. Use OllamaCloud, OpenAI, Anthropic, or Gemini for vision.
- **Leak-detect ONLY matches advertised tools.** If a leaked call references a tool that wasn't in `req.tools`, it falls through as text — preventing the model from hallucinating a tool name and triggering an unsafe dispatch.
- **`/api/show` probe is on-demand.** Called by `model_catalogue` refresh, not on every stream. The cached value populates the GUI's context-window display.
- **`OLLAMA_BASE_URL` env shared with `OllamaAnthropic`.** Both providers honor it. If you point this at a remote machine, both `/api/chat` AND `/v1/messages` get routed there.
- **No thinking field.** Even if the model is thinking-capable (qwen3, deepseek-r1 via local Ollama), `Thinking` blocks are dropped on the wire. The implicit-thinking detection in the assembler ([`providers.md`](providers.md) §6) handles the `<think>...</think>` pattern that those models emit as text.

### Ollama Cloud

- **API key required.** No `OLLAMA_BASE_URL` override (URL is hard-coded).
- **`think` field is unconditional** for thinking models. No way to disable thinking — if you want non-thinking output, pick a non-thinking model.
- **Image data prefix stripped defensively.** `data:image/png;base64,...` becomes `...` before going on the wire — Ollama Cloud expects raw base64 in the `images: []` array.
- **No leak-detect.** Cloud models reliably emit `tool_calls`; the buffered-text-leak path doesn't run.

---

## 9. What's NOT supported

### Local `Ollama`
- **Vision via `/api/chat` `images: [...]`** — would be a one-line addition mirroring OllamaCloud's image path.
- **`/api/generate`** (the non-chat completion endpoint) — used by some workflows that don't need turn structure. Not wired.
- **`/api/embeddings`** — embedding generation. Out of scope for the chat agent.
- **Custom model parameters per call** (`temperature`, `top_p`, `num_predict`, `stop`, etc.) — not exposed via `StreamRequest`.

### Ollama Cloud
- **`OLLAMA_CLOUD_BASE_URL` override** — would let users point at a private deployment.
- **`think` value passthrough** — currently auto-derived; no way to set "low" on a non-GPT-OSS model.
- **Per-call thinking budget**.

### Both
- **Embeddings, generate, copy, delete, push, pull endpoints.** This is a chat-only adapter; admin operations happen via the Ollama CLI.
