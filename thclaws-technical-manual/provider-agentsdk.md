# Anthropic Agent SDK (Subprocess) provider

`AgentSdkProvider` (`providers/agent_sdk.rs`, 392 LOC) is the only non-HTTP provider in the catalogue. It wraps the `claude` CLI binary as a subprocess and speaks the [Claude Agent SDK control protocol](https://github.com/anthropics/claude-agent-sdk-python) over stdin/stdout JSON-RPC.

One `ProviderKind` variant uses this impl: `AgentSdk`. Routing prefix: `agent/` (e.g. `agent/claude-sonnet-4-6`). **No `ANTHROPIC_API_KEY` required** — billing goes through the user's Claude subscription via the `claude` CLI's own auth.

**Source:** `crates/core/src/providers/agent_sdk.rs`
**Dependencies:**
- The `claude` CLI binary on `PATH` (override via `CLAUDE_BIN` env)

**Cross-references:**
- [`providers.md`](providers.md) — `Provider` trait, `StreamRequest`, `ProviderEvent`
- [`provider-anthropic.md`](provider-anthropic.md) — `list_models` falls back to Anthropic's API for the model catalogue

---

## 1. Why a subprocess provider?

The `claude` CLI ships with:
- Built-in MCP server registry, agent definitions, hooks, slash commands
- Server-side conversation state (sessions persist across CLI invocations)
- All the per-tool execution Claude Code already does (Read, Write, Bash, Grep, etc.)
- The user's existing Claude subscription (no separate API billing)

Wrapping it as a Provider lets thClaws invoke Claude Code as a single backend without re-implementing any of that. Everything between thClaws's user input and Claude Code's tools happens inside the `claude` subprocess — thClaws's own tool registry doesn't dispatch anything for `agent/` model turns.

The trade-off: the subprocess cycle is heavier than HTTP (spawn `claude` per turn), and stdin/stdout JSON framing is more fragile than SSE. But for users who want Claude Code's full feature set inside thClaws's UI, it's the right hatch.

---

## 2. Struct + builder

```rust
pub struct AgentSdkProvider {
    claude_bin: String,                        // path to `claude` CLI, default "claude"
    session_id: Arc<Mutex<Option<String>>>,    // captured from CLI for next-turn --resume
    next_req: Arc<Mutex<u64>>,                 // monotonic counter for control_request ids
}

impl AgentSdkProvider {
    pub fn new() -> Self {
        let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        ...
    }
    pub fn with_bin(mut self, bin: impl Into<String>) -> Self;
    fn next_request_id(&self) -> String;        // "req_{counter}_{nanos:08x}"
}
```

`session_id` is `Arc<Mutex>` so the streaming task can mutate it without `&mut self`. Captured the first time the CLI emits `session_id` in any frame, used on subsequent turns via `--resume <uuid>`.

---

## 3. CLI invocation

```bash
claude --output-format stream-json \
       --input-format stream-json \
       --verbose \
       --permission-mode bypassPermissions \
       --system-prompt <sys> \
       [--model <m>] \
       [--resume <sid>]
```

Flags:
- `--output-format stream-json` — newline-delimited JSON on stdout (NOT SSE). Each line is a complete JSON event.
- `--input-format stream-json` — accept newline-delimited JSON on stdin.
- `--verbose` — required for the SDK protocol; enables emission of system / progress events.
- `--permission-mode bypassPermissions` — Claude Code normally prompts for tool permission via the CLI; we bypass because thClaws is the user-facing surface (and thClaws's own approval gate runs at the agent loop level for non-AgentSdk providers; AgentSdk currently bypasses both layers, see §6 limitations).
- `--system-prompt <sys>` — ALWAYS set explicitly. Empty string suppresses Claude Code's bundled system prompt so the model sees only thClaws's. Non-empty replaces.
- `--model <m>` — optional. The `agent/` prefix is stripped first; if anything remains, it's passed.
- `--resume <sid>` — passed when `session_id` slot is `Some`. Reattaches to the existing CLI-side session. **NOT `--session-id`** — that flag is for *setting* a new session's id and errors with "Session ID is already in use" if re-passed on turn 2.

### Env hygiene

Mirrors the Python SDK's:
- `CLAUDE_CODE_ENTRYPOINT=sdk-thclaws` — identifies thClaws as the integrator
- `CLAUDECODE` env var REMOVED — prevents the child from thinking it's nested inside another Claude Code session (the parent process may be Claude Code itself, e.g. when developing thClaws via Claude Code)

---

## 4. The 4-stage protocol

Every `stream()` call goes through:

### Stage 1: Send `initialize` control_request

```json
{"type":"control_request","request_id":"req_1_a1b2c3d4","request":{"subtype":"initialize","hooks":null}}
```

Followed by `\n`. Required FIRST — the CLI ignores user input until it sees an initialize.

### Stage 2: Wait for `control_response` matching that `request_id`

Loop reading stdout lines with a 30-second timeout. Skip empty lines, non-JSON lines, JSON lines whose `type != "control_response"`, and `control_response`s whose `response.request_id` doesn't match. Break on match.

```rust
let mut ack_line = String::new();
loop {
    ack_line.clear();
    let n = tokio::time::timeout(Duration::from_secs(30),
                                  reader.read_line(&mut ack_line)).await
        .map_err(|_| Error::Provider("timed out waiting for initialize response. \
                                      Is the claude CLI version current?"))?...?;
    if n == 0 { return Err("process exited before initialize response"); }
    // parse trimmed; check type and request_id
    if matched { break; }
}
```

The 30s timeout error message includes the `claude --version` hint because outdated CLIs are the most common cause.

### Stage 3: Send the user message

```json
{"type":"user","session_id":"","message":{"role":"user","content":"<user_text>"},"parent_tool_use_id":null}
```

`user_text` is extracted from `req.messages.last()`'s first `ContentBlock::Text`. **Prior history is NOT sent** — Claude Code remembers the conversation server-side under `--resume <sid>`. Only the new user message goes on the wire.

`session_id: ""` is the user envelope's id field; the `--resume <sid>` flag does the actual session tracking. The CLI accepts the empty string here as "use whatever session is active."

### Stage 4: Close stdin, stream stdout

```rust
drop(stdin);
```

We have no bidirectional hooks / SDK MCP servers, so closing stdin signals EOF and lets the CLI commit the session file cleanly once the turn finishes.

Then loop reading stdout until either `{"type": "result"}` (terminal) or EOF.

---

## 5. Stdout event mapping

```rust
match msg_type {
    "assistant" => {
        // /message/content is an array of typed blocks
        for block in blocks {
            match btype {
                "text" => yield TextDelta(text_or_text_with_leading_newlines),
                "tool_use" => yield TextDelta(format!("\n\x1b[2m🔧 [{name}]\x1b[0m\n")),
                _ => {}
            }
        }
    }
    "user" => {} // tool_result echoes — model already has them server-side
    "control_request" => {} // permission prompts (shouldn't fire with bypassPermissions)
    "control_response" => {}
    "result" => {
        let usage = parse_usage(&v);
        yield MessageStop { stop_reason: "end_turn", usage };
        break;
    }
    "system" | "rate_limit_event" | "keep_alive"
        | "stream_event" | "task_started" | "task_progress"
        | "task_notification" => {} // benign
    _ => {} // unknown — ignore defensively
}
```

Key choices:

- **First text block streams as-is; subsequent text blocks get `\n\n` prepended.** Claude Code emits ONE `assistant` message per reply (not per token), with content as an array of typed blocks. Multiple text blocks are unusual but possible (model emitted text → tool_use → text). Joining with `\n\n` keeps them visually distinct.
- **`tool_use` blocks render as a dim `🔧 [name]` marker, NOT as actual tool calls.** Claude Code dispatches the tool itself server-side and feeds the result back to the model. From thClaws's perspective, the conversation is opaque — we just see text streaming with occasional tool indicators.
- **`user` frames (tool_result echoes) are ignored.** Same reason — Claude Code already has them in its server-side history; surfacing them would double the noise.
- **`session_id` is captured** from any frame on every iteration. The check is unconditional (`if let Some(sid) = v.get("session_id") ...`) so the first frame carrying a session id wins, and subsequent frames overwrite (the CLI emits the same id throughout a turn).
- **Stream-end without `result` still emits MessageStop.** Defensive — if the CLI exits weirdly (crash, kill), the agent's turn doesn't hang waiting for a frame that never arrives.

### Usage shape (`result` frame)

```json
{
    "type": "result",
    "usage": {
        "input_tokens": 100,
        "output_tokens": 50,
        "cache_creation_input_tokens": 1000,
        "cache_read_input_tokens": 500
    }
}
```

Cache fields ARE captured — Claude Code surfaces Anthropic's prompt caching counters even though thClaws didn't manage the cache directly.

---

## 6. Stderr piping

```rust
if let Some(stderr) = stderr {
    tokio::spawn(async move {
        let mut reader = BufReader::new(stderr);
        let mut line = String::new();
        while let Ok(n) = reader.read_line(&mut line).await {
            if n == 0 { break; }
            eprint!("\x1b[2m[claude] {}\x1b[0m", line);
            line.clear();
        }
    });
}
```

Child stderr is piped to thClaws's own stderr in real time, dim-formatted. Surfaces:
- Outdated CLI version warnings
- Auth/login prompts
- MCP server start-up messages
- Anything else the CLI logs

Important: stderr lines are NOT routed through the GUI — they only appear in the terminal where thClaws was launched. If you launch the GUI from a desktop launcher with no terminal, you won't see them.

---

## 7. `list_models`

```rust
async fn list_models(&self) -> Result<Vec<ModelInfo>> {
    if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
        let anthropic = AnthropicProvider::new(api_key);
        let mut models = anthropic.list_models().await?;
        for m in &mut models {
            m.id = format!("agent/{}", m.id);
            if let Some(ref name) = m.display_name {
                m.display_name = Some(format!("{} (Agent SDK)", name));
            }
        }
        Ok(models)
    } else {
        Err(Error::Provider("set ANTHROPIC_API_KEY to list models ..."))
    }
}
```

Falls back to the Anthropic API for the catalogue. Re-prefixes ids with `agent/` and adds " (Agent SDK)" to display names. **Requires `ANTHROPIC_API_KEY` even though `stream()` doesn't** — it's the only way to get the model list. Users without the key can hard-code an `agent/<name>` model in settings and bypass `list_models` entirely.

---

## 8. Notable behaviors / gotchas

- **One subprocess per turn.** Heavy compared to HTTP. Spawn cost: ~50-150ms on a warm machine. Acceptable for chat-paced use, slow for batch.
- **Server-side history.** Claude Code's session file at `~/.claude/sessions/<uuid>.jsonl` is what holds the actual conversation. thClaws's local session JSONL ([`sessions.md`](sessions.md)) is parallel — both record the same turns but neither is authoritative. Switching `agent/` ↔ `claude-` providers within one thClaws session means the local history will replay against Anthropic's API on the non-AgentSdk turns, ignoring whatever Claude Code remembers.
- **`--permission-mode bypassPermissions`.** Disables Claude Code's per-tool permission prompts. Means: when an `agent/` turn runs Bash/Edit/Write, thClaws's approval gate ([`permissions.md`](permissions.md)) NEVER fires (the dispatch happens inside `claude`, not thClaws's tool registry), AND Claude Code's own approval gate is suppressed. The user gets no per-call approval signal for AgentSdk turns. If you want approval gating, use `claude-` (Anthropic provider direct) instead of `agent/`.
- **Tool calls are opaque.** thClaws sees `tool_use` blocks as dim `🔧 [name]` markers, doesn't get to inspect the input or veto. The actual tool result goes back to the model server-side. If a tool errors, thClaws sees nothing — Claude Code retries or surfaces the error in subsequent assistant text.
- **`session_id` resets only on provider rebuild.** A session swap in thClaws (`/new`, `/load`) doesn't reset the AgentSdk's session id (the provider is held by the worker). To start a fresh AgentSdk session, swap the model away and back (`/model claude-sonnet-4-6` then `/model agent/claude-sonnet-4-6`) — `build_provider` constructs a fresh `AgentSdkProvider` with `None` session id.
- **Stream-end without `result`.** The defensive MessageStop covers crash / kill scenarios. If the CLI exits cleanly without `result` (e.g. user revoked subscription mid-turn), the agent gets `MessageStop { usage: None }` which renders as "0 tokens" but otherwise works.
- **`CLAUDE_BIN` env override.** Useful for testing against a local debug build of `claude`, or pointing at a versioned binary (`/usr/local/bin/claude-1.5.0`). The default `claude` resolves via `PATH`.
- **30-second initialize timeout.** Conservative — most invocations complete in <1s. Triggers when the binary is wrong (not on PATH), too old to recognize the SDK protocol, or hanging on a login flow.
- **No `with_base_url`.** This isn't HTTP. The "endpoint" is the binary path.

---

## 9. What's NOT supported

- **Bidirectional hooks.** The protocol allows the CLI to call back into the host via `control_request` (e.g. for permission prompts, hook callbacks). thClaws acknowledges these as no-ops — bypassPermissions covers the permission case; user-defined hooks would require wiring stdin to stay open and a request-handler loop.
- **SDK MCP servers** (in-process MCP servers exposed to Claude Code via stdin). Closing stdin after the user message kills any chance of these.
- **Custom CLI flags.** `--output-format`, `--input-format`, `--verbose`, `--permission-mode`, `--system-prompt`, `--model`, `--resume` are the only flags set. Power-user flags (`--mcp`, `--no-bundled-prompt`, `--allowed-tools`, etc.) would need provider-level wiring.
- **Multi-turn within one subprocess.** Each `stream()` call spawns a new `claude` process. The CLI supports multi-turn within one stdin/stdout stream; thClaws doesn't take advantage. Adds latency but simplifies state management.
- **Concurrent turns.** The `Arc<Mutex<Option<String>>>` session_id is single-slot — running two `stream()` calls concurrently would race on it. The agent loop is single-threaded per session so this isn't a concrete bug today.
- **Image / multimodal input.** The user envelope sends `content: <user_text>` (a string); image blocks in history are silently dropped.
- **Thinking blocks.** `Thinking { content }` from local history isn't propagated — Claude Code holds its own thinking state server-side.
