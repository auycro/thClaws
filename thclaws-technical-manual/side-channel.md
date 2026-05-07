# Side-channel agents (`/agent` slash command)

User-driven concurrent subagents. Where [`subagent.md`](subagent.md) covers the **model-driven** `Task` tool — parent's LLM decides to delegate, child blocks parent's turn, result lands in parent's history — this doc covers the **user-driven** counterpart that spawns the same `Agent` infrastructure on its own tokio task, runs concurrently with main, and never enters main's context.

The two surfaces share the AgentDef registry and the agent-build pipeline (`ProductionAgentFactory`). The differences are entirely lifecycle + visibility:

| | side-channel (this doc) | subagent ([`subagent.md`](subagent.md)) |
|---|---|---|
| **Trigger** | User types `/agent <name> <prompt>` | Model fires `Task` tool |
| **Execution** | `tokio::spawn` — concurrent with main agent | Same agent loop, blocks parent's turn |
| **Main's history** | Untouched — result is a separate UI bubble | Tool result lands in parent's context |
| **Cancel** | Independent `CancelToken` — main's Cmd-C does NOT kill side channels | Inherits parent's cancel (parent Cmd-C kills child) |
| **Visibility** | `chat_side_channel_*` IPC envelopes; per-id bubble in chat surface | Tool indicator in chat surface |
| **Permission attribution** | `AgentOrigin::SideChannel { id, agent_name }` on every approval request | `AgentOrigin::Main` (Subagent depth tagging is a follow-up) |

Both flows ultimately drive the same `Agent::run_turn` stream — the user-driven path just plumbs the events through different ViewEvent variants and lifecycle handlers.

This doc covers: the spawn lifecycle in detail, the process-level registry singleton, the `AgentOrigin`/`CancelToken::child` primitives that enable concurrent permission attribution and per-channel cancellation, the IPC protocol, the slash-command surface, the frontend integration (ApprovalModal + ChatView), and the testing surface.

**Source modules:**
- [`crates/core/src/side_channel.rs`](../thclaws/crates/core/src/side_channel.rs) — `SideChannelId`, `SideChannelHandle`, process-wide `registry()`, `spawn_side_channel`, `cancel_side_channel`, `list_side_channels`. Whole module is `#[cfg(feature = "gui")]` because it depends on `ViewEvent` from `shared_session` (also gui-gated).
- [`crates/core/src/permissions.rs`](../thclaws/crates/core/src/permissions.rs) — `AgentOrigin` enum (Main / SideChannel / Subagent), `originator: AgentOrigin` field on `ApprovalRequest` and `GuiApprovalRequest`. Carried through `GuiApprover::approve` to the frontend.
- [`crates/core/src/cancel.rs`](../thclaws/crates/core/src/cancel.rs) — `CancelToken::child()` constructor for downstream tokens that observe a parent's cancel transitively but don't propagate their own up. Only used by `Subagent`-style spawns; `/agent` side-channels use a fresh independent `CancelToken::new()` per spawn.
- [`crates/core/src/agent.rs`](../thclaws/crates/core/src/agent.rs) — `Agent::origin: AgentOrigin` field + `with_origin()` builder. The tool-dispatch loop reads `self.origin` and tags every `ApprovalRequest` with it.
- [`crates/core/src/shared_session.rs`](../thclaws/crates/core/src/shared_session.rs) — `WorkerState.agent_factory: Arc<dyn AgentFactory>` and `WorkerState.agent_defs: AgentDefsConfig`, populated at worker init. Five `ViewEvent::SideChannel*` variants. `dispatch` routes the new `SlashCommand::Agent { ... }` / `AgentsList` / `AgentCancel(...)` arms.
- [`crates/core/src/event_render.rs`](../thclaws/crates/core/src/event_render.rs) — chat dispatch arms emit `chat_side_channel_*` envelopes; terminal renderer emits one-line ANSI markers for start + done.
- [`crates/core/src/repl.rs`](../thclaws/crates/core/src/repl.rs) — `SlashCommand::Agent / AgentsList / AgentCancel` variants + `parse_agent_subcommand` parser. REPL dispatch prints "GUI-only" hint (the CLI doesn't have a broadcast surface to fan side-channel events through).
- [`crates/core/src/shell_dispatch.rs`](../thclaws/crates/core/src/shell_dispatch.rs) — GUI Chat-tab dispatch. This is where `/agent` actually fires `spawn_side_channel`.
- [`frontend/src/components/ApprovalModal.tsx`](../thclaws/frontend/src/components/ApprovalModal.tsx) — `AgentOrigin` discriminated union + `originLabel` / `originAccent` helpers; modal header reads "Main wants to run X" vs "translator (background) wants to run X" vs "researcher (subagent · depth 1) wants to run X".
- [`frontend/src/components/ChatView.tsx`](../thclaws/frontend/src/components/ChatView.tsx) — `ChatMessage.sideChannel` field + 5 IPC subscription arms + side-channel bubble render with status header, accent border, and accumulated stream output.

**Cross-references:**
- [`subagent.md`](subagent.md) — model-driven counterpart; same AgentDef registry + ProductionAgentFactory build pipeline
- [`agent-team.md`](agent-team.md) — heavyweight subprocess parallelism; the third primitive in the delegation hierarchy
- [`agentic-loop.md`](agentic-loop.md) — `Agent::run_turn` is the loop both subagent and side-channel drive
- [`permissions.md`](permissions.md) — `AgentOrigin` lives here; mention's the Subagent depth-tagging follow-up

---

## 1. Concept

Side-channel is the **third tier** in thClaws's delegation hierarchy:

```
mechanism                      │ trigger        │ concurrency       │ visibility
───────────────────────────────┼────────────────┼───────────────────┼─────────────────
TaskCreate / Update / Get / List │ model         │ N/A (no LLM)     │ /tasks list
Task tool (subagent)             │ model         │ blocks parent    │ tool indicator
/agent (side channel) ★          │ user          │ concurrent       │ side bubble
SpawnTeammate (team)             │ model or user │ subprocess       │ Team tab pane
```

★ Where this doc focuses.

The model-decided `Task` tool is the right choice when delegation is part of the parent's reasoning ("I should ask the reviewer about this"). The user-driven `/agent` is the right choice when the user knows specifically what they want a specialist to do AND wants to keep working with main while it runs. Examples:

- "Translate this file to Thai" while continuing to discuss code with main
- "Summarize the open PRs" while drafting a release note in main
- Long-running research that doesn't need to feed back into main's context immediately

**LLM-context isolation is preserved** — main's history doesn't grow because of `/agent` calls, and the side-channel agent has its own conversation thread (just like a Task subagent). The "concurrent" part is purely about the user being able to type into main while the side-channel agent runs.

## 2. Spawn lifecycle

```rust
pub async fn spawn_side_channel(
    agent_name: String,
    prompt: String,
    factory: Arc<dyn AgentFactory>,
    agent_defs: AgentDefsConfig,
    events_tx: broadcast::Sender<ViewEvent>,
) -> Result<SideChannelId>
```

Five sequential steps:

```
1. resolve agent_name → AgentDef in agent_defs.agents
   └─ error early if not found ("unknown agent '...' — try /agents")

2. generate stable id: format!("side-{}", uuid::Uuid::new_v4().split('-').first())
   └─ short, human-typable for /agent cancel

3. build child Agent via factory.build(prompt, Some(&agent_def), child_depth=0)
   └─ inherits parent's tool registry (filtered by AgentDef.tools allow-list +
      disallowed_tools deny-list, same as Task subagent)
   └─ inherits parent's system prompt + AgentDef.instructions addendum

4. override agent.with_origin(SideChannel { id, agent_name })
              .with_cancel(fresh CancelToken::new())
   └─ origin tagging makes every ApprovalRequest from this agent carry
      "translator (side-abc123)" so the modal can disambiguate
   └─ fresh independent cancel — main's Cmd-C does NOT propagate (this is the
      explicit semantic distinction from CancelToken::child() used elsewhere)

5. emit ViewEvent::SideChannelStart { id, agent_name }
   spawn tokio::spawn(async move { stream_loop(agent, prompt, events_tx, ...) })
   insert handle into registry()
   return Ok(id)
```

Returns immediately — the agent work happens asynchronously on the spawned task. Caller can subscribe to `events_tx` to observe streaming updates.

The `stream_loop` inside the spawn task:

```rust
loop {
    let next = tokio::select! {
        ev = stream.next() => ev,
        _ = cancel.cancelled() => { errored = Some("cancelled".into()); break; }
    };
    match ev {
        Ok(AgentEvent::Text(s)) => {
            full_text.push_str(&s);
            events_tx.send(ViewEvent::SideChannelTextDelta { id, text: s });
        }
        Ok(AgentEvent::ToolCallStart { name, .. }) => {
            events_tx.send(ViewEvent::SideChannelToolCall { id, tool_name: name, ... });
        }
        Ok(AgentEvent::Done { .. }) => break,
        Err(e) => { errored = Some(format!("{e}")); break; }
        _ => {}  // Thinking / ToolCallResult / ToolCallDenied / IterationStart all dropped
    }
}

// Always emit terminal event + remove from registry on exit.
match errored {
    Some(e) => events_tx.send(ViewEvent::SideChannelError { id, error: e }),
    None => events_tx.send(ViewEvent::SideChannelDone { id, agent_name, duration_ms, result_text: full_text }),
}
registry().lock().remove(&id);
```

`Thinking` / `ToolCallResult` / `IterationStart` events are dropped from the side-channel stream — too noisy for the chat surface. The full `result_text` is delivered in the terminal `Done` event.

## 3. Process-wide registry

```rust
pub fn registry() -> &'static Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>>;

pub struct SideChannelHandle {
    pub agent_name: String,
    pub started_at: Instant,
    pub cancel: CancelToken,
    pub join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}
```

`OnceLock<Arc<Mutex<HashMap<...>>>>` singleton — same instance shared across all surfaces (CLI REPL, GUI Chat dispatch). Two reasons it's a singleton:

1. `/agents` and `/agent cancel <id>` need to operate on the SAME set regardless of which surface fired the spawn.
2. Surfaces don't need to thread the registry handle through their dispatch chain — they call the static `registry()` accessor.

**Cleanup**: the spawn task itself removes its entry on exit (whether by success, error, or cancel — it's at the very end of the `tokio::spawn` block). The registry never accumulates stale handles. No periodic GC needed.

**Race**: between insert (after `tokio::spawn`) and the task's eventual remove (at task end), there's a window where the handle is in the registry. `cancel_side_channel(id)` reaching for that handle during the window is safe — `cancel.cancel()` is idempotent and the spawn task's `cancelled()` await will pick it up via the select gate.

Public accessors:

```rust
pub fn cancel_side_channel(id: &str) -> bool;
pub fn list_side_channels() -> Vec<(String, String, f64)>;  // (id, agent_name, elapsed_secs)
```

`list_side_channels` deliberately returns plain tuples instead of `&SideChannelHandle` — keeps `CancelToken` and `JoinHandle` private to the module.

## 4. `AgentOrigin` end-to-end

The origin tag flows from agent construction through tool dispatch, GuiApprover, IPC, and lands in the React modal:

```
Agent::with_origin(SideChannel { id: "side-abc123", agent_name: "translator" })
        ↓ stored on Agent.origin
Agent::run_turn loop captures `let origin = self.origin.clone()` once per turn
        ↓ at each tool's approval gate
ApprovalRequest { tool_name, input, summary, originator: origin.clone() }
        ↓ passed to approver.approve(&req)
GuiApprover::approve copies originator into GuiApprovalRequest
        ↓ tx.send(out)
gui.rs forwarder receives it, builds JSON:
    { "type": "approval_request", "id", "tool_name", "input", "summary", "originator": req.originator }
        ↓ proxy.send_event(UserEvent::Dispatch(payload))
Frontend `window.__thclaws_dispatch(payload)`
        ↓
ApprovalModal subscribes via useIPC.ts → push onto queue with originator
        ↓ render
Modal header: "<originLabel(originator)> wants to run <tool_name>"
   - originator.kind == "main"          → "Main"            (accent)
   - originator.kind == "side_channel"  → "translator (background)"  (amber)
   - originator.kind == "subagent"      → "researcher (subagent · depth 1)"  (blue)
```

Backwards compat: `ApprovalRequest.originator` defaults to `AgentOrigin::Main`. Existing code paths (the Agent's tool loop reads from `self.origin`, default Main) keep working unchanged. Frontend reads `msg.originator ?? { kind: "main" }` for old backends.

The `Subagent` variant exists in the enum but isn't fully wired yet — `crate::subagent::SubAgentTool::call` builds the child Agent without setting `with_origin`, so subagent calls still tag as `Main`. Subagent depth-tagging is the natural follow-up; the enum is already shaped for it.

## 5. `CancelToken::child()` semantics

```rust
pub struct CancelToken {
    flag: Arc<AtomicBool>,
    notify: Arc<Notify>,
    parent_flag: Option<Arc<AtomicBool>>,
    parent_notify: Option<Arc<Notify>>,
}

pub fn child(&self) -> Self;  // produces a downstream token
```

Contract:

- **Parent → child propagation**: `parent.cancel()` flips parent's flag; child's `is_cancelled()` returns true via the `parent_flag.load()` check; child's `cancelled().await` wakes via `parent_notify.notified()` in the inner `tokio::select!`.
- **Child → parent isolation**: `child.cancel()` only flips the child's own flag. Parent's flag and siblings' flags untouched.

**Important**: `/agent` side-channels do NOT use `child()`. They use `CancelToken::new()` — a completely independent token with no parent reference. Per the user-confirmed semantics, **main's Cmd-C does NOT propagate to side channels**. The user has to explicitly `/agent cancel <id>` to cancel a side channel.

`child()` exists in the API for the eventual subagent rewrite (where Task subagents could observe parent's cancel transitively but a child failure shouldn't kill siblings during a parallel fan-out). That work hasn't landed yet.

## 6. `ViewEvent` plumbing

Five variants in `shared_session::ViewEvent`:

```rust
SideChannelStart { id: String, agent_name: String }
SideChannelTextDelta { id: String, text: String }
SideChannelToolCall { id: String, tool_name: String, label: String }
SideChannelDone { id: String, agent_name: String, duration_ms: u64, result_text: String }
SideChannelError { id: String, error: String }
```

`event_render::render_chat_dispatches` emits one IPC envelope per variant:

| ViewEvent | Frontend envelope `type` |
|---|---|
| `SideChannelStart` | `chat_side_channel_start` |
| `SideChannelTextDelta` | `chat_side_channel_text_delta` |
| `SideChannelToolCall` | `chat_side_channel_tool_call` |
| `SideChannelDone` | `chat_side_channel_done` |
| `SideChannelError` | `chat_side_channel_error` |

`event_render::render_terminal_ansi` emits one-line ANSI markers:

- `SideChannelStart` → `\r\n[2m[agent translator (side-abc) — running in background][0m\r\n`
- `SideChannelTextDelta` / `_ToolCall` → None (silenced — too noisy without a separate panel)
- `SideChannelDone` → cyan status header + dim-italic full result body
- `SideChannelError` → red one-line marker

This means a CLI user (`thclaws --cli`) running `/agent` (which is GUI-only and prints a "GUI-only" hint) would never see these events anyway. But if the worker is running in `--gui` mode and the user is also looking at a Terminal pane on the same session, they'd see the markers.

## 7. Slash-command surface

| Slash | `SlashCommand` variant | Behavior |
|---|---|---|
| `/agent <name> <prompt>` | `Agent { name, prompt }` | GUI: `spawn_side_channel(...)`. CLI: prints "GUI-only" hint |
| `/agents` | `AgentsList` | List active side channels (id, agent_name, elapsed) |
| `/agent cancel <id>` | `AgentCancel(id)` | Fire `cancel_side_channel(id)`; spawn task picks up cancel via `select!` |

Parser (`parse_agent_subcommand` in repl.rs) recognizes `cancel <id>` first so "cancel" can't be misread as an agent name. Bare `/agent`, `/agent <name>` (no prompt), and `/agent cancel` (no id) all return `Unknown` with usage hints.

REPL dispatch path: prints "GUI-only" hint on both `gui` feature off (thclaws-cli binary) AND gui feature on but CLI mode (`thclaws --cli`). The CLI REPL doesn't have a broadcast surface to fan side-channel events through, so even with the side_channel module compiled in, there's nowhere for the events to go.

GUI Chat dispatch path (`shell_dispatch.rs`):

```rust
SlashCommand::Agent { name, prompt } => {
    match crate::side_channel::spawn_side_channel(
        name.clone(),
        prompt,
        state.agent_factory.clone(),       // populated at worker init
        state.agent_defs.clone(),          // same
        events_tx.clone(),                 // broadcast for SideChannel* events
    ).await {
        Ok(id) => emit(events_tx, format!("✓ spawned background agent '{name}' (id: {id})")),
        Err(e) => emit(events_tx, format!("/agent: {e}")),
    }
}
```

`state.agent_factory` and `state.agent_defs` were added to `WorkerState` specifically to support this — they're populated at worker init in `run_worker`, captured from the same `ProductionAgentFactory` already used to register the SubAgentTool.

## 8. Frontend integration

### ApprovalModal — origin tagging

```ts
type AgentOrigin =
  | { kind: "main" }
  | { kind: "side_channel"; id: string; agent_name: string }
  | { kind: "subagent"; agent_name: string; depth: number };

function originLabel(o: AgentOrigin): string;   // "Main" / "translator (background)" / "researcher (subagent · depth 1)"
function originAccent(o: AgentOrigin): string;  // accent / amber / blue
```

Modal header changes from `"Agent wants to run <tool>"` to `"<originLabel(originator)> wants to run <tool>"`. The accent dot color matches `originAccent` so concurrent permission requests are visually distinct in the queue.

`PendingRequest.originator` defaults to `{ kind: "main" }` if the field is missing — back-compat for any backend version that doesn't yet emit it.

### ChatView — side-channel bubble

```ts
type ChatMessage = {
    role: "user" | "assistant" | "tool" | "system";
    // ... existing fields
    sideChannel?: {
        id: string;
        agentName: string;
        status: "running" | "done" | "error";
        result: string;
        durationMs?: number;
        error?: string;
        startedAt: number;
    };
};
```

One bubble per side-channel spawn. On `chat_side_channel_start` push a new `system`-role message with `sideChannel` populated. On subsequent events update the same message in place by matching `sideChannel.id`.

Bubble layout:

- Bordered card, left accent stripe color-coded by status:
  - amber `#d97706` while `status: "running"`
  - green `var(--accent-success)` on `done`
  - red `var(--accent-error)` on `error`
- Header: `● agent: <name> (<id>)   <status line>`
  - `▸ running…` (running)
  - `✓ done in 5m 23s` (done — uses `formatDuration(ms)` helper)
  - `✗ <error>` (error / cancel)
- Body: monospace block with accumulated stream text (running) or full result (done)

The bubble renders BEFORE the existing `tool` / `assistant` / `user` branches in `messages.map`, since `sideChannel` is the diagnostic signal regardless of `role`.

## 9. WorkerState plumbing

Two new fields:

```rust
pub struct WorkerState {
    // ... existing fields
    pub agent_factory: std::sync::Arc<dyn crate::subagent::AgentFactory>,
    pub agent_defs: crate::agent_defs::AgentDefsConfig,
}
```

Both populated at worker init (`shared_session::run_worker`). The factory is built once and used twice:

1. Wrapped in a `SubAgentTool` and registered into the main agent's `ToolRegistry` (existing behavior, unchanged)
2. Stashed on `WorkerState` so the slash dispatch can reuse it for side-channel spawns

The `ProductionAgentFactory` itself is intentionally cheap to clone — it's just `Arc`s for `provider`, `base_tools`, `agent_defs`, `approver`, plus copy-able `model`, `system`, `permission_mode`, `cancel`, `hooks`. So the per-spawn clone in `shell_dispatch` doesn't allocate meaningfully.

## 10. Test surface

`cargo test --features gui --lib` runs everything. Side-channel related tests:

**Rust unit / integration (in `side_channel.rs`)**:
- `spawn_emits_start_text_done_events` — InlineProvider (defined inline) drives a child Agent emitting one TextDelta + Done. Test asserts the side-channel's events come through `events_tx` in the right order: Start → TextDelta → Done with the correct id and result_text. Also asserts the registry empties out after exit.
- `spawn_unknown_agent_errors` — `agent_defs` doesn't contain the requested name → error result with "unknown agent" message; no event fired.
- `list_returns_active_channels` — manually inserts a SideChannelHandle into the registry and asserts list returns the expected (id, name, elapsed) tuple.
- `cancel_returns_false_for_unknown` — `cancel_side_channel("does-not-exist")` returns false without panic.

**Rust parser tests (in `repl.rs`)**:
- `parse_slash_agent_basic` — `/agent translator แปลไฟล์ x` → `Agent { name: "translator", prompt: "แปลไฟล์ x" }`. Multi-word prompts preserved.
- `parse_slash_agent_no_prompt_errors` — `/agent translator` (no prompt) → `Unknown` with "prompt cannot be empty".
- `parse_slash_agent_bare_errors` — `/agent` alone → `Unknown` with "usage: /agent".
- `parse_slash_agents_list` — `/agents` → `AgentsList`.
- `parse_slash_agent_cancel` — `/agent cancel side-abc` → `AgentCancel("side-abc")`.
- `parse_slash_agent_cancel_no_id_errors` — `/agent cancel` → `Unknown` with usage hint.

**Rust permissions tests (in `permissions.rs`)**:
- `agent_origin_default_is_main` — `AgentOrigin::default() == AgentOrigin::Main`.
- `agent_origin_serializes_with_kind_tag` — round-trip serde for all three variants.
- `gui_approver_propagates_side_channel_originator` — sets `originator: SideChannel { id, agent_name }` on the request, asserts the GuiApprovalRequest received by the frontend tx carries the same fields.

**Rust cancel tests (in `cancel.rs`)** — for `child()`, not directly used by side-channel but covers the underlying primitive:
- `child_observes_parent_cancel`, `child_cancel_does_not_propagate_to_parent`, `sibling_children_are_independent`, `child_cancelled_wakes_on_parent_cancel`, `child_cancelled_wakes_on_own_cancel`.

Total: ~17 new tests across the four modules. Full lib suite: 950 passing post-merge.

**Test-time isolation patterns**:
- `side_channel.rs` tests use a custom `InlineProvider` (defined inline in the test mod) to avoid pulling in the real provider stack.
- Each test clears the registry singleton on entry (`registry().lock().clear()`) to avoid cross-test bleed since the singleton outlives any single test.
- Empty `AgentDefsConfig::default()` + spread syntax for AgentDef literals — keeps fixtures minimal.

**Frontend tests**: none yet for the new modal / ChatView code. Existing modal patterns rely on integration testing; the new code follows the same shape so a regression in the routing path would surface via the existing approval-modal integration tests if any are added.

## 11. Known gaps / future work

| | Description | Workaround / follow-up |
|---|---|---|
| **Subagent depth tagging** | `Agent::with_origin(Subagent { ... })` exists in API but `SubAgentTool::call` doesn't set it — Task subagents currently tag as `Main` | Wire through `ProductionAgentFactory::build` so child agents inherit the right origin with depth |
| **CLI `/agent` UX** | CLI REPL prints "GUI-only" hint — the broadcast surface required to fan side-channel events isn't available | Could add a CLI-side surface where each side-channel runs in a tmux pane; substantial UX project |
| **Side-channel from `--serve`** | Should work (server.rs uses the same `shell_dispatch`) but not explicitly tested | Add a serve integration test once the WS frontend exists for browser users |
| **Persistent threads** | Side channels are fire-and-forget. After `Done`, the agent state is dropped — user can't follow up to the same agent | Build a `ThreadStore` keyed by SideChannelId that retains the Agent + history; add `/agent followup <id> <prompt>` slash. Substantial — see [docs/claude-multiagent-vs-thclaws-th.md](../docs/claude-multiagent-vs-thclaws-th.md) §10 for the design space |
| **Parallel fan-out from `Task` tool** | The model can't spawn N parallel subagents — Task is sequential. /agent works around this for user-driven cases but not for model-driven ones | Add a `TaskParallel` tool that takes an array of (agent, prompt) and uses similar concurrent primitives |
| **Cancel-all** | No `/agent cancel-all` shortcut | Trivial to add — iterate registry, fire each cancel |
| **Approval routing edge case** | `GuiApprover` is a single shared instance; if 5 side channels all ask for permission at once, the modal queue serializes them visually but they're ALL still pending. The user sees a stack of "translator (background)", "researcher (background)", etc. — which is correct, but `+N more pending` count grows fast | The current modal already shows "+N more pending"; fine. If it becomes a real UX issue, build a per-agent collapsible queue panel |
| **Logs** | Side-channel output isn't persisted to disk like `/schedule run` jobs | Could persist `result_text` to `~/.local/share/thclaws/agent-history/<id>.log` if user wants |

## 12. What lives where (source-line index)

| Concern | File | Anchor |
|---|---|---|
| Spawn function | `crates/core/src/side_channel.rs` | `spawn_side_channel` |
| Registry singleton | same | `registry()` |
| Cancel + list helpers | same | `cancel_side_channel`, `list_side_channels` |
| Stream loop (inside spawn task) | same | `tokio::spawn(async move { ... })` block |
| `AgentOrigin` enum | `crates/core/src/permissions.rs` | search "pub enum AgentOrigin" |
| `originator` field | same | `pub struct ApprovalRequest` |
| `GuiApprovalRequest.originator` | same | `pub struct GuiApprovalRequest` |
| `Agent::origin` field + `with_origin` | `crates/core/src/agent.rs` | search "origin: AgentOrigin" |
| Tool dispatch reading origin | same | `originator: origin.clone()` near line 1300 |
| `CancelToken::child()` | `crates/core/src/cancel.rs` | `pub fn child` |
| `ViewEvent::SideChannel*` | `crates/core/src/shared_session.rs` | search "SideChannelStart" |
| Chat envelope render | `crates/core/src/event_render.rs` | `chat_side_channel_*` |
| Terminal ANSI render | same | search "SideChannelStart" / "SideChannelDone" |
| `WorkerState.agent_factory` + `.agent_defs` | `crates/core/src/shared_session.rs` | end of `pub struct WorkerState` |
| Worker init wiring | same | `run_worker` — search "factory_state" |
| `SlashCommand::Agent / AgentsList / AgentCancel` | `crates/core/src/repl.rs` | search "ScheduleUninstall," |
| Parser | same | `parse_agent_subcommand` |
| REPL dispatch (CLI hint) | same | search "SlashCommand::Agent { name, prompt } =>" |
| GUI dispatch (real spawn) | `crates/core/src/shell_dispatch.rs` | search "SlashCommand::Agent { name, prompt } =>" |
| /help text | `crates/core/src/repl.rs` | `render_help` — search "/agent NAME PROMPT" |
| Frontend `AgentOrigin` type | `frontend/src/components/ApprovalModal.tsx` | top of file |
| `originLabel`, `originAccent` | same | top of file |
| Modal header rendering | same | search "originLabel(current.originator)" |
| `ChatMessage.sideChannel` field | `frontend/src/components/ChatView.tsx` | type `ChatMessage` |
| Side-channel subscription arms | same | search "chat_side_channel_start" |
| Side-channel bubble render | same | search "if (msg.sideChannel)" |
| `formatDuration` helper | same | top of file |
