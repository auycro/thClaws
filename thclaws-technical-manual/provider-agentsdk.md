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

Wrapping it as a Provider lets thClaws invoke Claude Code as a single backend without re-implementing any of that. Pre-v0.9.6, *everything* between thClaws's user input and Claude Code's tools happened inside the `claude` subprocess — thClaws's own tool registry didn't dispatch anything for `agent/` model turns, which made KMS / Memory / MCP / Plan / Team tools unreachable from `agent/*` models.

**v0.9.6 added an in-process MCP bridge** (see §5b). thClaws now sends its own tools to Claude Code as `mcp__thclaws__<name>` entries via the Agent SDK's `mcp_message` control_request channel; dispatch happens back in the host process where the tools, hooks, and approval gate live. The user gets subscription-billed Claude Code + the full thClaws tool registry — both at once, no compromise.

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

### Stage 4: Keep stdin open, stream stdout, service MCP requests

```rust
// stdin stays alive — bridge tools may need to round-trip with claude.
```

**v0.9.6 change:** stdin is **not** closed after the user envelope. The MCP bridge (see §5b) needs a bidirectional channel — Claude Code sends `control_request { subtype: "mcp_message" }` frames at any point during the turn whenever the model invokes a `mcp__thclaws__<name>` tool, and we must write a `control_response` back on stdin before the model can continue. The CLI commits the session file on EOF, which we send after `{"type": "result"}` lands.

Then loop reading stdout until either `{"type": "result"}` (terminal) or EOF — servicing `mcp_message` frames mid-stream as they arrive.

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
    "control_request" => {
        // Two subtypes today:
        //   "mcp_message" — bridge dispatch. Handled by §5b.
        //   anything else — no-op (permission prompts shouldn't fire
        //                   under bypassPermissions).
    }
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

## 5b. MCP bridge (v0.9.6)

**Problem solved:** earlier AgentSdk turns ran Claude Code's built-in toolset only — thClaws's KMS / Memory / MCP-contributed / Plan / Side-channel tools were unreachable from `agent/*` models because the tool registry didn't cross the subprocess boundary. Switching to `claude-*` to use those tools meant losing the subscription billing AgentSdk provides.

**Mechanism:** an **in-process SDK MCP server** (`crate::sdk_mcp`, ~250 LOC) wraps the thClaws `ToolRegistry` and surfaces tools to Claude Code as `mcp__thclaws__<name>`. Claude Code dispatches them as it would any MCP tool — but the wire path is the existing Agent SDK `control_request { subtype: "mcp_message" }` channel, not a separate stdio MCP server. No second subprocess, no socket wiring.

### CLI invocation deltas

```rust
// agent_sdk.rs:164-174
let mcp_config = crate::sdk_mcp::mcp_config_value();
cmd.arg("--mcp-config").arg(mcp_config.to_string());
let patterns = crate::sdk_mcp::allowed_tool_patterns(tools);
cmd.arg("--allowedTools").arg(patterns.join(","));
```

- `--mcp-config <json>` — declares a single MCP server named `thclaws` of type `sdk` (in-process). Claude Code recognises this as a host-callback server and routes calls back on stdin.
- `--allowedTools mcp__thclaws__Read,mcp__thclaws__Edit,…` — the explicit allow-list of bridged tool names. **No Claude-built-in tools are in the list.** The previous "allow everything" posture is gone; the model only sees what we send.

```rust
pub const SERVER_NAME: &str = "thclaws";
pub const PROTOCOL_VERSION: &str = "2024-11-05";

pub fn bridged_tool_names(registry: &ToolRegistry) -> Vec<String>;
pub fn allowed_tool_patterns(registry: &ToolRegistry) -> Vec<String>;
pub fn mcp_config_value() -> Value;
pub async fn handle_mcp_message(registry: Arc<ToolRegistry>, msg: &Value) -> Value;
```

### Wire protocol

Three JSON-RPC methods, identical shape to what `claude-agent-sdk-python` implements (`_handle_sdk_mcp_request` in the upstream SDK):

| Method | Direction | Body |
|---|---|---|
| `initialize` | CLI → host | `{}` |
| `tools/list` | CLI → host | `{}` |
| `tools/call` | CLI → host | `{ "name": "Read", "arguments": { ... } }` |

The host's response shape:

```json
// initialize
{ "result": { "protocolVersion": "2024-11-05", "serverInfo": { "name": "thclaws", "version": "0.9.6" } } }

// tools/list
{ "result": { "tools": [{ "name": "Read", "description": "...", "inputSchema": { ... } }, ...] } }

// tools/call (success)
{ "result": { "content": [{ "type": "text", "text": "<tool output>" }] } }

// tools/call (error)
{ "result": { "content": [{ "type": "text", "text": "<error msg>" }], "isError": true } }
```

### Read loop integration

```rust
// agent_sdk.rs:390+
"control_request" => {
    let subtype = v.pointer("/request/subtype").and_then(|x| x.as_str()).unwrap_or("");
    if subtype == "mcp_message" {
        let server_name = v.pointer("/request/server_name").and_then(|x| x.as_str()).unwrap_or("");
        if server_name == crate::sdk_mcp::SERVER_NAME {
            let mcp_msg = v.pointer("/request/message").cloned().unwrap_or(Value::Null);
            let mcp_resp = match &bridge_tools {
                Some(reg) => crate::sdk_mcp::handle_mcp_message(reg.clone(), &mcp_msg).await,
                None => /* registry-missing error */,
            };
            // Write control_response back on stdin
            let response = json!({
                "type": "control_response",
                "response": { "request_id": v.pointer("/request_id"), "subtype": "success",
                              "response": { "mcp_response": mcp_resp } }
            });
            stdin.write_all(format!("{response}\n").as_bytes()).await?;
        }
    }
}
```

### Tool filter

`sdk_mcp::bridged_tool_names` excludes tools that depend on parent-process state Claude Code can't model:

- **Task** — recursive subagent spawner; would dispatch on the host but the model thinks it's local.
- **Team\*** — multi-process teammate orchestration via `.thclaws/team/`; doesn't fit a single-shot turn.
- **Skill** — rewrites the next turn at the host level; Claude Code's loop doesn't honor the rewrite.
- **EnterPlanMode / ExitPlanMode / SubmitPlan / UpdatePlanStep** — plan-mode state machine the host owns.
- **AskUserQuestion** — needs the GUI question modal; no surface inside the SDK subprocess.

Everything else (Bash, Read, Edit, Write, KMS\*, Memory\*, MCP-contributed tools, TodoWrite, the Web\* family, ResearchKickoff, etc.) is bridged. The list is computed per-turn from the live registry, so MCP-contributed tools that landed via `/mcp install` mid-session are reachable on the next AgentSdk turn without restart.

### Tradeoff captured in the design

The bridge keeps AgentSdk's subscription-billing benefit while restoring tool parity with `claude-*`. The cost is two stdin/stdout round-trips per tool call instead of one in-process function call — measurable for tool-heavy turns but negligible for chat-paced use.

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
- **Tool calls are partially opaque.** For thClaws-bridged tools (`mcp__thclaws__*`), the host runs the call in-process — same hooks, same approval gate, same on-disk state. For any Claude-built-in tool that might fire under the bridged registry's gaps (none currently exposed since `--allowedTools` is restricted to the bridge set), thClaws sees `tool_use` blocks as dim `🔧 [name]` markers without inspecting input. Pre-v0.9.6 this was the only path; post-v0.9.6 it's the residual path for any tool we deliberately keep out of the bridge.
- **`session_id` persists across `/load` (post-fix).** Pre-fix, the captured Claude-side UUID lived only in `Arc<Mutex<Option<String>>>` on the provider instance — when a thClaws session was reopened the provider got `None`, never passed `--resume <uuid>`, and the SDK started a brand-new conversation that saw only the latest user message. The model appeared to forget every prior turn. The fix added a `provider_state` JSONL event ([`sessions.md`](sessions.md#provider-state-event)) capturing the UUID after every turn that surfaced a new id, plus `Provider::provider_session_id` / `set_provider_session_id` trait methods so the worker can read/write the slot. `save_history` in `shared_session` writes the event when the value changes; the GUI `/load` and CLI `/resume` paths call `agent.provider().set_provider_session_id(loaded.provider_session_id.clone())` BEFORE swapping `state.session` so the next `stream()` passes `--resume <uuid>` and the SDK restores its server-side history. To start a fresh SDK conversation explicitly, swap the model away and back (`/model claude-sonnet-4-6` then `/model agent/claude-sonnet-4-6`) — `build_provider` constructs a fresh `AgentSdkProvider` with `None` session id and the next save writes a `provider_state: null` event.
- **Stream-end without `result`.** The defensive MessageStop covers crash / kill scenarios. If the CLI exits cleanly without `result` (e.g. user revoked subscription mid-turn), the agent gets `MessageStop { usage: None }` which renders as "0 tokens" but otherwise works.
- **`CLAUDE_BIN` env override.** Useful for testing against a local debug build of `claude`, or pointing at a versioned binary (`/usr/local/bin/claude-1.5.0`). The default `claude` resolves via `PATH`.
- **30-second initialize timeout.** Conservative — most invocations complete in <1s. Triggers when the binary is wrong (not on PATH), too old to recognize the SDK protocol, or hanging on a login flow.
- **No `with_base_url`.** This isn't HTTP. The "endpoint" is the binary path.

---

## 9. What's NOT supported

- **Bidirectional permission prompts / user-defined hooks.** stdin now stays open (for the MCP bridge — see §5b), so this is technically wireable. Today only `control_request { subtype: "mcp_message" }` is serviced; permission prompts are still suppressed via `bypassPermissions`, and user-defined hook callbacks aren't routed. Adding either is a straightforward arm in the §5b read loop.
- ~~SDK MCP servers~~ — shipped in v0.9.6. See §5b.
- **Custom CLI flags.** `--output-format`, `--input-format`, `--verbose`, `--permission-mode`, `--system-prompt`, `--model`, `--resume` are the only flags set. Power-user flags (`--mcp`, `--no-bundled-prompt`, `--allowed-tools`, etc.) would need provider-level wiring.
- **Multi-turn within one subprocess.** Each `stream()` call spawns a new `claude` process. The CLI supports multi-turn within one stdin/stdout stream; thClaws doesn't take advantage. Adds latency but simplifies state management.
- **Concurrent turns.** The `Arc<Mutex<Option<String>>>` session_id is single-slot — running two `stream()` calls concurrently would race on it. The agent loop is single-threaded per session so this isn't a concrete bug today.
- **Image / multimodal input.** The user envelope sends `content: <user_text>` (a string); image blocks in history are silently dropped.
- **Thinking blocks.** `Thinking { content }` from local history isn't propagated — Claude Code holds its own thinking state server-side.
