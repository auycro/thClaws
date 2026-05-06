# App architecture

How the binary is structured: three surfaces (desktop GUI, CLI REPL, print mode) backed by one engine (Agent + Session + ToolRegistry), the OS-thread / worker-thread split, the wry+tao webview wrapper, the Rust↔JS bridge, the filesystem sandbox, and the moving parts that wire it all together.

This doc covers: binary layout (entry points, clap, build feature), the three runtime modes, the shared-engine pattern, the cargo + frontend build chain, the process model (main thread vs worker thread vs forwarder threads), worker state + channel topology, the wry+tao webview, the Rust↔JS IPC bridge (both directions), the embedded React dist, the `thclaws://` custom protocol + file-asset route, the filesystem sandbox + write-policy carve-outs, settings layering, build metadata injection, cancellation, and dev escape hatches.

**Source modules:**
- `crates/core/src/bin/app.rs` — `thclaws` unified binary entry point (clap → GUI / CLI / print dispatch)
- `crates/core/src/bin/cli.rs` — `thclaws-cli` CLI-only binary (no `gui` feature dependency)
- `crates/core/src/gui.rs` — wry+tao webview, event loop, `UserEvent`, IPC handler, custom protocol, event translator (3349 LOC)
- `crates/core/src/shared_session.rs` — `WorkerState`, `ShellInput`/`ViewEvent`, `spawn_with_approver`, `run_worker`, `handle_line`, `drive_turn_stream` (2549 LOC)
- `crates/core/src/agent.rs` — `Agent::run_turn` streaming state machine (2559 LOC)
- `crates/core/src/repl.rs` — CLI REPL (`run_repl`, `run_print_mode`), slash-command parser
- `crates/core/src/session.rs` — `Session`, `SessionStore` JSONL persistence
- `crates/core/src/sandbox.rs` — `Sandbox::init`, `check`, `check_write`, write-policy
- `crates/core/src/config.rs` — `AppConfig::load`, settings-layering precedence
- `crates/core/src/permissions.rs` — `ApprovalSink`, `GuiApprover`, `AutoApprover`, `PermissionMode`
- `crates/core/src/cancel.rs` — `CancelToken`
- `crates/core/build.rs` — git SHA / branch / dirty / build-time injection
- `crates/core/src/version.rs` — read those rustc-env vars
- `frontend/src/hooks/useIPC.ts` — JS-side bridge (`window.ipc.postMessage` + `window.__thclaws_dispatch`)
- `frontend/vite.config.ts` — `vite-plugin-singlefile` config (single-HTML output)

**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — `Agent::run_turn` per-turn pipeline
- [`built-in-tools.md`](built-in-tools.md) — `ToolRegistry` + the `Tool` trait
- [`permissions.md`](permissions.md) — sandbox + approval-sink details
- [`sessions.md`](sessions.md) — JSONL persistence + cross-process behavior
- [`context-composer.md`](context-composer.md) — system-prompt construction
- [`mcp.md`](mcp.md) — MCP-Apps widget IPC (sits on top of the bridge described here)

---

## 1. Three surfaces, one engine

```
┌──────────────────── thclaws-core (single crate) ────────────────────┐
│                                                                      │
│   ┌─────────────────────── Engine ──────────────────────────┐        │
│   │  Agent ─── ToolRegistry ─── Session ─── ProviderKind    │        │
│   │     ↑          ↑              ↑              ↑          │        │
│   │     └──────────┴──────────────┴──────────────┘          │        │
│   │              owned by WorkerState                       │        │
│   └─────────────────────────────────────────────────────────┘        │
│                              ↑                                       │
│        ┌─────────────────────┼─────────────────────┐                 │
│        │                     │                     │                 │
│   ┌────────────┐      ┌──────────────┐      ┌──────────────┐         │
│   │  GUI mode  │      │   CLI REPL   │      │  Print mode  │         │
│   │  gui.rs    │      │   repl.rs    │      │  repl.rs     │         │
│   │  +wry+tao  │      │   +rustyline │      │  (one-shot)  │         │
│   └────────────┘      └──────────────┘      └──────────────┘         │
│                                                                      │
└──────────────────────────────────────────────────────────────────────┘

       binaries
       ────────
       thclaws        ← unified (GUI default, --cli, --print)
       thclaws-cli    ← CLI-only build (no GUI deps)
```

The same `Agent` + `Session` + `ToolRegistry` backs all three surfaces. They differ only in:
- **How input arrives** (webview IPC vs `rustyline` stdin vs argv positional)
- **How output renders** (chat bubbles + terminal ANSI vs raw stdout)
- **What lifecycle** (long-running event loop vs interactive readline loop vs single turn)

This is a deliberate constraint: bug fixes to the agent loop, tool dispatch, session format, etc. land once and benefit every surface.

### Binaries

[`crates/core/Cargo.toml`](../thclaws/crates/core/Cargo.toml) declares two binaries:

| Binary | Source | Purpose |
|---|---|---|
| `thclaws` | [`bin/app.rs`](thclaws/crates/core/src/bin/app.rs) | Unified entry point. GUI by default; `--cli` or `--print` flips to terminal mode. Built with `--features gui` for desktop use. |
| `thclaws-cli` | [`bin/cli.rs`](thclaws/crates/core/src/bin/cli.rs) | CLI-only build. No `gui` feature. Smaller binary; ships on systems where the wry/tao runtime can't be installed (headless CI, minimal containers). |

The `gui` feature gate ([`Cargo.toml::[features]`](../thclaws/crates/core/Cargo.toml)) makes `gui.rs` and its dependencies (`wry`, `tao`, `webview`) optional. `thclaws --cli` works in either build; `thclaws` with no flag in a non-`gui` build prints "GUI not available — rebuild with `cargo build --features gui --bin thclaws`" and exits 1.

### Three modes

`bin/app.rs` is mostly clap parsing + dispatch:

```rust
#[tokio::main]
async fn main() {
    secrets::load_into_env();      // OS keychain → env vars
    endpoints::load_into_env();    // Custom endpoint config
    load_dotenv();                 // .env fallback for keys
    let _ = Sandbox::init();       // Pin sandbox root to cwd

    if let Err(e) = thclaws_core::policy::load_or_refuse() { ... }  // EE org policy

    let cli = Cli::parse();
    let use_cli = cli.cli || cli.print;

    if !use_cli {
        #[cfg(feature = "gui")]
        thclaws_core::gui::run_gui();   // → wry+tao loop, blocks until window close
        return;
    }

    let mut config = AppConfig::load()?;
    // ... apply CLI overrides to config ...

    if cli.print {
        run_print_mode(config, &prompt).await   // single turn, exit
    } else {
        run_repl(config).await                  // interactive readline loop
    }
}
```

Three exit paths:
- **GUI**: `gui::run_gui()` blocks the main thread on the tao event loop until the window closes.
- **REPL**: `repl::run_repl(config)` blocks on the rustyline loop until `/quit` or EOF.
- **Print**: `repl::run_print_mode(config, prompt)` runs one `Agent::run_turn`, prints the result, exits.

---

## 2. Build chain (frontend before Rust)

There is **no workspace `Cargo.toml`** — the repo is a single crate at [`crates/core/`](../thclaws/crates/core/). All `cargo` commands run from there (or with `--manifest-path crates/core/Cargo.toml`).

The frontend lives at [`frontend/`](../thclaws/frontend/) (React 19 + Vite 8 + Tailwind 4) and **MUST be built before the Rust crate** when using the `gui` feature. Rust embeds the compiled webview via:

```rust
// gui.rs:61
const FRONTEND_HTML: &str = include_str!("../../../frontend/dist/index.html");
```

If `frontend/dist/index.html` is missing, `cargo build --features gui` fails with a confusing include-error rather than a missing-file message. The standard build sequence is:

```sh
cd frontend && pnpm install && pnpm build && cd ..
cd crates/core && cargo build --features gui
```

### Single-file frontend

[`vite-plugin-singlefile`](https://github.com/richardtallent/vite-plugin-singlefile) inlines all JS and CSS into a single `dist/index.html`. The result is:

- One file the Rust binary embeds with `include_str!`
- No relative URL loading, no asset routing for the frontend itself
- Hot-reload during development requires `cd frontend && pnpm dev` standalone (no backend IPC, just UI iteration)

The custom `thclaws://` protocol (§5) handles secondary resource loading (file previews) without breaking the single-file model.

### Build-metadata injection

[`build.rs`](../thclaws/crates/core/build.rs) runs before each Rust build and injects rustc-env vars:

| Var | Source |
|---|---|
| `THCLAWS_GIT_SHA` | `git rev-parse HEAD` |
| `THCLAWS_GIT_BRANCH` | `git rev-parse --abbrev-ref HEAD` |
| `THCLAWS_GIT_DIRTY` | `git status --porcelain` non-empty? |
| `THCLAWS_BUILD_TIME` | UTC timestamp |
| `THCLAWS_BUILD_PROFILE` | `debug` / `release` |

Read by [`version.rs`](../thclaws/crates/core/src/version.rs); surfaced via `thclaws --version`. `git` missing at build time is tolerated — values become `"unknown"`.

---

## 3. Process model

```
                    ┌──────────────────────────┐
                    │  Main thread (tao)       │
                    │  ────────────────        │
                    │  - WindowBuilder         │
                    │  - WebViewBuilder        │
                    │  - EventLoop::run        │
                    │  - UserEvent::Dispatch → │
                    │    webview.evaluate_     │
                    │    script(__thclaws_     │
                    │    dispatch(...))        │
                    │                          │
                    │  - IPC handler:          │
                    │    JSON → ShellInput →   │
                    │    input_tx.send         │
                    └──────────┬───────────────┘
                               │
                       channels (mpsc / broadcast)
                               │
                    ┌──────────┴───────────────┐
                    │  Worker thread (tokio)   │
                    │  ────────────────────    │
                    │  run_worker:             │
                    │    loop {                │
                    │      ev = input_rx.recv  │
                    │      handle_*(ev)        │
                    │        → Agent::run_turn │
                    │        → events_tx.send  │
                    │          (broadcast)     │
                    │    }                     │
                    └──────────┬───────────────┘
                               │
                       broadcast channel
                               │
                    ┌──────────┴───────────────┐
                    │  Translator thread       │
                    │  ───────────────────     │
                    │  rx.recv() {             │
                    │    ev → render_chat_     │
                    │           dispatches     │
                    │       → render_terminal_ │
                    │           ansi           │
                    │    proxy.send_event(     │
                    │      UserEvent::Dispatch)│
                    │  }                       │
                    └──────────────────────────┘

                    Plus 2-3 forwarder threads:
                    - approval forwarder (GuiApprover.approve → frontend)
                    - ask-user forwarder (AskUserQuestion → frontend)
                    - team poller (lead inbox, GUI parity)
```

### Main thread (OS event loop)

In GUI mode, the **main OS thread** runs the tao event loop. This is mandatory — wry/tao require the main thread for window event dispatch on macOS (Cocoa restriction) and Windows (UI thread affinity).

Setup at [`gui.rs::run_gui`](../thclaws/crates/core/src/gui.rs):
1. `EventLoopBuilder::<UserEvent>::with_user_event().build()` — typed event loop with custom `UserEvent` payloads (Dispatch / SessionLoaded / FileTree / etc.)
2. `WindowBuilder::new().with_inner_size(...).build()` — tao window
3. `WebViewBuilder::new().with_url("thclaws://localhost/").with_custom_protocol(...).with_ipc_handler(...).build()` — wry webview attached to the window
4. `event_loop.run(move |event, _, control_flow| match event { ... })` — blocks until window close

The `UserEvent::Dispatch(json)` arm fires `webview.evaluate_script(format!("window.__thclaws_dispatch('{json}')"))` to push messages to the JS side.

### Worker thread (tokio agent runtime)

Spawned by [`shared_session::spawn_with_approver`](../thclaws/crates/core/src/shared_session.rs). Lives for the entire window lifetime. Owns the `WorkerState` (Agent, Session, ToolRegistry, AppConfig, MCP clients, skill store, cancel token, lead-log file handle).

```rust
pub fn spawn_with_approver(approver: Arc<dyn ApprovalSink>) -> SharedSessionHandle {
    let (input_tx, input_rx) = mpsc::channel::<ShellInput>();
    let (events_tx, _) = broadcast::channel::<ViewEvent>(256);
    let cancel = CancelToken::new();
    let ready_gate = Arc::new(ReadyGate::new());

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run_worker(input_rx, ..., events_tx, cancel, approver, gate));
    });

    SharedSessionHandle { input_tx, events_tx, cancel, ready_gate }
}
```

`run_worker` builds `WorkerState`, then enters a `while let Some(ev) = input_rx.recv().await` loop dispatching to `handle_line` / `handle_line_with_images` / `LoadSession` handler / `NewSession` handler / etc. Each `handle_line` call may invoke `Agent::run_turn` and stream `ViewEvent`s out to `events_tx`.

The worker uses `std::panic::catch_unwind` so a panic mid-turn surfaces as a `ViewEvent::ErrorText("internal error: ...")` instead of killing the worker thread (which would silently freeze the GUI).

### Translator thread

[`gui::spawn_event_translator`](../thclaws/crates/core/src/gui.rs) subscribes to the worker's broadcast channel and fans each `ViewEvent` into:

1. **Chat-shaped IPC envelopes** via `render_chat_dispatches` — `chat_text_delta`, `chat_tool_call`, `chat_tool_result`, `chat_history_replaced`, `chat_plan_update`, etc.
2. **Terminal-shaped IPC envelopes** via `render_terminal_ansi` — `terminal_data` carrying base64-encoded ANSI bytes.

Both Terminal and Chat tabs subscribe to their respective IPC envelopes and render the same conversation from different angles. This is what makes the two tabs **share one session** — they're two views over the same `ViewEvent` stream.

### Forwarder threads

Three additional dedicated tokio threads in GUI mode:

- **Approval forwarder** ([`gui.rs:1396`](../thclaws/crates/core/src/gui.rs)) — pumps `GuiApprover.approve()` requests to `approval_request` IPC envelopes. Periodic redispatch every 1 s for unresolved requests handles the race where the initial dispatch fires before React mounts.
- **Ask-user forwarder** ([`gui.rs:1355`](../thclaws/crates/core/src/gui.rs)) — pumps `AskUserQuestion` tool requests to the chat composer.
- **Team poller** — sweeps the lead's mailbox every N seconds for teammate notifications; emits `ShellInput::TeamMessages` so the lead reacts in GUI mode (the CLI REPL has its own poller loop).

---

## 4. Worker state + channel topology

```rust
// shared_session.rs:71
pub enum ShellInput {
    Line(String),
    LineWithImages { text: String, images: Vec<(String, String)> },
    NewSession,
    LoadSession(String),
    SaveAndQuit,
    ChangeCwd(PathBuf),
    TeamMessages(Vec<TeamMessage>),
    McpReady { server_name, client, tools },
    McpFailed { server_name, error },
    ReloadConfig,
    McpAppCallTool { request_id, qualified_name, arguments },
    SessionDeletedExternal { id },
    SessionRenamedExternal { id, title },
}

// shared_session.rs:156
pub enum ViewEvent {
    UserPrompt(String),
    AssistantTextDelta(String),
    ToolCallStart { name, label, input },
    ToolCallResult { name, output, ui_resource },
    SlashOutput(String),
    TurnDone,
    HistoryReplaced(Vec<DisplayMessage>),
    SessionListRefresh(String),
    ProviderUpdate(String),
    KmsUpdate(String),
    McpUpdate(String),
    ModelPickerOpen(String),
    ContextWarning { file_size_mb },
    ErrorText(String),
    McpAppCallToolResult { request_id, content, is_error },
    QuitRequested,
    PlanUpdate(Option<Plan>),
    PermissionModeChanged(PermissionMode),
    PlanStalled { step_id, step_title, turns },
}

// shared_session.rs:350
pub struct WorkerState {
    pub agent: Agent,
    pub config: AppConfig,
    pub session: Session,
    pub session_store: Option<SessionStore>,
    pub tool_registry: ToolRegistry,
    pub system_prompt: String,
    pub cwd: PathBuf,
    pub approver: Arc<dyn ApprovalSink>,
    pub skill_store: Arc<Mutex<SkillStore>>,
    pub mcp_clients: Vec<Arc<McpClient>>,
    pub warned_file_size: bool,
    pub lead_log: Arc<Mutex<Option<File>>>,
    pub cancel: CancelToken,
}
```

`WorkerState::rebuild_agent(preserve_history)` rebuilds the `Agent` with a fresh provider — used by `/model`, `/provider`, `/permissions`, `/mcp add`, `/kms use`, etc. `WorkerState::rebuild_system_prompt()` recomposes the system prompt from current config + memory + KMS + skills strategy + team grounding.

The channel split (mpsc input + broadcast events) gives:
- **Single producer per input source, single consumer (worker)** — `input_tx.send(ShellInput::Line(...))` is the only way to drive the worker
- **Single producer (worker), many consumers (translator + tests + future tabs)** — `events_tx.send(ViewEvent::...)` fans out via `broadcast::channel(256)`

A consumer that lags falls behind — the broadcast drops events with `Lagged(N)` and the consumer continues from current. The translator handles this gracefully but doesn't currently re-emit a `HistoryReplaced` to resync (documented gap).

---

## 5. wry+tao webview

### Window + protocol

```rust
// gui.rs:1306-1455 (abridged)
pub fn run_gui() {
    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let window = WindowBuilder::new()
        .with_title(&branding::current().name)
        .with_inner_size(LogicalSize::new(win_w, win_h))
        .build(&event_loop)?;

    #[cfg(windows)] let start_url = "http://thclaws.localhost/";
    #[cfg(not(windows))] let start_url = "thclaws://localhost/";

    let webview = WebViewBuilder::new()
        .with_url(start_url)
        .with_custom_protocol("thclaws".into(), |req| { ... })
        .with_devtools(env::var("THCLAWS_DEVTOOLS").is_ok())
        .with_ipc_handler(move |req| { ... })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| match event { ... });
}
```

The custom `thclaws://` protocol serves:
- **`/`** (root) — returns the embedded `FRONTEND_HTML` constant (single-HTML React app)
- **`/file-asset/<path>`** — serves on-disk files for previewed HTML pages, with full sandbox validation via `Sandbox::check`. Lets a previewed `index.html` load its sibling `.css` / `.js` / images via relative URLs without breaking the single-file embedding model.

File-asset MIME types are detected by extension (html/css/js/json/svg/png/jpg/gif/webp/ico/woff/woff2/ttf/otf, default `application/octet-stream`).

403 returned for paths the sandbox rejects. 404 returned for missing files. Both bodies are tiny — the frontend handles the surface error.

### Why a webview, not a native UI

- **Cross-platform with one codebase** — wry wraps WKWebView (macOS), WebView2 (Windows), and WebKitGTK (Linux)
- **Reuse the React/Tailwind ecosystem** — components, libraries, stylesheets all run unchanged
- **Hot-reload during dev** — `pnpm dev` standalone iterates UI without rebuilding the Rust crate
- **xterm.js for the terminal tab** — full ANSI rendering without writing a TUI from scratch

Costs: ~50 MB RSS for the webview process tree, dependency on the OS-bundled webview (WebKitGTK on Linux requires `libwebkit2gtk-4.1-0`).

---

## 6. The Rust↔JS bridge

Two channels, each one-way:

```
      ┌─────────────────────────────────────────────────┐
      │   JS  →  Rust   (synchronous, fire-and-forget)  │
      │                                                 │
      │   window.ipc.postMessage(JSON.stringify(msg))   │
      │              ↓                                  │
      │   wry .with_ipc_handler(|req| { ... })          │
      │              ↓                                  │
      │   match msg.type {                              │
      │     "chat_input"     => input_tx.send(Line)     │
      │     "approval_resp"  => approver.resolve(...)   │
      │     "session_load"   => input_tx.send(Load...)  │
      │     ...                                         │
      │   }                                             │
      └─────────────────────────────────────────────────┘

      ┌─────────────────────────────────────────────────┐
      │   Rust  →  JS   (broadcast via evaluate_script) │
      │                                                 │
      │   proxy.send_event(UserEvent::Dispatch(json))   │
      │              ↓                                  │
      │   event_loop arm:                               │
      │     webview.evaluate_script(                    │
      │       format!("window.__thclaws_dispatch('{}')",│
      │               escaped_json))                    │
      │              ↓                                  │
      │   window.__thclaws_dispatch = (json) => {       │
      │     const msg = JSON.parse(json);               │
      │     handlers.forEach(h => h(msg));              │
      │   }                                             │
      │              ↓                                  │
      │   React components subscribe via useIPC.ts:     │
      │     subscribe((msg) => { ... });                │
      └─────────────────────────────────────────────────┘
```

### JS → Rust

[`useIPC.ts`](../thclaws/frontend/src/hooks/useIPC.ts):

```typescript
export function send(msg: IPCMessage) {
  if (window.ipc) {
    window.ipc.postMessage(JSON.stringify(msg));
  } else {
    console.warn("[ipc] no backend — running in browser dev mode?", msg);
  }
}
```

`window.ipc` is wry's injected bridge. `postMessage` is synchronous on the JS side and fire-and-forget — the IPC handler on the Rust side runs in the wry event loop's worker pool.

The Rust handler in [`gui.rs:1519-1900`](../thclaws/crates/core/src/gui.rs) is a giant `match msg.type` over message types. Selected examples:

| `type` | Rust action |
|---|---|
| `"chat_input"` | `input_tx.send(ShellInput::Line(text))` |
| `"chat_input_with_images"` | `input_tx.send(ShellInput::LineWithImages { text, images })` |
| `"cancel_turn"` | `cancel.cancel()` |
| `"approval_response"` | `approver.resolve(id, decision)` |
| `"ask_user_response"` | `pending_asks.lock().remove(&id).map(\|tx\| tx.send(answer))` |
| `"session_load"` | `input_tx.send(ShellInput::LoadSession(id))` |
| `"session_delete"` | Delete from disk + `input_tx.send(SessionDeletedExternal)` if it's the active one |
| `"file_tree"` | `proxy.send_event(UserEvent::FileTree(payload))` |
| `"file_open"` | Read file (sandbox-validated) → `proxy.send_event(UserEvent::FileContent(payload))` |
| `"app_close"` | `proxy.send_event(UserEvent::QuitRequested)` |
| `"mcp_call_tool"` | `input_tx.send(ShellInput::McpAppCallTool { request_id, qualified_name, arguments })` |
| `"zoom_set"` | `proxy.send_event(UserEvent::ZoomChanged(factor))` |

### Rust → JS

`UserEvent::Dispatch(json_string)` is the catchall envelope. The event-loop arm formats it into JS and runs:

```rust
webview.evaluate_script(&format!(
    "window.__thclaws_dispatch('{}')",
    json_escape(json_string),
))
```

The escape function handles single-quote and backslash so the JS string literal stays well-formed.

JS-side dispatch in [`useIPC.ts`](../thclaws/frontend/src/hooks/useIPC.ts):

```typescript
window.__thclaws_dispatch = (json: string) => {
  try {
    const msg: IPCMessage = JSON.parse(json);
    handlers.forEach((h) => h(msg));
  } catch (e) {
    console.error("[ipc] dispatch parse error:", e);
  }
};
```

React components register handlers via `subscribe(handler)` (returns an unsubscribe fn). The `useIPC` hook wraps `useEffect`:

```typescript
useEffect(() => {
  const unsub = subscribe((msg) => {
    if (msg.type === "chat_text_delta") setText((t) => t + msg.delta);
  });
  return unsub;
}, []);
```

### Race protection

The initial `evaluate_script` can fire before the webview finishes its first React mount, at which point `window.__thclaws_dispatch` is undefined and the call silently drops. The approval forwarder (and a few other one-shot dispatchers) re-emit every 1 s for unresolved requests until the responder resolves.

For chat-stream events: the `SendInitialState` UserEvent kicks the worker to re-broadcast the current session state once React signals readiness via a `frontend_ready` IPC. This handles both initial mount and post-reload.

---

## 7. Frontend tabs

Three primary tabs share one conversation:

| Tab | Component | Subscribes to |
|---|---|---|
| Terminal | [`TerminalView.tsx`](../thclaws/frontend/src/components/TerminalView.tsx) | `terminal_data` (base64 ANSI bytes), `terminal_history_replaced` |
| Chat | [`ChatView.tsx`](../thclaws/frontend/src/components/ChatView.tsx) | `chat_text_delta`, `chat_tool_call`, `chat_tool_result`, `chat_history_replaced`, `chat_plan_update` |
| Files | [`FilesView.tsx`](../thclaws/frontend/src/components/FilesView.tsx) | `file_tree`, `file_content`; CodeMirror preview + TipTap markdown editing |

Plus secondary tabs (Team, Sessions, Settings) and overlay surfaces (sidebar, plan sidebar, approval modal, ask modal, model picker).

Terminal uses xterm.js (~30 KB gz). Chat uses standard React rendering with Markdown support (react-markdown). Files uses CodeMirror 6 (~40 KB gz) for preview across ~40 languages.

The single-conversation contract: any state divergence between Terminal and Chat is a bug. Both translate the same `ViewEvent` stream — Terminal goes through `render_terminal_ansi` (ANSI escape codes), Chat goes through `render_chat_dispatches` (typed envelopes). When editing tab logic, treat them as views of one model.

---

## 8. Filesystem sandbox

[`sandbox.rs`](../thclaws/crates/core/src/sandbox.rs) — file tools call `Sandbox::check(path)` (read) or `Sandbox::check_write(path)` (write) before every operation.

### Initialization

`Sandbox::init()` runs once at process start (before clap parse, before AppConfig load). It:

1. Picks the root from `$THCLAWS_PROJECT_ROOT` if set (worktree teammates inherit the parent project), else `current_dir()`.
2. Canonicalizes (resolves symlinks once at boot).
3. Stores in `static SANDBOX_ROOT: RwLock<Option<PathBuf>>`.

Re-init is supported — the GUI's "change directory" modal calls `Sandbox::init()` after `set_current_dir` so the new project's root becomes the new sandbox root.

### `check(path)` — read validation

Resolves `path` (relative paths joined to **cwd**, not root — important for worktree teammates whose cwd is `.worktrees/<name>/`). Lexically normalizes `..` / `.` (so `cwd/../outside` doesn't falsely pass). Canonicalizes if the path exists (catches symlink escapes). Returns the canonical absolute path or `Err("access denied: ... outside the project directory ...")`.

For paths that don't exist yet (Write to a new deep path like `src/api/handlers/auth.ts`): walks up to the longest existing ancestor, canonicalizes that, joins the non-existing tail. Since the tail can't itself contain symlinks (it doesn't exist), and we already lexically resolved `..`, joining is safe.

### `check_write(path)` — write validation

`check_write` calls `check`, then enforces the **`.thclaws/` write-policy**:

```rust
fn enforce_write_policy(root: &Path, resolved: PathBuf) -> Result<PathBuf> {
    let protected = root.join(".thclaws");
    if resolved == protected || resolved.starts_with(&protected) {
        return Err("access denied: .thclaws/ is reserved for team state");
    }
    Ok(resolved)
}
```

`.thclaws/` holds team state (settings.json, agent defs, mailboxes, sessions/, kms/, memory/, todos.md). Generic `Write` / `Edit` / `Bash mv` etc. all reject paths inside it.

### Carve-outs (intentional sandbox bypass)

Three tools deliberately skip `Sandbox::check_write` to land inside `.thclaws/`:

| Tool | Target | Rationale |
|---|---|---|
| `TodoWrite` | `.thclaws/todos.md` | Model-managed task list scratchpad |
| `KmsWrite` / `KmsAppend` | `.thclaws/kms/<name>/pages/...` | M6.25 — LLM-maintained wiki pages |
| `MemoryWrite` / `MemoryAppend` | `<root>/.thclaws/memory/...` and `~/.local/share/thclaws/memory/...` | M6.26 — LLM-maintainable long-lived memory |

Each has its own finer-grained validation (`writable_page_path`, `writable_entry_path`) that enforces the same security properties (no `..`, no path separators, no symlink escape) but allows the specific carve-out path. See [`kms.md`](kms.md) §7 and [`memory.md`](memory.md) §6 for the security model.

### Cwd-relative resolution (worktree teammates)

A worktree teammate's cwd is `.worktrees/<name>/` but its sandbox root is the **workspace** (set via `THCLAWS_PROJECT_ROOT` at spawn). Relative writes resolve from cwd:

```rust
let initial = if Path::new(path).is_absolute() {
    PathBuf::from(path)
} else {
    cwd.join(path)   // ← cwd, not root
};
```

So `Write("src/server.ts")` from a worktree teammate lands in the worktree (where it stays on `team/<name>` branch), not in the workspace's `src/` (which would put the file on `main`). `..`-escapes from the worktree into shared workspace artifacts (`docs/api-spec.md`) are still allowed because the canonical form is inside the sandbox.

---

## 9. Approval flow

`Agent::run_turn` calls `approver.approve(req).await` before each tool dispatch (in Ask mode; bypassed in Auto mode). The approver is a trait:

```rust
#[async_trait]
pub trait ApprovalSink: Send + Sync {
    async fn approve(&self, req: &ApprovalRequest) -> ApprovalDecision;
}
```

Two implementations:

| Type | Used by | Behavior |
|---|---|---|
| `AutoApprover` | `--accept-all`, `permission_mode = "auto"` | Returns `Allow` for everything |
| `GuiApprover` | GUI mode | Sends request to a forwarder thread → `approval_request` IPC → frontend modal → `approval_response` IPC → `oneshot::Sender::send(decision)` → resolves the await |
| (REPL prompt) | CLI REPL | Reads `[y/N]` from stdin via `tokio::task::spawn_blocking` |

`GuiApprover` keeps two HashMap structures:
- `pending` — `id → oneshot::Sender<ApprovalDecision>` for awaiting requests
- `unresolved` — `id → ApprovalRequest` for redispatch (race protection)

The IPC handler resolves the pending sender on `approval_response`; the periodic redispatcher walks unresolved every 1 s and re-emits `approval_request` until resolved. Same pattern protects `AskUserQuestion`.

See [`permissions.md`](permissions.md) for the full approval matrix and `requires_approval` rules per tool.

---

## 10. Settings layering

[`config.rs`](../thclaws/crates/core/src/config.rs) layers settings in this precedence (higher wins):

```
1. CLI flags (--model, --permission-mode, --max-iterations, ...)
2. <cwd>/.thclaws/settings.json (project)
3. ~/.config/thclaws/settings.json (user)
4. ~/.claude/settings.json (Claude Code fallback)
5. Compiled-in defaults
```

API keys are **never** stored in `settings.json`. They live in:
- OS keychain ([`secrets.rs`](../thclaws/crates/core/src/secrets.rs)) — primary
- `.env` file ([`dotenv.rs`](../thclaws/crates/core/src/dotenv.rs)) — fallback for headless / sandboxed envs without keychain

`secrets::load_into_env()` runs at process start (before `AppConfig::load`) so `std::env::var("ANTHROPIC_API_KEY")` works uniformly throughout the codebase.

The CLI fallback path (`~/.claude/settings.json`) is intentional — configs stay portable between Claude Code and thClaws.

---

## 11. Cancellation

[`cancel.rs`](../thclaws/crates/core/src/cancel.rs) implements a cooperative `CancelToken`:

```rust
#[derive(Clone)]
pub struct CancelToken { ... }

impl CancelToken {
    pub fn new() -> Self;
    pub fn cancel(&self);            // signal cancellation
    pub fn is_cancelled(&self) -> bool;
    pub async fn cancelled(&self);   // await until cancelled
}
```

Wired into:
- `Agent` — checked between iterations and inside `provider.stream()` retry sleeps
- `WorkerState` — kept around so `rebuild_agent` can re-wire onto the fresh Agent
- `SharedSessionHandle` — exposed so the GUI's `cancel_turn` IPC can fire `cancel.cancel()`

Cancellation is **cooperative** — long-running tools (`Bash` with a slow command, `WebFetch` with the M6.23 timeout) check periodically. Provider HTTP requests check at retry boundaries. The `/cancel` slash command and the GUI's "Stop" button both route to `cancel.cancel()`.

There's only one shared CancelToken per worker. `/new` and `/load` reset it implicitly via `WorkerState::rebuild_agent`.

---

## 12. Dev escape hatches

Environment variables that change runtime behavior without rebuilding:

| Var | Effect |
|---|---|
| `THCLAWS_DEVTOOLS=1` | Open WebView devtools when running the GUI. Use this to debug frontend issues instead of shipping `console.log`. |
| `THCLAWS_DISABLE_KEYCHAIN=1` | Skip OS keychain reads — for headless CI / sandboxed envs where the keychain prompts are blocked. Falls back to `.env`. |
| `THCLAWS_PROJECT_ROOT=<path>` | Pin the sandbox root to this path regardless of cwd. Set by SpawnTeammate so worktree agents treat the parent project as writable. |
| `THCLAWS_TEAM_AGENT=<name>` + `THCLAWS_TEAM_DIR=<path>` | Run as a team agent with the given name + team directory. Set by `--team-agent` flag. |
| `THCLAWS_DISABLE_TEAM_POLLER=1` | Skip the lead's inbox poller (debugging / tests). |

CLI flags that override config:

- `--cli` / `--print` — surface mode
- `--model <id>` — `config.model`
- `--accept-all` (alias `--dangerously-skip-permissions`) — `config.permissions = "auto"`
- `--permission-mode <auto|ask>` — `config.permissions`
- `--system-prompt <text>` — `config.system_prompt`
- `--allowed-tools <csv>` / `--disallowed-tools <csv>` — `config.allowed/disallowed_tools`
- `--max-iterations <n>` — `config.max_iterations`
- `--resume <id-or-last>` — `config.resume_session`
- `--team-agent <name>` + `--team-dir <path>` — sets the team env vars
- `--verbose` — prints token counts + timing
- `--output-format text|stream-json` — print mode output shape

---

## 13. CI constraints

[`.github/workflows/ci.yml`](../.github/workflows/ci.yml) runs tests on **ubuntu + macOS only**; **Windows is excluded** because several tests assume Unix path separators (the sandbox tests in particular). Windows coverage comes from the release workflow building the binary.

The CI `frontend` job uploads `frontend/dist/` as an artifact that the `clippy` and `test` jobs download — the Rust build depends on it existing.

If you add tests that touch paths, use `Path::new` / `PathBuf` and avoid hardcoded `/` or `\`. Tests that mutate process env (`HOME`, cwd) must serialize via the shared test-env-lock (see `kms::test_env_lock`).

---

## 14. Code-organization summary

```
crates/core/
├── Cargo.toml                 — single-crate manifest, [features] gui = [wry, tao, ...]
├── build.rs                   — git/build-time injection
└── src/
    ├── bin/
    │   ├── app.rs            — `thclaws` (unified, GUI/CLI/print)
    │   └── cli.rs            — `thclaws-cli` (CLI-only)
    ├── lib.rs                 — module roots
    ├── agent.rs               — Agent::run_turn (per-turn pipeline)
    ├── shared_session.rs      — WorkerState, ShellInput, ViewEvent, run_worker
    ├── gui.rs                 — wry+tao + IPC handler + event translator
    ├── repl.rs                — CLI REPL + slash command parser
    ├── session.rs             — JSONL persistence
    ├── sandbox.rs             — filesystem sandbox
    ├── permissions.rs         — ApprovalSink, GuiApprover, PermissionMode
    ├── cancel.rs              — CancelToken
    ├── config.rs              — settings layering
    ├── secrets.rs / dotenv.rs — API key resolution
    ├── providers/             — provider abstraction (anthropic, openai, ...)
    ├── tools/                 — Tool trait + 30+ implementations
    ├── kms.rs / tools/kms.rs  — KMS subsystem
    ├── memory.rs / tools/memory.rs — Memory subsystem
    ├── plugins.rs             — plugin lifecycle
    ├── mcp.rs                 — MCP client subsystem
    ├── skills.rs              — skill subsystem
    ├── compaction.rs          — history compaction
    ├── default_prompts/       — embedded markdown prompts
    └── ... (~50 more modules)

frontend/
├── package.json              — pnpm + vite + react + tailwind
├── vite.config.ts            — singlefile plugin config
├── index.html                — entry HTML (template)
└── src/
    ├── App.tsx               — root tab router
    ├── hooks/useIPC.ts       — bridge wrapper (send + subscribe)
    ├── components/
    │   ├── TerminalView.tsx
    │   ├── ChatView.tsx
    │   ├── FilesView.tsx
    │   ├── PlanSidebar.tsx
    │   ├── ApprovalModal.tsx
    │   └── ...
    └── styles/
```

---

## 15. Migration / known limitations

### Known limitations

- **Single window, single project** per process. A second project requires a new `thclaws` invocation. Multi-window support would require multiple `WorkerState` instances or a routing layer in the worker.
- **Translator can drop events under heavy load** (`broadcast::error::Lagged`). Currently no resync; the next `HistoryReplaced` re-establishes state. Practical impact rare since 256-slot buffer is generous.
- **GUI hot-reload doesn't work for Rust changes** — every Rust edit requires `cargo build` + restart. Frontend hot-reload via `pnpm dev` standalone (no backend IPC).
- **No headless GUI test harness** — GUI changes require manual verification in the wry window. The agent loop, tool dispatch, session format, etc. all have unit tests, but the IPC bridge end-to-end is untested.
- **Worker thread can wedge under panic in shell-dispatch arms** — protected by `catch_unwind` in `spawn_with_approver`, but a panic mid-stream may leave `events_tx` orphan-broadcasting before the error message lands.

### Sprint chronology (selected)

The architecture itself has been stable since Phase 1; subsequent sprints layered new subsystems onto it without changing the foundational shape.

- **Phase 1** — three-surface design (CLI + GUI + print), shared `Agent`/`Session`
- **Phase 4** — multimodal (image paste/drag-drop in chat composer)
- **Phase 6** — MCP (subprocess + Streamable HTTP)
- **M6.8** — bash non-interactive env, server-command detection
- **M6.11** — marketplace + plugins
- **M6.14** — session-store cwd resolution (per-project sessions)
- **M6.15-M6.16** — MCP-Apps widgets (host↔widget postMessage protocol on top of the bridge)
- **M6.17-M6.19** — agent loop hygiene + session JSONL hardening
- **M6.20** — permissions audit (subagent propagation, plan-mode message order)
- **M6.21-M6.22** — providers audit + prompt-cache visibility
- **M6.23-M6.24** — tools audit + medium-issue cleanup (sessions perf, cross-process locking)
- **M6.25-M6.27** — KMS + Memory subsystems converted to LLM-maintainable
