# Agentic loop

The state machine that turns a user message into a streamed conversation with tool calls. Lives in [`crates/core/src/agent.rs`](../thclaws/crates/core/src/agent.rs) (`Agent::run_turn`); driven by [`shared_session.rs::drive_turn_stream`](../thclaws/crates/core/src/shared_session.rs) (GUI worker) or [`repl.rs::run_repl`](../thclaws/crates/core/src/repl.rs) (CLI). Both paths consume the same `Stream<Item = Result<AgentEvent>>` so the agent's behavior is identical regardless of surface.

This doc covers: the per-turn pipeline (system-prompt composition → history compaction → provider stream with retry → stream assembly → tool dispatch → loop or stop), the cancellation model, every stop condition, the parse-then-execute split for tool calls, plan-mode integration, the approval gate, MCP-Apps widget fetch, the truncation-to-disk path for oversized tool output, code organization, testing, and migration / known limitations.

**Source modules:**
- `crates/core/src/agent.rs` — `Agent`, `run_turn`, `run_turn_multipart`, `AgentEvent`, the per-turn loop body, `maybe_truncate_to_disk`
- `crates/core/src/providers/assemble.rs` — `assemble`, `AssembledEvent`, `BlockState`, `split_think_text`, `ToolParseFailed` recovery
- `crates/core/src/providers/mod.rs` — `Provider` trait, `ProviderEvent`, `StreamRequest`, `Usage`
- `crates/core/src/compaction.rs` — `compact`, `truncate_oversized_message`, `compact_with_summary`, `compact_for_step_boundary`, `clear_for_step_boundary`
- `crates/core/src/shared_session.rs` — worker loop, `WorkerState`, `drive_turn_stream`, cancel wiring
- `crates/core/src/cancel.rs` — `CancelToken` (M6.17 BUG H1/M3)
- `crates/core/src/permissions.rs` — `ApprovalSink`, `PermissionMode`, `current_mode`
- `crates/core/src/tools/mod.rs` — `Tool` trait, `ToolRegistry`, `call_multimodal`, `fetch_ui_resource`
- `crates/core/src/tools/plan_state.rs` — plan state read by the agent's per-tool gate

**Cross-references:**
- [`skills.md`](skills.md) — how the `Skill` tool fits into the dispatch (lazy body load + script auto-listing)
- [`mcp.md`](mcp.md) §7 — MCP-Apps widget fetch contract (the `fetch_ui_resource` step)
- [`commands.md`](commands.md) — `/<name>` slash resolution that produces the user message the loop runs against
- [`plugins.md`](plugins.md) — what `WorkerState.tool_registry` and `WorkerState.skill_store` get populated from at startup

---

## 1. Overview: where the loop runs

```
USER  →  composer text  →  ┌─────────────────────────────────────┐
                            │  ShellInput::Line / LineWithImages  │
                            │     (worker mpsc input channel)     │
                            └──────────────────┬──────────────────┘
                                               │
                               ┌───────────────┴───────────────┐
                               │  cancel.reset()               │
                               │  handle_line / _with_images   │
                               │  → drive_turn_stream(state.   │
                               │       agent.run_turn(prompt)) │
                               └───────────────┬───────────────┘
                                               │
                               ┌───────────────┴───────────────┐
                               │ Agent::run_turn (try_stream!) │
                               │   per-iteration:              │
                               │     compact(history)          │
                               │     provider.stream(req)      │
                               │       (with retry backoff)    │
                               │     assemble(raw)             │
                               │       → AssembledEvent stream │
                               │     tool dispatch loop        │
                               │       (approval, plan-mode,   │
                               │        execute, ui_resource)  │
                               │     yield AgentEvent::*       │
                               └───────────────┬───────────────┘
                                               │
                                               ▼
                          drive_turn_stream consumes AgentEvents:
                              Text → AssistantTextDelta IPC
                              ToolCallStart → ToolCallStart IPC
                              ToolCallResult → ToolCallResult IPC (+ ui_resource)
                              Done → TurnDone IPC + history persist
                              cancel.cancelled() → (interrupted) + return
```

Three surfaces, one engine:

| Surface | Drives the agent via | Notes |
|---|---|---|
| GUI Chat | `drive_turn_stream` (worker thread, single tokio runtime) | Both Chat and Terminal tabs share the same `WorkerState`; the event-translator fans out `ViewEvent`s into both Chat-shaped (`chat_*`) and Terminal-shaped (`terminal_data`) IPC dispatches |
| GUI Terminal | Same as above | Routes the same input through the worker; no separate agent state |
| CLI REPL | `repl.rs::run_repl`'s top-level loop | Calls `agent.run_turn(prompt)` directly; consumes the same `AgentEvent` stream with a different rendering pipeline (rustyline + ANSI directly) |

The **worker is single-threaded for loop dispatch** — only one `drive_turn_stream` runs at a time. Fresh user messages while a turn is in-flight queue in `input_rx`; M6.10 added the `streaming` flag so the chat composer's Stop button shows up while a turn is active, and pressing Enter mid-turn submits an input that gets queued (with a `[queued]` indicator surfaced to the user — M6.15 BUG 1 fix).

---

## 2. Entry points

### `Agent::run_turn(user_msg: String) -> Stream<Item = Result<AgentEvent>>`

Wraps `user_msg` as a single `ContentBlock::Text` and delegates to `run_turn_multipart`. The text-only entry point most callers use.

### `Agent::run_turn_multipart(user_content: Vec<ContentBlock>) -> Stream<...>`

Accepts an arbitrary list of content blocks for the user turn. Used by the GUI chat composer to ship a message with image attachments alongside text (Phase 4 paste/drag → `ContentBlock::Image` + `ContentBlock::Text` in the same `user` message). Identical loop semantics; the only difference is what gets pushed to history at turn start.

### `WorkerState.agent` — pre-built, reused across turns

`Agent::new(provider, tools, model, system).with_approver(...).with_cancel(...)` is constructed once at worker spawn. Re-built on `/model`, `/provider`, `/mcp add`, `/kms use`, `/permissions` swap, etc. via `WorkerState::rebuild_agent(preserve_history)`. Approver + cancel + permission-mode + thinking-budget all preserved across rebuilds.

History is held in `Arc<Mutex<Vec<Message>>>` inside the agent so `clear_history()`, `set_history()`, `history_snapshot()` are cheap.

---

## 3. The per-turn pipeline

Pseudocode of `Agent::run_turn_multipart`'s body (`agent.rs:754-1320`):

```rust
try_stream! {
    // 0. Push user message to history
    history.push(Message { role: User, content: user_content });

    // 1. Per-iteration loop, capped at max_iterations (default 50 in
    //    AppConfig; 0 means unlimited)
    for iteration in 0..effective_max {
        yield AgentEvent::IterationStart { iteration };

        // 2. Compose the per-turn system prompt: base + plan reminder +
        //    todos reminder. Cheap (string concat per turn) so plan
        //    state / todos changes take effect on the next iteration.
        // 3. Compact history to fit budget_tokens
        let messages = compact(&history.lock(), budget_tokens);

        // 4. Build StreamRequest
        let req = StreamRequest {
            model, system, messages, tools: tool_defs,
            max_tokens: current_max_tokens,
            thinking_budget,
        };

        // 5. provider.stream(req) with up to max_retries backoff
        //    (cancel-interruptible — M6.17 BUG M3)
        let raw = with_retry_backoff(provider.stream(req)).await?;

        // 6. assemble(raw) folds raw ProviderEvents into semantic
        //    AssembledEvents (Text / Thinking / ToolUse / Done /
        //    ToolParseFailed)
        let mut assembled = assemble(raw);

        // 7. Drain the assembled stream:
        //    - Text → buffer + yield Text
        //    - Thinking → buffer (persisted but not yielded — no UI
        //      consumer yet)
        //    - ToolUse → yield ToolCallStart immediately (M6.17 L1),
        //      buffer for execution
        //    - ToolParseFailed → synth empty-input ToolUse + record
        //      parse error (M6.17 L4)
        //    - Done → capture stop_reason + accumulate usage
        let (text, thinking, tool_uses, parse_errors, stop_reason) = drain(assembled);

        // 8. Persist the assistant message to history
        history.push(Message {
            role: Assistant,
            content: [Thinking + Text + ToolUses],
        });

        // 9. Stop condition: no tool uses → turn over
        if tool_uses.is_empty() {
            if stop_reason == "max_tokens" && !escalated_yet {
                // 9a. Single-shot escalation: bump max_tokens to 64K,
                //     skip the tool-result push (no tools = no result),
                //     continue the loop for another LLM call
                current_max_tokens = ESCALATED_MAX_TOKENS;
                continue;
            }
            // 9b. Natural end of turn
            yield AgentEvent::Done { stop_reason, usage };
            return;
        }

        // 10. Per-tool dispatch loop:
        //     - Parse error short-circuit (M6.17 L4)
        //     - Plan-mode block (M2)
        //     - TodoWrite block in plan mode
        //     - Plan-approval gate (M5)
        //     - Permission-mode approval (Ask)
        //     - Execute via tool.call_multimodal
        //     - Truncate-to-disk if oversized
        //     - For MCP-Apps tools: fetch_ui_resource → ship widget HTML
        //     - Yield ToolCallResult (with optional ui_resource)
        let result_blocks = dispatch_tools(tool_uses, parse_errors);

        // 11. Push tool results as a User message so the model sees
        //     them on the next iteration. Empty result_blocks skipped
        //     (Anthropic rejects empty user messages).
        if !result_blocks.is_empty() {
            history.push(Message { role: User, content: result_blocks });
        }
        // → loop back to step 2 with the model's new context
    }

    // 12. Hit max_iterations → emit Done with stop_reason="max_iterations"
    yield AgentEvent::Done { stop_reason: "max_iterations", usage };
}
```

The `try_stream!` macro from `async_stream` produces a `Stream` whose items are `Result<AgentEvent>`; `?` inside the body propagates as `Some(Err(e))` and ends the stream.

---

## 4. System-prompt composition (per turn)

`agent.rs:766-787` rebuilds the per-turn system prompt every iteration so dynamic state (plan mode, plan submission, todos.md) is fresh:

```
<base system prompt>
              ↑
        from `Agent::new(... system)`. Includes:
        - prompts/system.md (CLAUDE.md-equivalent)
        - ProjectContext discoveries (CLAUDE.md / AGENTS.md auto-merge)
        - Memory store section
        - KMS attachments
        - Team grounding (when team mode active)
        - Available skills section (per skills_listing_strategy)

<plan reminder>            (when in PermissionMode::Plan)
        Layer-1 instructions: "Use Read/Grep/Glob/Ls to explore.
        When ready, call SubmitPlan with concrete steps. The user
        will review and approve."
        Layer-2 (M4.1, when a plan is submitted): focuses on the
        current step + escalates wording on repeated attempts.

<todos reminder>           (when .thclaws/todos.md exists)
        Renders the current todo list inline so the model treats it
        as living state, not a stale artifact from a prior session.
```

The plan and todos reminders combine if both are active (plan dominates layout). Captured at iteration start, so EnterPlanMode / sidebar Approve / `/plan` slash flips take effect on the **next** iteration.

---

## 5. History compaction

`compaction.rs::compact(messages, budget_tokens)` — synchronous drop-oldest with two invariants:

1. **Tool-pair preservation.** Never splits a `ToolUse` from its `ToolResult`: if dropping a message would orphan a result, drop the result too.
2. **At-least-one-message guarantee** with **truncation rescue** (M6.17 BUG M1). The drop loop stops with at least the last message. If that single message still exceeds budget, `truncate_oversized_message` truncates each Text / ToolResult block in-place (char-boundary safe) and appends a `[...truncated by thClaws: original content exceeded the N token context budget]` notice the model can read.

Pre-M6.17: the loop returned the single oversize message intact, leading to a 400 "context length exceeded" from the provider. Now: provider always sees a request that fits.

`compact_with_summary` (`compaction.rs:111`) is the LLM-summarized variant invoked by `/compact`. Strategy: keep the last 4 messages verbatim, ask the provider to summarize the older ones, prepend the summary as a synthetic user message. Falls back to drop-oldest on summary failure.

`compact_for_step_boundary` and `clear_for_step_boundary` (`compaction.rs:260, 364`) are M6.2 / M6.4 plan-mode variants — invoked at step transitions in plan execution to either compact or fully clear the prior step's conversation noise.

`estimate_tokens` is conservative (~3–4 chars/token); the budget walk is approximate but bounded above the provider's hard limit.

---

## 6. Provider stream + retry with backoff

```rust
for attempt in 0..=max_retries {
    match provider.stream(req.clone()).await {
        Ok(s) => { stream_result = Some(s); break; }
        Err(e) => {
            let is_config = matches!(e, Error::Config(_));
            if !is_config && attempt < max_retries {
                let delay = Duration::from_secs(1 << attempt);
                // M6.17 BUG M3: select! against cancel so a Stop
                // during the retry sleep short-circuits in <50ms
                // instead of waiting the full 1+2+4 = 7s cycle.
                if let Some(token) = &cancel {
                    tokio::select! {
                        _ = tokio::time::sleep(delay) => {}
                        _ = token.cancelled() => {
                            cancelled_during_retry = true;
                            break;
                        }
                    }
                } else {
                    tokio::time::sleep(delay).await;
                }
            }
            last_err = Some(e);
            if is_config { break; }
        }
    }
}
```

- **Backoff**: exponential 1s, 2s, 4s. Worst-case 7s before giving up.
- **`Error::Config` skips the retry loop.** Missing API key / bad model name doesn't fix itself between attempts.
- **Mid-stream errors don't retry.** Once `provider.stream()` returns Ok, the assembled stream is consumed live; if it errors mid-response, that propagates out as the iteration's error and the turn ends. Probably intentional — the partial response is already user-visible. Tracked as "BUG L2 deferred" in `dev-log/135`.

---

## 7. Stream assembly: `ProviderEvent → AssembledEvent`

`providers/assemble.rs::assemble(raw)` folds a `Stream<ProviderEvent>` into a `Stream<AssembledEvent>` by buffering partial blocks. Per-event behavior:

| `ProviderEvent` | `BlockState` transition | Yields |
|---|---|---|
| `MessageStart { model }` | — (records implicit-thinking-model flag) | (nothing) |
| `TextDelta(s)` | `None`/`Text` → `Text` | `Text(s)` (or `Thinking(s)` if implicit-think buffer is open) |
| `ThinkingDelta(s)` | (no state change) | `Thinking(s)`. Also disables implicit-think buffer for the rest of the stream — Qwen3.6 / DashScope reasoning lives out-of-band so the `</think>` tag never arrives in the text stream |
| `ToolUseStart { id, name }` | → `ToolUse { id, name, buf: "" }` | (nothing — buffering JSON) |
| `ToolUseDelta { partial_json }` | append to `ToolUse::buf` | (nothing) |
| `ContentBlockStop` | drains current state | If `ToolUse`: parse `buf` as JSON. Success → `ToolUse(ContentBlock::ToolUse{...})`. Failure → `ToolParseFailed{id, name, error}` (M6.17 BUG L4 — pre-fix this returned `Err` and killed the turn) |
| `MessageStop { stop_reason, usage }` | — | `Done { stop_reason, usage }` |

### `split_think_text` — handling implicit `<think>...</think>` text streams

Some model families (Qwen3, QwQ, DeepSeek-R1) emit reasoning as plain text wrapped in `<think>...</think>` tags rather than a structured `reasoning_content` field. `split_think_text` (`assemble.rs:91`) is a small state machine that buffers partial tags across SSE chunks and:

- Routes text inside `<think>...</think>` to `AssembledEvent::Thinking`
- Routes text outside to `AssembledEvent::Text`
- Trims leading newlines after `</think>` (the blank-lines-between-think-and-answer convention)

`is_implicit_thinking_model` is conservative (only Qwen3/QwQ/DeepSeek-R1) — other model families pay zero overhead.

When a structured `ThinkingDelta` arrives mid-stream from a model whose name matched the implicit-thinking heuristic (e.g. "qwen3.6-flash" on DashScope, where reasoning is actually out-of-band), the assembler disables the implicit buffer permanently for that stream. Pre-fix DashScope qwen3.6 emitted text that got swallowed as Thinking forever because the `</think>` close never arrived in the content channel.

---

## 8. Tool dispatch (parse → announce → gate → execute)

After `assemble` finishes, the agent has a buffer of `turn_tool_uses: Vec<ContentBlock>` plus a `turn_parse_errors: Vec<(id, error)>` map for any malformed-JSON blocks. The dispatch loop runs each one through:

```
for tu in &turn_tool_uses:
    let ContentBlock::ToolUse { id, name, input } = tu else continue;

    1. Parse-error short-circuit (M6.17 BUG L4)
       if id ∈ turn_parse_errors:
           push synthetic error tool_result
           yield ToolCallResult { id, name, output: Err(parse_msg), ui_resource: None }
           continue;

    2. Tool lookup
       tool = registry.get(name) or yield_unknown_tool_error_and_continue();

    3. Read CURRENT permission mode
       (so EnterPlanMode mid-turn flips immediately)
       mode = permissions::current_mode();

    4. Plan-mode block (M2)
       if mode == Plan && tool.requires_approval(input):
           push "Blocked: <tool> not available in plan mode" error result
           yield ToolCallResult { ..., output: Err(blocked) }
           continue;

    5. TodoWrite block in plan mode
       if mode == Plan && name == "TodoWrite":
           push "Blocked: TodoWrite is the casual scratchpad..." error
           continue;

    6. Plan-approval gate (M5)
       if mode == Plan && (name == "UpdatePlanStep" || name == "ExitPlanMode")
          && plan_state::get().is_some():
           push "Blocked: not available while waiting for user approval" error
           continue;

    7. Approval gate
       if mode == Ask && tool.requires_approval(input):
           decision = approver.approve(req).await;
           if Deny:
               push "denied by user: <name>" error
               yield ToolCallDenied { id, name }
               continue;

    8. Execute
       result = tool.call_multimodal(input).await;
       (truncate oversized text via maybe_truncate_to_disk)

    9. MCP-Apps widget fetch (only on success)
       ui_resource = if result.is_ok() { tool.fetch_ui_resource().await } else { None };

    10. Push tool_result + yield ToolCallResult
        result_blocks.push(ToolResult { tool_use_id, content, is_error });
        yield ToolCallResult { id, name, output, ui_resource };
```

### `ToolCallStart` is yielded at PARSE time, not execution time (M6.17 BUG L1)

```rust
AssembledEvent::ToolUse(block) => {
    if let ContentBlock::ToolUse { id, name, input } = &block {
        yield AgentEvent::ToolCallStart { id, name, input };  // ← here
    }
    turn_tool_uses.push(block);
}
```

So the UI shows `[tool: Bash]` the instant the model emits the tool call — BEFORE the per-tool execution loop's gates run. Pre-fix the announce came right before the actual `tool.call(...)` call, so the user saw "assistant text streaming → silent pause → result." New contract: `tool_calls` (in `AgentTurnOutcome`) = "model parsed", `tool_denials` = "of those, which were rejected".

### Why `permissions::current_mode()` is read inside the loop

Plan mode can be flipped mid-turn by `EnterPlanMode` (the model decided to go into plan mode), `ExitPlanMode` (the model thinks it's ready), the sidebar Approve / Cancel buttons, or `/plan` slash. Reading the global on every dispatch — not capturing it once at iteration start — means the next tool call sees the current mode, not the stale mode from before the flip.

The `permission_mode_default` field on `Agent` is only used as a startup-default fallback (when `current_mode()` returns `Ask` on a bare-default Mutex before any explicit `set_current_mode` has been called).

### `maybe_truncate_to_disk` — for oversized tool output

`agent.rs::maybe_truncate_to_disk` (called at line 1135 inside the dispatch loop). If a Text tool result exceeds `TOOL_RESULT_CONTEXT_LIMIT` (50,000 bytes), the full content is saved to a temp file and the in-context message becomes:

```
<first 2000 chars>

... [truncated: 200000 total bytes — full output saved to /tmp/thclaws-tool-output/tool-12345-abc….txt]
```

If the disk write fails (FS full, permissions), the footer becomes:

```
... [truncated: 200000 total bytes — could not save full output to disk (<error>); preview only]
```

M6.17 BUG M2 fix: filename is now `tool-{pid}-{uuid}.txt`. Pre-fix it was just `tool-{pid}.txt`, so multiple truncations in the same process overwrote each other and the model's "reference the full file" promise was a lie after the second truncation.

Multimodal `Blocks` results pass through unchanged (vision tools, etc. — bounded in size by the provider's image limits).

### MCP-Apps `fetch_ui_resource` — widget HTML attachment

After a successful tool call, `tool.fetch_ui_resource().await` runs (`agent.rs:1180+`). For most tools this returns `None` (no widget). For `McpTool` (the MCP adapter), if the tool advertised a `ui_resource_uri` in its `_meta` AND the originating server is `trusted: true`, the URI is fetched via `resources/read` and the resulting HTML returned as `Some(UiResource { uri, html, mime })`. The agent ships it via `AgentEvent::ToolCallResult { ui_resource, ... }`; the GUI mounts an iframe alongside the text result. See [`mcp.md`](mcp.md) §7.

---

## 9. Stop conditions

The loop exits via one of:

| Condition | Where | What's emitted |
|---|---|---|
| **No tool uses in the assistant message** | `agent.rs:944` | `AgentEvent::Done { stop_reason, usage }`. Most common — model produced a text-only response. |
| **`max_tokens` hit + already escalated** | `agent.rs:951` | Same. Single-shot: first `max_tokens` triggers a continue with `ESCALATED_MAX_TOKENS`; the second falls through to Done. |
| **`max_iterations` exhausted** | `agent.rs:1207` | `AgentEvent::Done { stop_reason: "max_iterations", usage }`. Default 50 (`AppConfig::max_iterations`). 0 means unlimited. |
| **Provider error after retries** | `agent.rs:870` (via `?`) | The stream errors with `Some(Err(e))` then ends. Worker shows `Error: <msg>` + emits `TurnDone`. |
| **Cancel during retry sleep** (M6.17 BUG M3) | `agent.rs:870` (via `?`) | Same — the agent stream errors with "cancelled by user during retry backoff". Worker shows `(interrupted)` + saves history + `TurnDone`. |
| **Cancel mid-stream** (M6.17 BUG H1) | `shared_session.rs::drive_turn_stream` `tokio::select!` | Worker drops the stream, shows `(interrupted)`, saves history, `TurnDone`. The agent's underlying provider request future drops along with the stream; HTTP body may continue server-side until tcp close, but the local future tree unwinds. |

The escalation single-shot logic:

```rust
if turn_tool_uses.is_empty() {
    if stop_reason == "max_tokens" && current_max_tokens < ESCALATED_MAX_TOKENS {
        current_max_tokens = ESCALATED_MAX_TOKENS;  // 8192 → 64000
        continue;  // skip the empty-tool-result push
    }
    yield Done { ... };
    return;
}
```

The `continue` skips the tool-result-push branch (no tool uses = nothing to push, and Anthropic rejects empty `user` messages with `messages.N: user messages must have non-empty content`).

---

## 10. Cancellation model: `CancelToken`

New in M6.17 (`crates/core/src/cancel.rs`). Pairs an `AtomicBool` (sync polling) with `tokio::sync::Notify` (async wakeup):

```rust
#[derive(Clone, Default, Debug)]
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
}

impl CancelToken {
    pub fn cancel(&self);              // flag.store(true) + notify.notify_waiters()
    pub fn is_cancelled(&self) -> bool;
    pub fn reset(&self);               // flag.store(false), prep for next op
    pub async fn cancelled(&self);     // resolves on cancel; checks flag first
}
```

### Three places it's checked

1. **Worker drive loop** (`shared_session.rs::drive_turn_stream`, also the inline drive in `handle_team_messages`):
   ```rust
   loop {
       let ev = tokio::select! {
           biased;
           _ = cancel.cancelled() => {
               // emit (interrupted), save, return
           }
           ev = stream.next() => ev,
       };
       // ...
   }
   ```
   `biased` prefers the cancel arm so a freshly-fired cancel pre-empts a still-pollable stream.

2. **Agent retry sleep** (`agent.rs:851`):
   ```rust
   tokio::select! {
       _ = tokio::time::sleep(delay) => {}
       _ = token.cancelled() => { cancelled_during_retry = true; break; }
   }
   ```

3. **Worker input-loop dispatch** (`shared_session.rs:896, 900, 1038`):
   `cancel.reset()` at the start of every new user turn so a stale cancel from the previous turn doesn't immediately fire on the next.

### What's NOT plumbed yet

`Tool::call` doesn't accept a cancel token. A long-running Bash command, for example, can't be killed mid-run. The worker stops polling the stream (so no further events surface) but the tool's underlying subprocess continues. Tracked as "tool-trait cancel propagation" in `dev-log/135` — needs a trait change + per-tool implementation.

---

## 11. Plan-mode integration

Plan mode (`PermissionMode::Plan`) is a special permission state where:

- Mutating tools are **blocked** — `tool.requires_approval(input)` is the discriminator. Read tools (Read/Grep/Glob/Ls) and the plan tools themselves (SubmitPlan, UpdatePlanStep, EnterPlanMode, ExitPlanMode) sail through; Bash/Write/Edit are blocked with a structured tool_result the model reads on the next iteration ("Use Read/Grep/Glob/Ls to explore...").
- **TodoWrite is blocked** specifically. SubmitPlan / UpdatePlanStep are the structured replacement; letting both coexist confused the model in tests.
- Once `SubmitPlan` has populated `plan_state` AND the user hasn't approved yet, **`UpdatePlanStep` and `ExitPlanMode` are also blocked** — they'd bypass the user's review window. The only legal path out is the user clicking sidebar Approve / Cancel, which fires `plan_approve` / `plan_cancel` IPCs.

The blocks don't pop modal — they return an error tool_result the model reads next iteration and self-corrects. See the per-tool dispatch flow in §8.

The plan-state broadcaster (set up at worker spawn, `shared_session.rs:625`) listens for plan-tool mutations (SubmitPlan, UpdatePlanStep, clear) and:
1. Sends `ViewEvent::PlanUpdate` to the GUI sidebar
2. Appends a `plan_snapshot` event to the active session's JSONL

The session loader's `restore_from_session` repopulates `plan_state` from the latest snapshot on `/load`. M6.16.1 hardened load_from to ignore plan_snapshot timestamps for sort recency (so clicking a session doesn't bump it to the top of the sidebar).

---

## 12. Approval gate

`crate::permissions::ApprovalSink` is a trait the agent calls when `mode == Ask && tool.requires_approval(input)`. Two implementations:

- **`AutoApprover`** — accepts everything. Used in tests + when `permission_mode == Auto`.
- **`GuiApprover`** (in `gui.rs`) — fires an IPC modal that resolves to `Allow` / `AllowForSession` / `Deny`. The worker awaits the modal's response.
- **`CliApprover`** (in `repl.rs`) — stderr/stdin prompt requiring a TTY.

The agent loop calls `approver.approve(req).await` and matches on the decision:
- `Allow` / `AllowForSession` → execute the tool
- `Deny` → push a denial tool_result, yield `ToolCallDenied { id, name }`, continue to the next tool

`ApprovalRequest` shape:
```rust
pub struct ApprovalRequest {
    pub tool_name: String,
    pub input: serde_json::Value,
    pub summary: Option<String>,    // user-facing one-liner
}
```

The summary is set for non-default flows (e.g. M6.15 BUG 2 sets it to `"MCP-App widget requested \`{name}\`. Allow?"` so the user knows the LLM didn't decide this). Tools dispatched via the agent loop's standard path leave `summary: None` and the modal renders the bare tool input.

---

## 13. Wire-format examples

### Assistant message persisted to history

Single-turn with thinking + text + tool_use:

```json
{
  "role": "assistant",
  "content": [
    {"type": "thinking", "content": "Let me check the file first..."},
    {"type": "text", "text": "I'll read the config now."},
    {"type": "tool_use", "id": "toolu_01abc", "name": "Read", "input": {"path": "/etc/hosts"}}
  ]
}
```

Order matters: thinking → text → tool_uses. Mirrors the order the model emitted; some providers expect that order on echo.

### Tool result message

```json
{
  "role": "user",
  "content": [
    {
      "type": "tool_result",
      "tool_use_id": "toolu_01abc",
      "content": "127.0.0.1 localhost\n...",
      "is_error": false
    }
  ]
}
```

Multi-tool turns produce multiple `tool_result` blocks in one user message — never split across messages, and always paired with the matching `tool_use_id`.

### `AgentEvent` stream as the worker sees it

```
IterationStart { iteration: 0 }
ToolCallStart { id: "toolu_01abc", name: "Read", input: {...} }    ← M6.17 L1: parse-time
Text("I'll read the config now.")
ToolCallResult { id: "toolu_01abc", name: "Read", output: Ok("..."), ui_resource: None }
IterationStart { iteration: 1 }
Text("The file shows...")
Done { stop_reason: Some("end_turn"), usage: Usage { ... } }
```

Cancel mid-stream: the worker observes `cancel.cancelled()` in its `tokio::select!`, drops the stream (which cancels the underlying `try_stream!` future tree), emits `(interrupted)`, persists history, fires `TurnDone`.

---

## 14. Code organization

```
crates/core/src/
├── agent.rs                       ── ~2400 LOC, the loop
│   ├── Agent (struct + new + with_* builders)
│   ├── run_turn / run_turn_multipart           (Stream-returning entry points)
│   ├── (per-iteration body, inside try_stream!)
│   │   ├── compose system prompt (plan + todos reminders)
│   │   ├── compact history
│   │   ├── provider.stream + retry backoff (cancel-aware)
│   │   ├── assemble + drain (Text / Thinking / ToolUse / ToolParseFailed)
│   │   ├── persist assistant message
│   │   ├── per-tool dispatch loop
│   │   │   ├── parse-error short-circuit (L4)
│   │   │   ├── plan-mode block (M2)
│   │   │   ├── TodoWrite-in-plan-mode block
│   │   │   ├── plan-approval gate (M5)
│   │   │   ├── approval gate (Ask)
│   │   │   ├── tool.call_multimodal
│   │   │   ├── maybe_truncate_to_disk
│   │   │   ├── tool.fetch_ui_resource (MCP-Apps)
│   │   │   └── yield ToolCallResult
│   │   ├── push tool_results
│   │   └── stop / continue
│   ├── AgentEvent (IterationStart / Text / ToolCallStart / ToolCallResult / ToolCallDenied / Done)
│   ├── AgentTurnOutcome + collect_agent_turn   (test/utility collector)
│   ├── maybe_truncate_to_disk                  (UUID filename, M6.17 M2)
│   ├── build_plan_reminder + build_todos_reminder + build_step_continuation_prompt
│   └── tests                                   (~30 unit + integration)
│
├── providers/
│   ├── mod.rs                                  (Provider trait, ProviderEvent, StreamRequest, Usage)
│   ├── assemble.rs                             (assemble, AssembledEvent + ToolParseFailed, split_think_text)
│   ├── anthropic.rs / openai.rs / gemini.rs / openai_responses.rs / ollama.rs / ollama_cloud.rs / dashscope.rs / agent_sdk.rs / agentic_press.rs / openrouter.rs
│   └── gateway.rs                              (provider-aware HTTP gateway used by KMS / endpoints)
│
├── compaction.rs                               (compact + truncate_oversized_message + compact_with_summary + step-boundary variants)
├── cancel.rs                                   (CancelToken — M6.17)
├── permissions.rs                              (PermissionMode, ApprovalSink, current_mode, set_current_mode_and_broadcast)
├── tools/mod.rs                                (Tool trait, ToolRegistry, call_multimodal, fetch_ui_resource)
├── tools/plan_state.rs                         (Plan, PlanStep, broadcaster)
├── tools/                                      (Bash, Read, Write, Edit, Grep, Glob, Ls, ...)
├── shared_session.rs
│   ├── WorkerState                             (agent + config + session + tool_registry + skill_store + mcp_clients + cancel + ...)
│   ├── run_worker / spawn / spawn_with_approver
│   ├── handle_line / handle_line_with_images   (entry from ShellInput)
│   ├── drive_turn_stream                       (consumes AgentEvent stream → ViewEvent IPC)
│   ├── handle_team_messages                    (inline drive for team-orchestrated turns)
│   └── ShellInput / ViewEvent enum
│
└── repl.rs::run_repl                           (CLI's analog of drive_turn_stream)
```

---

## 15. Testing

`agent::tests` ships ~30 tests:

**Layer-2 plan-reminder shape** (pure-function, no I/O):
- `step_continuation_prompt_first_attempt_is_terse_and_directive`
- `step_continuation_prompt_in_progress_step_skips_begin_transition`
- `step_continuation_prompt_escalates_on_repeated_attempts`
- `step_continuation_prompt_surfaces_prior_step_outputs`
- `step_continuation_prompt_omits_outputs_section_when_none`
- `step_continuation_prompt_excludes_failed_step_outputs`
- `step_continuation_prompt_no_op_for_done_or_failed_steps`

**Loop end-to-end** (against `ScriptedProvider`, a fake provider that returns a canned event sequence):
- `turn_with_no_tool_calls_yields_text_and_done`
- `turn_with_tool_call_executes_and_loops`
- `ask_mode_denies_and_surfaces_error_result` (M6.17 L1 update — denied call now in `tool_calls`)
- `ask_mode_skips_approval_for_read_only_tools`
- `iteration_cap_yields_max_iterations_done`
- `cancel_during_retry_sleep_short_circuits` (M6.17 H1+M3 regression)

**`assemble` layer** (~10 tests):
- `text_only_turn`
- `tool_use_accumulates_partial_json`
- `structured_thinking_disables_implicit_thinking_buffer`
- `malformed_tool_json_emits_parse_failed_event` (M6.17 L4 — pre-fix expected an Err; now expects in-band recovery)
- `multiple_text_blocks_concatenate`
- `think_tag_split_across_chunks`
- `closing_think_tag_then_text_chunks_correctly`

**`compaction` layer** (~10 tests):
- `compact_under_budget_returns_unchanged`
- `compact_drops_oldest_until_under_budget`
- `compact_preserves_tool_pair`
- `never_drops_below_last_message_and_truncates_oversize` (M6.17 M1 — pre-fix asserted equality)
- `summary_compaction_inserts_synthetic_user_message`
- `step_boundary_clear_picks_plan_trigger_when_unrelated_chat_precedes_plan`
- `step_boundary_clear_keeps_first_user_when_plan_starts_at_top_of_session`

**`cancel` module** (4 tests):
- `cancelled_returns_immediately_when_already_cancelled`
- `cancelled_wakes_when_cancel_called_while_waiting`
- `select_against_long_sleep` (the canonical use case — interrupt a 10s sleep within 200ms)
- `reset_clears_flag`

**Worker drive loop** — covered by manual verification (no fake-stream `WorkerState` fixture exists yet). Tracked as "BUG L5 deferred" in `dev-log/135`.

---

## 16. Migration / known limitations

### M6.17 fixes (`dev-log/135`)

| # | Severity | What | Where |
|---|---|---|---|
| H1 | HIGH | Cancel was unresponsive during long tool runs / stalled streams. `tokio::select!` on `cancel.cancelled()` in both worker drive loops. | `shared_session.rs::drive_turn_stream` + `handle_team_messages` |
| M1 | MED | `compact()` could return a single message > budget. New `truncate_oversized_message` rescues with char-boundary truncation + notice. | `compaction.rs::compact` |
| M2 | MED | Truncation filename collision (`tool-{pid}.txt` overwrote across calls). UUID per truncation + disk-error surfacing. | `agent.rs::maybe_truncate_to_disk` |
| M3 | MED | Cancel was unresponsive during retry-backoff sleep (1+2+4 = 7s blocking). `tokio::select!` against cancel. | `agent.rs:851` |
| L1 | LOW | Tool calls didn't surface until assistant message fully assembled. `ToolCallStart` now yields at parse time. | `agent.rs::run_turn` |
| L4 | LOW | Tool-use JSON parse failure killed the entire turn. New `AssembledEvent::ToolParseFailed` + agent synthesizes error tool_result. | `assemble.rs` + `agent.rs` |

### Deferred

- **BUG L2** — mid-stream errors don't retry. Probably intentional (don't double-stream the same response); known limitation.
- **BUG L3** — `BlockState::ToolUse` overwrites on back-to-back `ToolUseStart` without `ContentBlockStop`. Provider-bug-only path; not observed in practice.
- **BUG L5** — no GUI integration tests for cancel paths. Needs a fake-stream `WorkerState` fixture.
- **Tool-trait cancel propagation** — `Tool::call` doesn't accept a cancel token. Long Bash subprocesses can't be killed mid-run. Worker stops polling but the child process continues. Needs a trait change + per-tool implementation work.

### Sprint chronology

| Sprint | Dev-log | What shipped (loop-relevant) |
|---|---|---|
| Phase 8 | (initial) | `Agent::run_turn`, basic loop, tool dispatch, `assemble` |
| Phase 11 | `~115` | Plan-mode integration (M2 plan reminder, plan-state, plan-tool blocks) |
| M4.x | `114` | Layer-2 step narrowing in execution reminder + Retry/Skip/Abort sidebar plumbing |
| M5 | `115` | CLI ANSI parity for plan rendering, plan-approval gate (UpdatePlanStep / ExitPlanMode block during pending approval) |
| M6.x | `120-130` | Step-boundary compaction strategies (compact / clear), per-step driver, plan-quality reinforcements, stalled-turn detector |
| M6.10 | (in-flight) | `streaming` flag for Stop button while turn in flight |
| M6.15 | `133` | MCP-Apps widget tool-call approval gate (widget tool calls now go through the same approver as agent-initiated calls) |
| M6.17 | `135` | This sprint — cancel responsiveness, compaction safety, tool announce timing, parse-failure recovery, truncation-filename uniqueness |
