# Running modes

thClaws ships **one engine, four surfaces**. The same `Agent` loop, `Session` JSONL, `ToolRegistry`, permission gate, hooks subsystem, KMS, memory, plan mode, subagent, and team primitives back every running mode — they only differ in how the user feeds input and how output flows back.

The four modes:

| Mode | Binary + flag | Surface | Process model |
|---|---|---|---|
| **GUI** (default) | `thclaws` | wry+tao desktop window with React UI | tao event loop on main thread + tokio worker thread |
| **CLI REPL** | `thclaws-cli` OR `thclaws --cli` | rustyline interactive terminal | single tokio runtime; readline on a `spawn_blocking` thread |
| **Headless / print** | `thclaws-cli -p "..."` OR `thclaws -p "..."` | non-interactive, one-shot prompt | single tokio runtime; exits when the turn completes |
| **Web (`--serve`)** | `thclaws --serve --port <N>` | Axum HTTP + WebSocket; React UI in browser | tokio runtime hosting Axum + the same shared worker the GUI uses |

This doc covers: the shared engine that all four modes consume, the per-mode process model + dispatch chain, the binary + feature-flag layout (`thclaws` vs `thclaws-cli`, `gui` feature gate), how each mode handles input + output + approvals + hooks + cancellation, when to pick which, deployment patterns, and the M6.36 SERVE9 architecture invariant (`crate::ipc::handle_ipc` returning `bool` so wry GUI delegates to the shared dispatch then falls through for wry-only arms).

**Source modules:**
- `crates/core/src/bin/app.rs` — unified `thclaws` binary; parses CLI flags + dispatches to GUI / serve / CLI / print
- `crates/core/src/bin/cli.rs` — CLI-only `thclaws-cli` binary (no `gui` feature; never enters GUI or serve)
- `crates/core/src/repl.rs` — CLI REPL (`run_repl`) + print mode (`run_print_mode`)
- `crates/core/src/gui.rs` — wry+tao desktop GUI (`#[cfg(feature = "gui")]`-gated; `run_gui`)
- `crates/core/src/server.rs` — Axum HTTP + WebSocket serve mode (`#[cfg(feature = "gui")]`-gated; `run`)
- `crates/core/src/shared_session.rs` — `SharedSessionHandle` (input_tx + events_tx + cancel + ready_gate) consumed by GUI + serve
- `crates/core/src/ipc.rs` — `IpcContext` + `handle_ipc(msg, ctx) -> bool` transport-agnostic dispatch consumed by GUI + serve
- `crates/core/src/event_render.rs` — `ViewEvent → JSON envelope` translator consumed by GUI + serve
- `crates/core/src/agent.rs` — `Agent::run_turn` — the loop every mode runs
- `crates/core/src/session.rs` — JSONL persistence shared across modes
- `crates/core/src/config.rs` — `AppConfig::load` layering (compiled defaults → user → project → CLI flags)
- `crates/core/Cargo.toml` — `gui` feature gate covering `tao` + `wry` + `comrak` + `rfd` + `native-dialog`

**Cross-references:**
- [`app-architecture.md`](app-architecture.md) — the three-surface diagram + `WorkerState` + `ShellInput`/`ViewEvent` channel topology
- [`agentic-loop.md`](agentic-loop.md) — `Agent::run_turn` per-turn pipeline that every mode runs
- [`serve-mode.md`](serve-mode.md) — `--serve` mode in detail (deploy workflow, trust model, snapshot semantics)
- [`sessions.md`](sessions.md), [`hooks.md`](hooks.md), [`plan-mode.md`](plan-mode.md), [`agent-team.md`](agent-team.md) — every project subsystem that flows through unchanged regardless of mode

---

## 1. The shared engine

All four modes consume the same engine surface. Picking a different mode does NOT change which model runs, which tools are available, which sessions persist, or which approvals fire — it only changes the wrapper that feeds input + renders output.

```
                    ┌──────────────────────────────────────────────────────────────────┐
                    │                       SHARED ENGINE                              │
                    │                                                                  │
                    │   crate::agent::Agent           — run_turn loop, retry, compact  │
                    │   crate::session::Session       — JSONL replay + per-turn save   │
                    │   crate::tools::ToolRegistry    — built-ins + MCP + skills       │
                    │   crate::permissions::*         — approval gate (Auto/Ask/Plan)  │
                    │   crate::hooks::*               — pre/post tool, session, ...    │
                    │   crate::memory + kms           — system-prompt context layer    │
                    │   crate::config::AppConfig      — settings.json layering         │
                    │   crate::tools::plan_state::*   — sequential plan gate           │
                    │   crate::subagent + team        — recursive + multi-process      │
                    │                                                                  │
                    └──────────────────────────────────────────────────────────────────┘
                              ▲                ▲                ▲                ▲
                              │                │                │                │
                    ┌─────────┴────┐ ┌─────────┴────┐ ┌────────┴─────┐ ┌────────┴────┐
                    │  GUI         │ │  CLI REPL    │ │  Headless    │ │  Web        │
                    │  (wry + tao) │ │  (rustyline) │ │  (one-shot)  │ │  (Axum + WS)│
                    │  gui.rs      │ │  repl.rs     │ │  repl.rs     │ │  server.rs  │
                    └──────────────┘ └──────────────┘ └──────────────┘ └─────────────┘
```

Two engines emerge from this:

- **`Agent::run_turn`** — the per-turn LLM call loop ([`agentic-loop.md`](agentic-loop.md)). All four modes invoke it the same way. It produces an async stream of `AgentEvent`s (text deltas, tool calls, tool results, done).
- **`SharedSessionHandle`** (gui + serve only) — wraps `Agent` + `Session` + the input mpsc + the events broadcast for multi-tab consumers. CLI + print drive `Agent::run_turn` directly without this wrapper.

---

## 2. Mode selection — when to pick which

| Need | Pick | Reason |
|---|---|---|
| Local interactive desktop work | **GUI** | Best UX: chat tab + terminal tab + plan sidebar + KMS sidebar + file browser, all native window |
| Pipe input/output, scripting, CI | **Headless** | One-shot, no UI, exits with status code |
| SSH session on a server, no display | **CLI REPL** | Same conversational shape as the GUI's chat tab, terminal-rendered |
| Remote access from phone / laptop browser, single user | **Web** | Project folder is the deploy unit; SSH tunnel handles auth |
| Hosting N projects on one server | **Web** ×N | Multiple `--serve` processes on different ports |
| Embedding agent in an existing tool / pipeline | **Headless** | stream-json output format for line-buffered consumers |
| Restricted / sandboxed CI box without GTK/WebKit | **CLI** (`thclaws-cli`) | Zero GUI dependencies; smaller binary |

You can mix modes against the same project — desktop GUI at home, `--serve` from your VPS, occasional `thclaws-cli -p` for a quick scripted query — all share the same `.thclaws/` state directory, so sessions / plans / KMS / todos / hooks carry transparently.

---

## 3. Binary + feature layout

Two binaries are built from one library crate:

```
thclaws/crates/core/Cargo.toml

[features]
default = []
gui = ["dep:tao", "dep:wry", "dep:comrak", "dep:rfd", "dep:native-dialog"]

[[bin]]
name = "thclaws"        path = "src/bin/app.rs"     # unified — supports CLI/print/serve/GUI
[[bin]]
name = "thclaws-cli"    path = "src/bin/cli.rs"     # CLI-only — no GUI deps, no --serve
```

| Binary | Default build | With `--features gui` | Use case |
|---|---|---|---|
| `thclaws-cli` | CLI + print | (gui feature ignored — binary doesn't ship the wry/serve paths) | Headless servers, sandboxed CI, smallest install |
| `thclaws` (no feature) | CLI + print only | — | Rare: when you want the unified flag surface but don't need GUI/serve |
| `thclaws` `--features gui` | All four modes | All four modes | Default desktop install + `make serve` |

The `gui` feature pulls in `tao` (windowing), `wry` (webview), `comrak` (markdown→HTML for the file preview pipeline), `rfd` (native file dialog), and `native-dialog` (Windows confirm). It also enables compilation of `crate::gui` + `crate::shared_session` + `crate::ipc` + `crate::server` + `crate::event_render` + `crate::file_preview` + `crate::shell_dispatch` modules — all the modules that touch wry, ViewEvent broadcasting, or comrak rendering.

`make build` produces both binaries by default; the `Makefile` documents per-binary targets (`build-cli`, `build-app`).

---

## 4. Dispatch chain — how a CLI flag selects the mode

The unified `thclaws` binary's `main()` (`bin/app.rs`) parses CLI flags via clap, then short-circuits in this order:

```rust
// bin/app.rs (simplified)
let cli = Cli::parse();

// 1. --serve (M6.36) wins over everything else.
if cli.serve {
    #[cfg(feature = "gui")]
    return server::run(ServeConfig { bind, port }).await;
    #[cfg(not(feature = "gui"))]
    eprintln!("--serve requires --features gui"); std::process::exit(1);
}

// 2. Default: GUI (when feature compiled in + neither --cli nor --print).
if !cli.cli && !cli.print {
    #[cfg(feature = "gui")]
    { detach_console(); gui::run_gui(); return; }
    #[cfg(not(feature = "gui"))]
    eprintln!("GUI not available — use --cli"); std::process::exit(1);
}

// 3. Print mode (--print) — one-shot, exits when done.
if cli.print {
    return run_print_mode(config, &prompt).await;
}

// 4. CLI REPL (--cli or --print).
return run_repl(config).await;
```

The CLI-only `thclaws-cli` binary (`bin/cli.rs`) has a simpler dispatch — no `--serve`, no `--cli`, no GUI escape:

```rust
// bin/cli.rs (simplified)
if cli.print {
    return run_print_mode(config, &prompt).await;
}
return run_repl(config).await;
```

---

## 5. Per-mode internals

### 5.1 GUI mode

**Entry**: `gui::run_gui()` — invoked from `bin/app.rs` when no flag short-circuits earlier and `--features gui` is built in.

**Process model**:
- **Main thread**: tao event loop. Owns the wry webview, processes window events (close, resize, keyboard), receives `UserEvent` enum values from background threads via `EventLoopProxy::send_event`.
- **Worker thread**: tokio runtime hosting `shared_session::run_worker`. Owns `WorkerState` (Agent + Session + tool_registry + skill_store + mcp_clients + lead_log). Receives `ShellInput` over a `std::sync::mpsc` channel; broadcasts `ViewEvent`s to subscribers via `tokio::sync::broadcast`.
- **Translator thread**: subscribes to the worker's `events_tx`, runs `crate::event_render::{render_chat_dispatches, render_terminal_ansi}` per ViewEvent, fans the resulting JSON envelopes out to the main thread as `UserEvent::Dispatch(payload)` for `webview.evaluate_script("__thclaws_dispatch(...)")`.

**Inbound IPC**: wry's `with_ipc_handler` closure receives `window.ipc.postMessage(json)` calls from the React frontend. Pre-M6.36 this closure had a 1600-LOC `match ty` block. M6.36 SERVE1+9 promoted that to `crate::ipc::handle_ipc`; the closure now delegates first:

```rust
.with_ipc_handler(move |req| {
    let msg = serde_json::from_str(req.body())?;
    let ipc_ctx = ipc::IpcContext { /* wry-flavored */ };
    if ipc::handle_ipc(msg.clone(), &ipc_ctx) {
        return; // shared dispatch handled it
    }
    // Fall through for wry-only arms (gui_scale_get, pick_directory,
    // confirm, pty_*, the static-asset MIME-type arms — those stay
    // because they have no web equivalent).
    let ty = msg.get("type")...;
    match ty {
        "gui_scale_get" => { ... }
        "pick_directory" => { ... }
        // ...
    }
})
```

**Frontend protocol**: same JSON message envelopes the WS transport in `--serve` mode uses. `useIPC.ts` detects `window.ipc` (wry) vs WebSocket (web) at module-load and picks the right `send()` / `subscribe()`.

**Approval surface**: `GuiApprover` — frontend modal popup. The `approval_request` IPC envelope carries an `id`; the user's click → `approval_response` with the same id → resolves the worker's pending oneshot. AskUserQuestion uses the same pattern via `pending_asks: HashMap<u64, oneshot::Sender<String>>`.

**Tools available**: full set including the desktop-only File browser tab (`file_list` / `file_read` / `file_write` IPCs), Team tab (`team_list` / `team_send_message`), MCP-Apps widgets (iframe alongside chat tool results).

**Cancellation**: `CancelToken` shared across worker + agent. Sidebar Cancel button → `shell_cancel` IPC → `shared.request_cancel()` → cancel propagates into the agent's retry-backoff sleeps and the streaming collector.

### 5.2 CLI REPL mode

**Entry**: `repl::run_repl(config)` — invoked from either binary when `--cli` is set (or by default from `thclaws-cli`).

**Process model**:
- **Single tokio runtime** running `run_repl`'s main loop.
- **Readline thread**: `tokio::task::spawn_blocking` wraps `rustyline::Editor::readline` so it doesn't block tokio. The blocking task returns `Some(line)` / `None` (EOF).
- **Main loop**: `tokio::select!` between the readline future + (when team mode active) the lead inbox poller's mpsc + (M6.29) the `/loop` firing channel.

**Inbound input**: keyboard via rustyline. Slash commands (`/help`, `/sessions`, `/model`, `/plan`, `/loop`, etc.) parsed by `repl.rs::SlashCommand` enum. Plain text → user message → `Agent::run_turn(prompt).await`.

**Outbound rendering**: streams `AgentEvent`s from `Agent::run_turn`, emits ANSI-colored text directly to stdout. No event broadcast / event-translator — the CLI is single-consumer.

**Approval surface**: `ReplApprover` — interactive y/n/s prompt at the terminal. `s` = "allow for session" (yolo flag persists for the rest of the REPL session).

**Tools available**: same agent loop + tool registry as GUI, but no Files tab / MCP-Apps widgets / Team tab UI affordances. Slash commands cover most settings actions (`/model`, `/permissions`, `/plan`, `/kms`, etc.).

**Cancellation**: `tokio::signal::ctrl_c()` in the inbound `select!` — ctrl-C aborts the active turn cleanly, returns to the prompt.

### 5.3 Headless / print mode

**Entry**: `repl::run_print_mode(config, prompt)` — invoked when `--print` (or `-p`) is set with a positional prompt.

**Process model**: single tokio runtime. Builds the same `Agent` (with the same hooks, same approval mode, same tool registry as the REPL) and runs ONE turn:

```rust
let mut stream = agent.run_turn(prompt);
while let Some(ev) = stream.next().await {
    // Render to stdout (text mode) or emit JSON envelopes (stream-json mode)
}
process::exit(if any_error { 1 } else { 0 });
```

**Inbound input**: a single positional prompt arg (or stdin via `--input-format stream-json`). No interactive readline.

**Outbound rendering**:
- `--output-format text` (default) — plain text deltas to stdout
- `--output-format stream-json` — line-delimited JSON envelopes for pipeline consumers (one per `AgentEvent`)

**Approval surface**: defaults to `--accept-all` (Auto mode) implicit unless you pass `--permission-mode ask`. Ask mode in print mode pipes the approval request to stderr + reads y/n from stdin — useful for human-in-the-loop scripts but rare in practice.

**Tools available**: same engine, same registry. Bash / Edit / Write / Read all work.

**Use cases**: scripting (`thclaws-cli -p "summarize this file: $(cat report.md)"`), CI (`thclaws-cli -p "review the diff: $(git diff)"`), one-shot pipeline integration.

**Exit code**: 0 on success, 1 on any agent error / config error / provider failure.

### 5.4 Web (`--serve`) mode

Full coverage in [`serve-mode.md`](serve-mode.md). Summary:

**Entry**: `server::run(ServeConfig { bind, port })` — invoked from `bin/app.rs` when `--serve` is set (requires `--features gui`).

**Process model**: tokio runtime hosting Axum HTTP + the same `SharedSessionHandle` worker the GUI uses. One process per project (cd into project dir before running). Bind defaults to `127.0.0.1` (Phase 1 trust model: SSH tunnel handles auth).

**Routes**:
- `GET /` — serves embedded React `index.html` (same single-file vite build the desktop GUI embeds)
- `GET /healthz` — liveness probe
- `GET /ws` — WebSocket upgrade

**Per-WS-connection**:
- Subscribes to `events_tx`, runs the same `event_render` translators, forwards JSON envelopes via per-connection `mpsc → sink writer task` (serializes WS writes)
- Inbound JSON frames parsed → `crate::ipc::handle_ipc(msg, ctx)` with WS-flavored `IpcContext` (dispatch closure → `out_tx`; on_quit → log + close; on_send_initial_state → stub today, snapshot frame in future)

**Frontend**: `useIPC.ts` detects no `window.ipc` → uses WebSocket. Auto-reconnect with exponential backoff (250ms → 5s cap); emits `{type: "ws_status", status}` synthetic events for "reconnecting…" banner.

**Approval surface**: same `GuiApprover` plumbing as the desktop GUI — frontend modal, response routes back via `approval_response` IPC. Identical UX in browser as in desktop.

**Tools available**: same registry. The wry-only tools (file dialog, native confirm, GUI zoom) don't apply — frontend handles those via web equivalents (HTML5 file input, `window.confirm()`, browser zoom).

---

## 6. The architectural invariant — `handle_ipc` returns bool

M6.36 SERVE9 set up an invariant that lets GUI + serve share the same dispatch table:

```rust
// crate::ipc::handle_ipc returns true if it recognized + dispatched
// the message; false if the type wasn't one of the migrated arms.
#[must_use]
pub fn handle_ipc(msg: Value, ctx: &IpcContext) -> bool { ... }
```

- **Web (`--serve`)** ignores the return value — `handle_ipc` IS the dispatch surface; nothing else is wired. Anything not handled is silently dropped.
- **GUI (`gui.rs`)** consults the return — `if handle_ipc(msg, &ctx) { return }`, otherwise falls through to its own `match ty` for wry-only arms (`gui_scale_get`, `gui_set_zoom`, `pick_directory`, `confirm`, `pty_*`, MIME-type asset arms).

This kept the migration risk-bounded: each arm migrated from gui.rs to ipc.rs in isolation, with the boolean signal as the hand-off contract. `cargo test` between batches caught any regression.

After SERVE9k cleanup, **50 IPC arms** live in `handle_ipc`; ~7 wry-only arms remain in `gui.rs` (correctly, since they need wry primitives that have no web equivalent).

---

## 7. What's shared, what's not

| Concern | GUI | CLI | Print | Web |
|---|---|---|---|---|
| `Agent::run_turn` | ✓ | ✓ | ✓ | ✓ |
| `Session` JSONL persistence | ✓ | ✓ | ✓ | ✓ |
| Tool registry (built-ins + MCP + skills) | ✓ | ✓ | ✓ | ✓ |
| Permissions / approval gate | ✓ (modal) | ✓ (terminal y/n) | ✓ (auto by default) | ✓ (modal in browser) |
| Hooks (pre/post tool, session, etc.) | ✓ | ✓ | ✓ | ✓ |
| Memory + KMS | ✓ | ✓ | ✓ | ✓ |
| Plan mode | ✓ (sidebar buttons) | ✓ (`/plan` slash) | — (single-shot; plan mode doesn't fit) | ✓ (sidebar buttons in browser) |
| Subagent (`Task`) | ✓ | ✓ | ✓ | ✓ |
| Team subprocess | ✓ (Team tab) | ✓ (lead inbox poller in REPL) | — | ✓ (Team tab in browser) |
| `SharedSessionHandle` worker | ✓ | — (drives Agent directly) | — | ✓ (same instance pattern as GUI) |
| Multi-tab consistency | ✓ (Terminal + Chat tabs share state) | — (single tab) | — | ✓ (multiple browser tabs share state) |
| File browser tab | ✓ | — | — | ✓ |
| MCP-Apps widgets | ✓ (iframe in chat) | — | — | ✓ (iframe in browser chat) |
| Native dialogs (`confirm`, `pick_directory`) | ✓ | — | — | — (frontend uses HTML5 equivalents) |
| Webview zoom | ✓ | — | — | — (browser handles it) |
| OS keychain for secrets | ✓ (default) | ✓ (default) | ✓ (default) | — (`THCLAWS_DISABLE_KEYCHAIN=1` default; use `.thclaws/.env`) |
| Auto-reconnect on transport drop | — (in-process IPC) | — (no transport) | — | ✓ (WS reconnect with backoff) |

---

## 8. Configuration loading

`AppConfig::load` runs the same layering in every mode:

1. Compiled defaults (`AppConfig::default`)
2. User settings (`~/.config/thclaws/settings.json`)
3. Project settings (`<cwd>/.thclaws/settings.json`)
4. CLI flag overrides (`--model`, `--permissions`, `--allowed-tools`, etc.)

Mode-specific overlays:
- **GUI**: also reads `~/.config/thclaws/theme.json` (light/dark/system) + `~/.config/thclaws/recent_dirs.json` (workspace picker history). Runtime mutations (`/kms use`, `/model`, theme switch) write to project / user settings + fire `ShellInput::ReloadConfig`.
- **CLI**: same loading; runtime mutations via slash commands write back the same way.
- **Print**: loads + applies CLI overrides; never mutates settings.
- **Web**: same loading. `THCLAWS_DISABLE_KEYCHAIN=1` is set automatically (keychains absent on most server boxes); secrets resolved via `.thclaws/.env` instead.

API keys: OS keychain on desktop (auto), `.thclaws/.env` (project-scoped) or `~/.config/thclaws/.env` (user-scoped) for headless / serve. See [`hooks.md`](hooks.md) §7 + [`serve-mode.md`](serve-mode.md) §3.

---

## 9. Build matrix

```bash
# Default — both binaries with GUI feature
make build

# CLI-only binary (no GUI deps, smallest)
make build-cli                     # produces target/release/thclaws-cli
# OR: cd thclaws/crates/core && cargo build --bin thclaws-cli --release

# Unified binary (CLI + GUI + serve, requires GUI feature)
make build-app                     # produces target/release/thclaws
# OR: cd thclaws/crates/core && cargo build --features gui --bin thclaws --release

# Frontend bundle (required for GUI + serve modes — embedded via include_str!)
make build-frontend
# OR: cd thclaws/frontend && pnpm build

# Run serve mode
make serve PORT=8443 BIND=127.0.0.1
# OR: thclaws --serve --port 8443 --bind 127.0.0.1

# Run desktop GUI
thclaws

# Run interactive CLI REPL
thclaws --cli      # OR: thclaws-cli

# One-shot prompt (headless)
thclaws -p "what's in package.json?"          # unified binary
thclaws-cli -p "what's in package.json?"      # CLI-only binary
```

`thclaws-cli` is preferable on headless servers / CI / containers — it doesn't pull in `tao` / `wry` / `comrak` / `rfd` / `native-dialog`, so the binary is significantly smaller and the dependency closure has no GUI / WebKit2GTK / GTK requirements.

---

## 10. Mixing modes against the same project

The four modes share `.thclaws/` state, so cross-mode workflows compose:

```
LOCAL (desktop GUI)               CLOUD (web)                    ANYWHERE (CLI)
~/projects/foo/                   /srv/agents/foo/               ssh server cd /srv/agents/foo
  .thclaws/                         .thclaws/                      thclaws-cli -p "review"
    sessions/                         sessions/   ← rsync ←
    plans/...                         plans/...
    todos.md                          todos.md
    team/                             team/
    kms/                              kms/
```

All flows write to the same on-disk JSONL session files + plan state. Concurrent writes from multiple modes against the same active session aren't serialized — only one mode should be actively driving a given session at a time. Hand-off works via rsync / git push between machines, OR via session swap inside one mode (`/load <id>` switches active session; the prior session is autosaved before the swap).

The desktop GUI + serve mode add live multi-tab consistency on top of the JSONL: changes broadcast via `events_tx` to every subscribed UI surface in the same process. The CLI REPL doesn't subscribe — it just renders its own turn output.

---

## 11. Known gaps + design decisions

- **Print mode + plan mode don't combine** — plan mode requires the sidebar buttons (Approve / Cancel / Retry / Skip / Continue) which print mode has no UI for. CLI's `/plan enter` works as a one-shot mode flip but the per-step Ralph driver lives in the GUI worker — CLI plan mode requires manual "continue" between steps. Documented as plan-mode PM5 deferred gap.
- **`thclaws-cli` doesn't ship `--serve`** — `--serve` requires `--features gui` (it shares the `WorkerState` engine with the GUI via `crate::shared_session`, which is gui-feature-gated). Building `thclaws --serve` is the canonical path.
- **No headless serve mode (no GUI feature, but Axum)** — would need `shared_session` un-gated from the `gui` feature, which is a larger refactor. Today `--serve` IS the "GUI without a window."
- **No web equivalent of native dialogs** — `pick_directory` (rfd) + `confirm` (native_dialog) wouldn't make sense in a browser. Frontend handles these via HTML5 file input + `window.confirm()`. The wry-only arms remain in `gui.rs` by design.

---

## 12. What lives where (source-line index)

| Concern | File | Symbol |
|---|---|---|
| Unified binary entry | `bin/app.rs` | `main` (parses flags, dispatches by mode) |
| CLI-only binary entry | `bin/cli.rs` | `main` (CLI + print only) |
| GUI dispatch | `gui.rs` | `pub fn run_gui()` |
| Serve dispatch | `server.rs` | `pub async fn run(config: ServeConfig)` |
| CLI REPL dispatch | `repl.rs` | `pub async fn run_repl(config: AppConfig)` |
| Print mode dispatch | `repl.rs` | `pub async fn run_print_mode(config, prompt)` |
| Shared session worker | `shared_session.rs::run_worker` | gui-gated; consumed by GUI + serve |
| Transport-agnostic IPC | `ipc.rs` | `IpcContext`, `handle_ipc(msg, ctx) -> bool` |
| ViewEvent → JSON translator | `event_render.rs` | `render_chat_dispatches`, `render_terminal_ansi` |
| Wry IPC closure | `gui.rs::run_gui` | `with_ipc_handler` (delegates to handle_ipc first, falls through for wry-only) |
| WS handler | `server.rs::handle_socket` | Per-connection task with subscribe + writer + handle_ipc |
| Frontend transport | `frontend/src/hooks/useIPC.ts` | `window.ipc` vs WebSocket detection |
| Feature gate | `Cargo.toml` | `[features] gui = [...]`; gates `tao` + `wry` + `comrak` + `rfd` + `native-dialog` |
| Module gating | `lib.rs` | `#[cfg(feature = "gui")] pub mod {gui, shared_session, ipc, server, event_render, file_preview, shell_dispatch}` |
| Makefile targets | `/Makefile` | `build`, `build-cli`, `build-app`, `serve`, `dev-frontend`, `test`, `sync-public` |
