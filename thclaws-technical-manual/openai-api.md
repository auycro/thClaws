# OpenAI-compatible API

`thclaws --serve` exposes an OpenAI Chat Completions–compatible API at
`/v1/*` alongside the existing webapp. Any client that speaks the
OpenAI ecosystem can drive thClaws: openai-python, LiteLLM, LangChain,
Cursor's custom OpenAI provider, Aider, n8n, Open WebUI, etc.

The webapp + WebSocket are unchanged; the OpenAI surface is additive.

Source: [`crates/core/src/api_v1/`](../crates/core/src/api_v1/) +
the Router merge in [`server.rs`](../crates/core/src/server.rs).
Companion smoke tests: [`tests/openai_compat/`](../tests/openai_compat/).

## Quick start

```sh
# 1. Start the server with an API token
THCLAWS_API_TOKEN=secret-pick-your-own \
  thclaws --serve --bind 127.0.0.1 --port 7878

# 2. From any OpenAI-compatible client:
curl -H "Authorization: Bearer secret-pick-your-own" \
     -H "Content-Type: application/json" \
     -d '{
       "model": "claude-haiku-4-5",
       "messages": [{"role": "user", "content": "Reply with OK"}]
     }' \
  http://127.0.0.1:7878/v1/chat/completions
```

The server still needs a real LLM key in its environment (e.g.
`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`) to talk to the upstream model —
it's the operator's responsibility to provide that, same as for
`thclaws -p` print mode.

## Endpoints

### `GET /v1/models`

List every model id thClaws knows how to route to, sourced from the
embedded model catalogue (no network call).

```sh
curl -H "Authorization: Bearer $TOKEN" http://localhost:7878/v1/models
```

```json
{
  "object": "list",
  "data": [
    { "id": "claude-haiku-4-5", "object": "model",
      "created": 1747449617, "owned_by": "anthropic" },
    { "id": "gpt-5.4", "object": "model",
      "created": 1747449617, "owned_by": "openai" }
  ]
}
```

`owned_by` is the provider name as the catalogue records it
(`anthropic`, `openai`, `openrouter`, `gemini`, `dashscope`, etc).
Non-chat models (embeddings, audio, image-only) are filtered out so the
list matches what `/v1/chat/completions` can actually serve.

### `POST /v1/chat/completions`

OpenAI Chat Completions. Supports both non-streaming (returns JSON) and
streaming (`stream: true` → Server-Sent Events).

**Honored request fields**

| Field | Required | Notes |
|---|---|---|
| `model` | yes | Routed via `ProviderKind::detect` |
| `messages` | yes | Last `user`-role message becomes the turn prompt; everything before is history; `system` messages append to thClaws's default system prompt |
| `stream` | no | `false` → JSON, `true` → SSE |
| `temperature`, `top_p`, `max_tokens`, `stop` | no | Forwarded to the underlying model when supported |
| `user` | no | Logged for audit |

**Silently ignored** (matches OpenAI's tolerance for unknown fields):
`tools`, `tool_choice`, `response_format`, `seed`, `logit_bias`,
`logprobs`, `top_logprobs`, `presence_penalty`, `frequency_penalty`,
`n`, `stream_options`.

**Non-stream response**

```json
{
  "id": "chatcmpl-thc-6a09...",
  "object": "chat.completion",
  "created": 1747449617,
  "model": "claude-haiku-4-5",
  "choices": [
    {
      "index": 0,
      "message": { "role": "assistant", "content": "OK" },
      "finish_reason": "stop"
    }
  ],
  "usage": {
    "prompt_tokens": 328,
    "completion_tokens": 4,
    "total_tokens": 332
  }
}
```

**Stream response (SSE)**

```
data: {"id":"chatcmpl-thc-...","object":"chat.completion.chunk","created":...,
       "model":"...","choices":[{"index":0,"delta":{"role":"assistant"},
       "finish_reason":null}]}

data: {... ,"choices":[{"index":0,"delta":{"content":"Looking at "},
       "finish_reason":null}]}

data: {... ,"choices":[{"index":0,"delta":{"content":"your project..."},
       "finish_reason":null}]}

data: {... ,"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
       "usage":{"prompt_tokens":1234,"completion_tokens":567,
                "total_tokens":1801}}

data: [DONE]
```

A `:keepalive` SSE comment is sent every 15s during long agent thinks
so HTTP proxies don't drop the connection. Spec-compliant SSE parsers
(openai-python, LiteLLM) ignore comments automatically.

### Tool-use events (extension)

When the agent invokes one of its internal tools (Bash, Read, Write,
KMS, etc.) during a streaming response, thClaws emits an
`x_thclaws_tool_use` field on otherwise-empty chunks:

```json
{
  "id": "...", "object": "chat.completion.chunk", "created": ...,
  "model": "...",
  "choices": [{"index": 0, "delta": {}, "finish_reason": null}],
  "x_thclaws_tool_use": {
    "id": "tu_abc",
    "name": "Bash",
    "status": "started",
    "input": {"command": "echo hi"}
  }
}
```

Status progression:

| `status` | Fields | When |
|---|---|---|
| `started` | `id`, `name`, `status`, `input` | Agent decided to run the tool |
| `completed` | `id`, `name`, `status`, `output` | Tool returned successfully |
| `error` | `id`, `name`, `status`, `output` | Tool returned an error |
| `denied` | `id`, `name`, `status` | An `ApprovalSink` rejected the call |

`output` is `{preview, truncated, total_chars}` — preview is the first
~400 chars (boundary-safe for UTF-8), `total_chars` is the full byte
count, `truncated` is `true` when preview was cut. Clients that need
the full output should re-run the tool directly or call
`/v1/chat/completions` non-stream.

`x_thclaws_tool_use` is non-standard OpenAI. Strict clients that parse
only documented fields ignore it cleanly; aware clients render the
tool-call timeline live.

## Authentication

Auth is controlled by the `THCLAWS_API_TOKEN` environment variable on
the server, with three modes:

| `THCLAWS_API_TOKEN` value | API state |
|---|---|
| unset or empty | `/v1/*` returns 404 — API disabled |
| `<value>` | Every `/v1/*` request must carry `Authorization: Bearer <value>` |
| literal string `disable-auth` | No auth check. **Refused** unless the listener is loopback-bound — startup errors otherwise |

Tokens are compared in constant time so timing-based extraction
attempts don't leak partial matches.

For SaaS / multi-tenant deployments: mint a unique token per tenant
pod and pass it as `Authorization: Bearer <token>` from the consumer.
Token rotation = restart the pod with a new env value.

## Error responses

All errors use the OpenAI envelope shape:

```json
{
  "error": {
    "message": "Invalid API key. Set THCLAWS_API_TOKEN on the server, then send it as `Authorization: Bearer <token>`.",
    "type": "invalid_request_error",
    "code": "invalid_api_key"
  }
}
```

| HTTP | `error.type` | `error.code` | When |
|---|---|---|---|
| 400 | `invalid_request_error` | `invalid_messages` | `messages` array empty or no user message |
| 401 | `invalid_request_error` | `invalid_api_key` | Missing / wrong Bearer token |
| 404 | (text body) | — | `THCLAWS_API_TOKEN` unset on server |
| 500 | `server_error` | `internal_error` | Upstream provider, network, or tool failure |

Errors that happen MID-SSE-STREAM (after headers flush) can't return a
new HTTP status. Instead the stream emits a final content chunk
prefixed with `[thclaws error] <message>` and a `finish_reason: "error"`
terminal chunk, then `[DONE]`. Clients see the error inline.

## Working directory

`thclaws --serve` runs against a single working directory (the process
cwd, or `--workspace <dir>` if specified). Every chat request uses the
same directory — file edits made by tool calls in one request are
visible to the next request's tool calls. The pod's filesystem **is**
the long-lived agent memory; the OpenAI API itself is stateless per
the standard `messages: [...]` convention.

For multi-tenant SaaS where each customer should have isolated state,
run one `thclaws --serve` per tenant (one container each), with the
customer's working directory mounted as a volume.

## Client examples

### openai-python SDK

```python
from openai import OpenAI

client = OpenAI(
    base_url="http://localhost:7878/v1",
    api_key="secret-pick-your-own",
)

# Non-stream
resp = client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Hello"}],
)
print(resp.choices[0].message.content)

# Streaming
for chunk in client.chat.completions.create(
    model="claude-haiku-4-5",
    messages=[{"role": "user", "content": "Tell a joke"}],
    stream=True,
):
    print(chunk.choices[0].delta.content or "", end="", flush=True)
```

### LiteLLM

```python
import litellm

resp = litellm.completion(
    model="openai/claude-haiku-4-5",   # `openai/` prefix uses OpenAI-compat client
    messages=[{"role": "user", "content": "Hello"}],
    api_base="http://localhost:7878/v1",
    api_key="secret-pick-your-own",
)
print(resp.choices[0].message.content)
```

### Aider

```sh
aider --openai-api-base http://localhost:7878/v1 \
      --openai-api-key secret-pick-your-own \
      --model openai/claude-haiku-4-5 \
      file.py
```

### Cursor

Settings → Models → Override OpenAI Base URL = `http://localhost:7878/v1`,
API Key = your `THCLAWS_API_TOKEN`. Models in the dropdown match what
`/v1/models` returns.

## Limits and non-goals

This is **Chat Completions only** — by design.

| Endpoint | Status |
|---|---|
| `POST /v1/chat/completions` | ✅ |
| `GET /v1/models` | ✅ |
| `POST /v1/embeddings` | ❌ Not planned. Use the underlying provider directly. |
| `POST /v1/audio/*` | ❌ Not planned. |
| `POST /v1/images/*` | ❌ Not planned. |
| Assistants v2 | ❌ Not planned — thClaws's agent runtime IS the assistant. |
| Batch / fine-tuning | ❌ Not planned. |
| Client-driven function calling (`tools`/`tool_choice`) | ❌ Internal tools only. thClaws decides when to call them; clients see results via the `x_thclaws_tool_use` extension. |
| `n != 1` multi-response | ❌ Returns 400. |

## Troubleshooting

| Symptom | Fix |
|---|---|
| `404` on `/v1/*` | `THCLAWS_API_TOKEN` not set on server |
| `401 invalid_api_key` | Bearer header missing or doesn't match `THCLAWS_API_TOKEN` |
| Server refuses to start with "THCLAWS_API_TOKEN=disable-auth is only allowed on a loopback bind" | Bind to `127.0.0.1` or set a real token |
| Streaming returns 200 but no content | Check `ANTHROPIC_API_KEY` / equivalent is set in the server's env — without it, the upstream LLM call fails and you'll see `[thclaws error]` in the stream |
| Long agent runs time out before completion | Increase the timeout on your HTTP client (openai-python defaults to 600s, often enough; LiteLLM has its own knobs) |

## See also

- [`serve-mode.md`](serve-mode.md) — what `--serve` actually does;
  the trust model the OpenAI endpoints inherit.
- [`docker.md`](docker.md) — container packaging for `thclaws --serve`;
  the OpenAI endpoints work identically inside the container.
- [`paperclip-adapter.md`](paperclip-adapter.md) — for the
  Paperclip-specific integration path (an alternative to driving
  thClaws via the OpenAI API).
