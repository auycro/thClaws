# OpenAI Responses API provider

`OpenAIResponsesProvider` (`providers/openai_responses.rs`, 516 LOC) speaks OpenAI's newer Responses API at `/v1/responses` — separate from the older Chat Completions endpoint covered in [`provider-openai.md`](provider-openai.md). It exists because some models (Codex, GPT-5.x reasoning variants) only run on `/v1/responses`, and the wire format is different enough from chat/completions that coercing one parser to handle both would be lossy.

One `ProviderKind` variant uses this impl: `OpenAIResponses`. Routing prefix: `codex/` (or any model id containing `codex`).

**Source:** `crates/core/src/providers/openai_responses.rs`
**Constants:**
- `DEFAULT_API_URL = "https://api.openai.com/v1/responses"`

**Cross-references:**
- [`providers.md`](providers.md) — `Provider` trait, `StreamRequest`, `ProviderEvent`
- [`provider-openai.md`](provider-openai.md) — sibling Chat Completions impl

---

## 1. What's different from Chat Completions

| Aspect | Chat Completions | Responses |
|---|---|---|
| Endpoint | `/v1/chat/completions` | `/v1/responses` |
| System prompt placement | `messages[0].role = "system"` | top-level `instructions` field |
| History array name | `messages` | `input` |
| Server-side history | none (client sends full history every turn) | `previous_response_id` in body — server keeps state |
| SSE event names | none (chunks are raw `data:` lines) | typed `event:` lines (`response.output_text.delta`, etc.) |
| Tool defs | nested under `{type: "function", function: {name, description, parameters}}` | flat `{type: "function", name, description, parameters}` |
| Tool call shape (in input) | `tool_calls: [{function: {name, arguments}}]` on assistant message | separate `function_call` items at the top level |
| Tool result shape (in input) | `role: "tool"` message | separate `function_call_output` items at the top level |
| Max tokens field | `max_completion_tokens` | `max_output_tokens` |

The big difference is **server-side history**: when `previous_response_id` is set, the server already remembers the prior turn — the request only needs to carry the new input. The provider tracks `last_response_id: Arc<Mutex<Option<String>>>` and includes it on every subsequent call. This means the agent's locally-stored history is somewhat redundant for this provider — but the provider still re-sends the local history regardless, so it's defensive against server-side garbage collection.

---

## 2. Struct + builder

```rust
pub struct OpenAIResponsesProvider {
    client: Client,
    api_key: String,
    base_url: String,
    last_response_id: Arc<Mutex<Option<String>>>,
}

impl OpenAIResponsesProvider {
    pub fn new(api_key: impl Into<String>) -> Self;
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self;
    fn model_id(model: &str) -> &str {
        model.strip_prefix("codex/").unwrap_or(model)
    }
}
```

No `with_strip_model_prefix` knob — the strip is hard-coded for `codex/`. There's only one variant (`OpenAIResponses`) so this is fine. No `with_api_key_header` — auth is always `Authorization: Bearer <key>`.

`last_response_id` is an `Arc<Mutex<Option<String>>>` so the provider can mutate it from inside the streaming `try_stream!` block (which doesn't have `&mut self`).

---

## 3. Request body construction

```rust
{
  "model": "<post-codex/-strip>",
  "input": [...],
  "stream": true,
  "instructions": "<system prompt>",
  "max_output_tokens": 1024,
  "previous_response_id": "resp_abc123",
  "tools": [{"type": "function", "name": ..., "description": ..., "parameters": ...}, ...]
}
```

### `messages_to_input`

Each `Message { role, content: Vec<ContentBlock> }` is decomposed per-block into the `input` array (one entry per block, NOT one entry per message — Responses' input is item-stream, not message-stream).

| `ContentBlock` | Becomes |
|---|---|
| `Text { text }` | `{role, content: text}` |
| `Thinking { .. }` | (dropped — Responses-API thinking models would use a different `{type: "reasoning", ...}` block; not wired yet) |
| `Image { .. }` | (dropped — `input_image` content block not wired yet; remains in local history for future vision-provider turns) |
| `ToolUse { id, name, input }` | `{type: "function_call", call_id: id, name, arguments: JSON.stringify(input)}` |
| `ToolResult { tool_use_id, content, .. }` | `{type: "function_call_output", call_id: tool_use_id, output: content}` |

Tool calls use `call_id` (Responses convention), not `id` like Chat Completions or `tool_use_id` like Anthropic. The mapping is the same identifier flowing under a different field name.

### `instructions` instead of system message

```rust
if let Some(sys) = &req.system {
    if !sys.is_empty() {
        body["instructions"] = json!(sys);
    }
}
```

Responses' equivalent of Anthropic's top-level `system`. NOT prepended to `input` like Chat Completions does for `messages[0].role = "system"`.

### `previous_response_id`

```rust
if let Ok(guard) = self.last_response_id.lock() {
    if let Some(ref id) = *guard {
        body["previous_response_id"] = json!(id);
    }
}
```

Always sent when present. Captured at end of stream from `response.completed`/`response.created`/etc. events. Persists across multiple `stream()` calls on the same `OpenAIResponsesProvider` instance.

The agent loop's session swap creates a new provider instance per session-with-different-model, so `last_response_id` resets correctly when the user switches sessions. Within a session, sequential turns share the same provider and chain via `previous_response_id`.

### Tools

```rust
{"type": "function", "name": ..., "description": ..., "parameters": ...}
```

Flat shape — no nested `function` object. Same `parameters` JSON schema content.

---

## 4. Stream pipeline

```rust
async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
    let body = self.build_body(&req);
    let resp = self.client.post(&self.base_url)
        .header("authorization", format!("Bearer {}", self.api_key))
        .header("content-type", "application/json")
        .json(&body)
        .send().await?;
    if !resp.status().is_success() { return Err(...); }   // body redacted

    let id_slot = Arc::new(Mutex::new(None::<String>));         // local capture
    let id_slot_stream = id_slot.clone();
    let id_slot_final = id_slot.clone();
    let id_store = self.last_response_id.clone();               // shared persist target

    Ok(Box::pin(try_stream! {
        let mut buffer = String::new();
        let mut seen_start = false;
        while let Some(chunk) = byte_stream.next().await {
            buffer.push_str(&String::from_utf8_lossy(&chunk?));
            while let Some(boundary) = buffer.find("\n\n") {
                let event_text: String = buffer.drain(..boundary + 2).collect();
                let events = parse_response_event(&event_text, &mut seen_start, &id_slot_stream)?;
                for ev in events {
                    if let ProviderEvent::TextDelta(ref s) = ev { raw.push(s); }
                    yield ev;
                }
            }
        }
        // Copy the captured response ID to the provider's persistent store
        if let Ok(captured) = id_slot_final.lock() {
            if let Some(ref id) = *captured {
                if let Ok(mut store) = id_store.lock() { *store = Some(id.clone()); }
            }
        }
    }))
}
```

The double-Arc dance (local `id_slot` captured by the parser; copied to provider-level `last_response_id` only at end of stream) is to avoid mutating the persistent store mid-stream — if the stream errors before completion, we don't want to chain a half-baked response on the next turn. End-of-stream success → commit; mid-stream failure → leave the prior `last_response_id` intact so retry chains from the last good state.

---

## 5. SSE parsing (`parse_response_event`)

Responses uses **typed `event:` lines** (Anthropic also has them; Chat Completions doesn't):

```
event: response.created
data: {"id":"resp_abc","model":"gpt-5.2-codex","status":"in_progress"}

event: response.output_text.delta
data: {"type":"response.output_text.delta","delta":"Hello"}

event: response.output_item.added
data: {"item":{"type":"function_call","call_id":"call_1","name":"Read"}}

event: response.function_call_arguments.delta
data: {"delta":"{\"path\":"}

event: response.function_call_arguments.done
data: {"arguments":"{\"path\":\"/tmp\"}"}

event: response.completed
data: {"response":{"id":"resp_abc","status":"completed","usage":{"input_tokens":20,"output_tokens":10}}}
```

### Event-name → `ProviderEvent` mapping

| `event:` line | Mapped to |
|---|---|
| First event parsed (any) | `MessageStart { model: data.response.model OR data.model }` (gated by `seen_start`) |
| `response.created` / `response.in_progress` | (captures `data.id` into `id_slot`) |
| `response.output_text.delta` | `TextDelta(data.delta)` |
| `response.output_item.added` w/ `item.type == "function_call"` | `ToolUseStart { id: item.call_id, name: item.name }` |
| `response.function_call_arguments.delta` | `ToolUseDelta { partial_json: data.delta }` |
| `response.function_call_arguments.done` | `ContentBlockStop` |
| `response.output_text.done` | (no event — text doesn't get a structural close marker downstream) |
| `response.completed` | `MessageStop { stop_reason: data.response.status, usage: parsed }` |
| anything else (incl. `[DONE]`) | (ignored) |

`response.created` carries the response id in `data.id` (top-level); other events carry it in `data.response.id`. Both are checked.

### Usage shape

```json
"usage": {
    "input_tokens": 20,
    "output_tokens": 10
}
```

No cache fields (Responses doesn't surface them, even though server-side history continuation is technically a form of cache).

---

## 6. `list_models`

```rust
async fn list_models(&self) -> Result<Vec<ModelInfo>> {
    let models_url = self.base_url.replace("/responses", "/models");
    // GET → JSON {data: [{id, ...}]} → ModelInfo
}
```

URL transform: replaces `/responses` with `/models`. For default URL: `https://api.openai.com/v1/models` (the same models endpoint as Chat Completions — both APIs share the model catalogue). `display_name` is not captured (Responses' `/v1/models` doesn't return it).

---

## 7. Testing

`openai_responses::tests` — 4 tests covering parser:
- `text_delta_emits_message_start_and_text` — `response.created` → `MessageStart`; subsequent deltas → `TextDelta`; `response.completed` → `MessageStop`
- `function_call_emits_tool_use_events` — full tool-call sequence: `output_item.added` → `ToolUseStart`; deltas → `ToolUseDelta`; `arguments.done` → `ContentBlockStop`; `completed` → `MessageStop`
- `response_id_captured` — `response.created` event populates `id_slot`
- `model_id_strips_prefix` — `codex/gpt-5.2-codex` → `gpt-5.2-codex`; `gpt-5.2-codex` passes through

No end-to-end mock-server test (yet) — the parser tests cover the wire shape, and the streaming pipeline is identical structure to the Anthropic provider (which IS mock-tested). If parser tests pass and the wire shape doesn't change, the pipeline works.

---

## 8. Notable behaviors / gotchas

- **Server-side history is one-way.** Once you send `previous_response_id`, the server includes the prior assistant output. There's no API to inspect what the server thinks the conversation contains — if the local history and server-side state diverge (e.g. user edited a prior message), the provider will silently send a request the server thinks contradicts what it has.
- **`Thinking` blocks are dropped.** Responses-API thinking models exist (gpt-5-thinking, o-series) but use a different `reasoning` block shape that isn't wired yet. `ContentBlock::Thinking` round-trips locally (assembler captures it) but doesn't make it into the wire request — the model doesn't see its own prior reasoning. For now, treat reasoning models as either Anthropic (full thinking support) or DeepSeek/o-series via OpenRouter (Chat Completions `reasoning_content` path).
- **`Image` blocks are dropped.** Same status as Thinking — Responses supports `input_image` blocks but the conversion isn't wired.
- **No server-side history reset.** The provider holds `last_response_id` until the worker drops it (session swap → new provider instance). There's no `clear_history()` method on the provider; agent-level history clearing doesn't reach into the provider state. If you need to fork a Responses-based session without inheriting server state, mint a new provider (which `build_provider` does on every `/model` swap).
- **`max_output_tokens` is conditional on `req.max_tokens > 0`.** The `> 0` check exists to allow a "leave it to the server" mode — though `StreamRequest.max_tokens` is `u32` so it's always non-negative; in practice, `max_tokens = 0` from the agent loop is rare and would bypass the limit.
- **Auth is always `Authorization: Bearer`.** No header override. Azure-hosted OpenAI Responses (if/when it exists) would need a custom impl.

---

## 9. What's NOT supported

- **Built-in tools** (web_search, file_search, computer_use, etc.) — Responses' value-add over Chat Completions is server-side tool execution. This impl only does function calls (the agent dispatches tools locally). Wiring a `{type: "web_search"}` tool def is one-line, but the response handling needs new event types.
- **Reasoning blocks** — covered above.
- **Image input** — covered above.
- **Tool choice / parallel_tool_calls / etc. tuning knobs** — none exposed.
- **Response object fetch / cancel API** (`/v1/responses/{id}`, `/v1/responses/{id}/cancel`) — would let the agent retrieve a response after stream disconnect or cancel a long-running thinking generation. Not wired.
