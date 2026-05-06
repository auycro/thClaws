# Hooks

User-defined shell commands that fire on agent lifecycle events. Configured in `.thclaws/settings.json` (project) or `~/.config/thclaws/settings.json` (user) under the `"hooks"` key. Eight events: `pre_tool_use`, `post_tool_use`, `post_tool_use_failure`, `permission_denied`, `session_start`, `session_end`, `pre_compact`, `post_compact`. Hooks are **fire-and-forget** — the agent loop doesn't wait for the hook child to finish, but a tokio reaper task `wait()`s in the background so processes don't leak as zombies. Each hook child runs with stdin/stdout/stderr redirected to `/dev/null` (no terminal corruption), bounded by a configurable per-hook timeout (default 5s, SIGKILL on expiry), with the parent's full environment plus event-specific `THCLAWS_*` vars.

This subsystem was completely orphaned dead code from initial commit through M6.34 — the module compiled, the user-manual chapter advertised it, but no production code path called `fire_*`. Users configured hooks, got silent no-ops with no error or warning. M6.35 wired it all up + added zombie reaping, byte-cap truncation, snake-case event names, GUI chat visibility for hook errors, and 7 new tests. See `dev-log/150-hooks-m6-35-wired-up.md`.

This doc covers: the event taxonomy + per-event env vars, the `HooksConfig` schema, the `fire()` lifecycle (spawn → reap → timeout), per-event call sites in agent / shared_session / repl, the GUI error broadcaster, configuration precedence, security model (env inheritance trust), the M6.35 audit fixes (HOOK1–HOOK10), known gaps + deferred items, and the testing surface.

**Source modules:**
- `crates/core/src/hooks.rs` — `HooksConfig` schema, `HookEvent` enum + `name()` snake-case mapping, `fire(config, event, env)`, `fire_pre_tool_use` / `fire_post_tool_use` / `fire_permission_denied` / `fire_session` / `fire_compact` convenience helpers, `truncate_for_env` (UTF-8 safe byte cap with marker), `set_error_broadcaster` + `report_error`, `DEFAULT_HOOK_TIMEOUT_SECS = 5`, `MAX_HOOK_ENV_BYTES = 8192`
- `crates/core/src/config.rs` — `AppConfig.hooks: HooksConfig` field (parseable from settings.json)
- `crates/core/src/util.rs` — `shell_command_async()` returns `tokio::process::Command` with platform-default shell + `THCLAWS_SHELL` override
- `crates/core/src/agent.rs` — `Agent.hooks: Option<Arc<HooksConfig>>` field + `with_hooks()` builder, `run_turn` dispatch site (pre/post_tool_use, permission_denied), compaction site (pre_compact/post_compact gated on actual trim)
- `crates/core/src/subagent.rs` — `ProductionAgentFactory.hooks` field; recursive child factories propagate; built `Agent` calls `.with_hooks(h)`
- `crates/core/src/shared_session.rs::run_worker` — GUI-side `hooks_arc` snapshot, `set_error_broadcaster` registration → `ViewEvent::SlashOutput`, `fire_session(SessionStart)` after WorkerState built, `fire_session(SessionEnd)` after `input_rx` loop exits
- `crates/core/src/shared_session.rs::WorkerState::rebuild_agent` — re-snapshots `config.hooks` so live config edits take effect on the next agent
- `crates/core/src/repl.rs::run_repl_with_state` — CLI-side `hooks_arc` snapshot before factory, `fire_session(SessionStart)` before readline loop, `fire_session(SessionEnd)` at both EOF handlers; `with_hooks` chained at every `Agent::new` build site (initial + 3 in-place rebuilds)
- `user-manual/ch13-hooks.md` — user-facing chapter with config snippets + practical recipes

**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — `pre_tool_use` / `post_tool_use` fire from inside `Agent::run_turn` at the dispatch site
- [`permissions.md`](permissions.md) — `permission_denied` fires from the explicit `ApprovalDecision::Deny` path; BashTool / sandbox / plan-mode hard-blocks do NOT fire it (documented gap)
- [`subagent.md`](subagent.md) — subagent inherits parent's hooks via `ProductionAgentFactory.hooks` propagation; Task-spawned tool calls fire hooks too
- [`agent-team.md`](agent-team.md) — each teammate is a separate `thclaws --team-agent` subprocess that loads its own `AppConfig` (and thus its own `HooksConfig`), fires its own session_start/end + per-tool hooks
- [`sessions.md`](sessions.md) — session id (`THCLAWS_SESSION_ID`) is set at `session_start` and `session_end`

---

## 1. Concept

Hooks are the **observation surface** for the agent loop. Use them to:

- **Audit** — log every `Bash` command, every `Edit` / `Write`, every approval denial to a SIEM-bound file or HTTP endpoint
- **Notify** — desktop/Slack/email notification on session start, session end, or permission denial
- **Side-effect** — auto-commit after every successful Edit, ping CI on session end, snapshot disk state pre-compact

What hooks are NOT:

- **Mutators**: hooks are read-only observers. They cannot rewrite tool inputs or block tool execution. To block, use `permissions.deny` + tool filters (see [`permissions.md`](permissions.md)).
- **Synchronous gatekeepers**: `pre_tool_use` is fire-and-forget; the tool may run before the hook has even read its env. For audit hooks where strict ordering matters, the contract is best-effort.
- **A replacement for proper telemetry**: hooks log into the user's choice of sink. They don't ship metrics anywhere by themselves.

---

## 2. Event taxonomy

| Event | Fires when | Env vars (in addition to inherited parent env + `THCLAWS_HOOK_EVENT`) |
|---|---|---|
| `pre_tool_use` | After approval gate, before `tool.call_multimodal()` | `THCLAWS_TOOL_NAME`, `THCLAWS_TOOL_INPUT` (JSON, ≤8KB) |
| `post_tool_use` | After successful tool result + truncate-to-disk + empty-replacement | `THCLAWS_TOOL_NAME`, `THCLAWS_TOOL_OUTPUT` (≤8KB), `THCLAWS_TOOL_ERROR=false` |
| `post_tool_use_failure` | After tool result with `is_error=true` (alternative to `post_tool_use`) | `THCLAWS_TOOL_NAME`, `THCLAWS_TOOL_OUTPUT`, `THCLAWS_TOOL_ERROR=true` |
| `permission_denied` | Inside `ApprovalDecision::Deny` branch (explicit user "n") | `THCLAWS_TOOL_NAME` |
| `session_start` | After `WorkerState` (GUI) or session/agent (CLI) is built, before message loop | `THCLAWS_SESSION_ID`, `THCLAWS_MODEL` |
| `session_end` | When input channel closes (GUI: `input_rx` returns Err; CLI: readline EOF / `bye` print) | `THCLAWS_SESSION_ID`, `THCLAWS_MODEL` |
| `pre_compact` | Before `compact()` runs AND `pre_tokens > messages_budget` (i.e. compaction will actually trim) | `THCLAWS_COMPACT_MESSAGES`, `THCLAWS_COMPACT_TOKENS` |
| `post_compact` | After `compact()` returns (same gate as pre_compact) | `THCLAWS_COMPACT_MESSAGES`, `THCLAWS_COMPACT_TOKENS` (post-compact values) |

`THCLAWS_HOOK_EVENT` is always set to the snake_case event name (e.g. `pre_tool_use`). M6.35 HOOK8 — pre-fix `format!("{event:?}")` produced PascalCase (`"PreToolUse"`), inconsistent with config field names. Fixed via `HookEvent::name()`.

### Events that DON'T fire `permission_denied`

The doc gate is intentional. Only the explicit `ApprovalDecision::Deny` path fires `permission_denied`. The following denial-shaped events are **invisible** to the hook today:

- BashTool's `lead_forbidden_command` rejection (lead-only blocked commands)
- BashTool's `teammate_forbidden_command` rejection (cross-branch reset)
- Sandbox `check_write` / `check` failure
- Plan-mode `TodoWrite` block (M6.20 BUG M1)
- Plan-mode generic mutating-tool block
- Plan-mode approval-window gate (waiting on user Approve)
- Subagent recursion-limit refusal

Users wiring `permission_denied` for "log every action denied" should be aware of this gap — the hook only sees explicit user denials. Documented in dev-log/150 as deferred.

---

## 3. Configuration

```json
{
  "hooks": {
    "pre_tool_use":          "echo \"$THCLAWS_TOOL_NAME\" >> /tmp/thclaws.log",
    "post_tool_use":         "...",
    "post_tool_use_failure": "...",
    "permission_denied":     "...",
    "session_start":         "notify-send 'thClaws started'",
    "session_end":           "...",
    "pre_compact":           "...",
    "post_compact":          "...",
    "timeout_secs":          5
  }
}
```

All fields optional. `null` / missing / empty string → event not registered (`fire()` short-circuits via `HooksConfig::get` returning `None`).

`timeout_secs` is the per-hook ceiling enforced by the reaper. Default `DEFAULT_HOOK_TIMEOUT_SECS = 5`. Set higher for slow notifications (e.g. `osascript` at 10s) or HTTP webhook calls (e.g. `curl` at 15s). The same value applies to ALL events — no per-event override today (deferred).

### Precedence

`AppConfig::load` merges three sources (later wins): compiled defaults → `~/.config/thclaws/settings.json` (user) → `.thclaws/settings.json` (project).

For nested struct fields (HooksConfig), serde performs **field-by-field replacement on the whole struct** — i.e. project's `"hooks"` block fully overrides user's. Setting `"hooks": {}` in a project file disables ALL hooks even if the user has them configured. Field-level merge is NOT supported today (TEAM-IF-WIRED-14 deferred).

### Hook command shell

`fire()` uses `crate::util::shell_command_async()` which resolves to:
- `THCLAWS_SHELL` env var if set, parsed as `<shell> <flag>` (e.g. `THCLAWS_SHELL="bash -c"`)
- Else `cmd /C` on Windows
- Else `/bin/sh -c` on Unix

The hook command is passed as a single argument to the shell, so multi-line snippets, pipelines, here-docs, and `&&` chains all parse normally. Quote `$THCLAWS_TOOL_INPUT` in scripts (`"$VAR"` not `$VAR`) to prevent re-splitting on whitespace.

---

## 4. The `fire()` lifecycle

```rust
pub fn fire(config: &HooksConfig, event: HookEvent, env: &HashMap<String, String>) {
    let Some(cmd) = config.get(event) else { return };
    let mut command = crate::util::shell_command_async(cmd);
    command.env("THCLAWS_HOOK_EVENT", event.name());      // snake_case (HOOK8)
    for (k, v) in env { command.env(k, v); }
    command.stdin(Stdio::null());                          // HOOK6
    command.stdout(Stdio::null());
    command.stderr(Stdio::null());

    let timeout = config.timeout();                        // HOOK7
    let event_name = event.name();
    match command.spawn() {
        Ok(mut child) => {
            tokio::spawn(async move {                      // HOOK5 reaper
                match tokio::time::timeout(timeout, child.wait()).await {
                    Ok(Ok(status)) if !status.success() => report_error(...),
                    Ok(Err(e))                           => report_error(...),
                    Err(_) => {                            // HOOK7 timeout
                        let _ = child.kill().await;
                        report_error(...);
                    }
                    _ => {}
                }
            });
        }
        Err(e) => report_error(...),                       // HOOK10
    }
}
```

**Must be called from within a tokio runtime context** — the reaper uses `tokio::spawn`. Every production caller (Agent::run_turn, shared_session::run_worker, repl loop) is async. Synchronous tests use `#[tokio::test]`.

### Why fire-and-forget?

A blocking `pre_tool_use` would couple the agent loop's per-tool latency to the hook's runtime. Even a 100ms hook on every Read / Grep / Edit is unacceptable for a 50-iteration turn. Fire-and-forget keeps the agent loop's hot path uncontaminated.

The trade-off: `pre_tool_use` may fire AFTER the tool has already started running. For audit hooks, the contract is "the hook will run for every tool call eventually, but timing is not synchronized." For most use cases (logging, notifications, side effects) this is fine.

### Why a reaper task?

`std::process::Command::spawn()` returns a `Child` that, on Unix, **becomes a zombie when dropped without `wait()`**. The agent loop fires hooks at high frequency (potentially 2× per tool call × 50 tool calls per turn × N concurrent sessions) — without reaping, the process table fills with zombies and PIDs eventually exhaust. M6.35 HOOK5 spawns a tokio task per child to call `child.wait().await`, then drops the Child cleanly.

### Why null stdio?

The default inherited stdio mixes the hook's output into the parent terminal mid-stream — corrupts CLI rendering, GUI terminal pane. M6.35 HOOK6 redirects all three to `Stdio::null()`. Hook scripts that want to log should write to a file explicitly:

```json
{ "hooks": { "pre_tool_use": "echo \"$THCLAWS_TOOL_NAME\" >> ~/.thclaws/hook.log" } }
```

### Why a 5s timeout?

A hook that hangs (read from inherited stdin, slow `curl` against an unresponsive endpoint, `sleep 30`) keeps its child alive forever holding fds + a process table slot. M6.35 HOOK7 wraps the reaper's `wait()` in `tokio::time::timeout`; on expiry, `child.kill().await` SIGKILLs the child and `report_error` surfaces "X timed out after 5s — killed". Configurable via `HooksConfig.timeout_secs`.

---

## 5. Per-event call sites

### `pre_tool_use` — `agent.rs::run_turn`

```rust
// After approval gate, before tool.call_multimodal:
if let Some(h) = &hooks {
    let input_str = serde_json::to_string(input).unwrap_or_else(|_| "<unserializable>".into());
    crate::hooks::fire_pre_tool_use(h, &name, &input_str);
}
let tool_result = tool.call_multimodal(input.clone()).await;
```

`input` is JSON-serialized then truncated to `MAX_HOOK_ENV_BYTES` (8KB) at a UTF-8 char boundary by `truncate_for_env`. Truncation marker: `" … [truncated, originally <N> bytes]"`.

### `post_tool_use` / `post_tool_use_failure` — `agent.rs::run_turn`

```rust
// After computing (content, is_error), before pushing into history:
if let Some(h) = &hooks {
    let preview = match &content {
        ToolResultContent::Text(s) => s.clone(),
        ToolResultContent::Blocks(_) => "<multimodal>".to_string(),
    };
    crate::hooks::fire_post_tool_use(h, &name, &preview, is_error);
}
```

`is_error` selects `PostToolUseFailure` event variant. The preview the hook sees is the **truncate-to-disk** variant — same text the next provider call will see, so audit hooks log what the model actually consumed. Multimodal blocks (e.g. images returned by Read) collapse to literal `"<multimodal>"`.

### `permission_denied` — `agent.rs::run_turn` Deny branch

```rust
if matches!(decision, ApprovalDecision::Deny) {
    if let Some(h) = &hooks {
        crate::hooks::fire_permission_denied(h, &name);
    }
    let denied = format!("denied by user: {name}");
    // ... yield ToolCallDenied + continue
}
```

Only fires for explicit user denials — see §2 for the list of denial paths that DON'T fire this hook.

### `session_start` — both lead startup paths

CLI (`repl.rs::run_repl_with_state`) — fires after agent built + before readline loop:

```rust
crate::hooks::fire_session(
    &hooks_arc,
    crate::hooks::HookEvent::SessionStart,
    &session.id,
    &config.model,
);
```

GUI (`shared_session.rs::run_worker`) — fires after `WorkerState` is built:

```rust
crate::hooks::fire_session(
    &hooks_arc,
    crate::hooks::HookEvent::SessionStart,
    &state.session.id,
    &state.config.model,
);
```

### `session_end` — three sites

CLI readline EOF (inside the `select!` `Ok(Some(l))` arm):
```rust
_ => {
    crate::hooks::fire_session(&hooks_arc, HookEvent::SessionEnd, &session.id, &config.model);
    crate::team::kill_my_teammates();
    println!("{COLOR_DIM}bye{COLOR_RESET}");
    return Ok(());
}
```

CLI bottom-of-function (e.g. `/quit` slash returns):
```rust
crate::hooks::fire_session(&hooks_arc, HookEvent::SessionEnd, &session.id, &config.model);
crate::team::kill_my_teammates();
println!("{COLOR_DIM}bye{COLOR_RESET}");
```

GUI worker `input_rx` exit (channel closed by handle drop / GUI shutdown):
```rust
// After `while let Ok(input) = input_rx.recv() { ... }`:
crate::hooks::fire_session(
    &hooks_arc,
    crate::hooks::HookEvent::SessionEnd,
    &state.session.id,
    &state.config.model,
);
```

The GUI session_end runs as the worker is being torn down — best-effort. Long-lived hook commands (slow `osascript`, `curl`) may be killed by the runtime teardown before completing. Prefer fast-exec commands here.

### `pre_compact` / `post_compact` — `agent.rs::run_turn` compact site

```rust
let pre_tokens = compaction::estimate_messages_tokens(&h);
let pre_count = h.len();
let will_compact = pre_tokens > messages_budget;
if will_compact {
    if let Some(hk) = &hooks {
        fire_compact(hk, HookEvent::PreCompact, pre_count, pre_tokens);
    }
}
let compacted = compact(&h, messages_budget);
if will_compact {
    if let Some(hk) = &hooks {
        let post_tokens = compaction::estimate_messages_tokens(&compacted);
        fire_compact(hk, HookEvent::PostCompact, compacted.len(), post_tokens);
    }
}
```

**Gated on `pre_tokens > messages_budget`** so hooks fire only when compaction actually trims. `compact()` is called every turn to re-fit the window, but no-ops when within budget — without the gate, audit hooks would fire empty events on every turn.

Two env vars per fire: `THCLAWS_COMPACT_MESSAGES` (count) + `THCLAWS_COMPACT_TOKENS` (estimated). Pre/post values let scripts compute ratios.

### Subagent inheritance

`ProductionAgentFactory.hooks: Option<Arc<HooksConfig>>` propagates parent → child. Recursive `child_factory` clones `self.hooks.clone()`; built `Agent` calls `.with_hooks(h)` if Some. Subagent's pre/post tool hooks fire just like the top-level agent's.

Subagents do NOT fire their own session_start / session_end — those events are scoped to the lead's process. A teammate **subprocess** (separate `thclaws --team-agent`) DOES fire session_start/end via its own `run_repl_with_state` path because it's a fresh process with its own AppConfig load.

### Live config reload

`WorkerState::rebuild_agent` re-snapshots `config.hooks`:

```rust
let new_agent = Agent::new(...)
    .with_approver(self.approver.clone())
    .with_cancel(self.cancel.clone())
    .with_hooks(std::sync::Arc::new(self.config.hooks.clone()));
```

Triggered by Settings → save → `ShellInput::ReloadConfig`. Same in CLI's three rebuild sites (model swap, provider swap, MCP add) — each chains `.with_hooks(Arc::new(config.hooks.clone()))`. Live edits to `.thclaws/settings.json` thus take effect on the next turn without requiring a full restart.

---

## 6. Error broadcasting (HOOK10)

`hooks.rs` exposes:

```rust
pub fn set_error_broadcaster<F>(f: F)
where F: Fn(String) + Send + Sync + 'static
```

backed by a `OnceLock<Mutex<Option<ErrorBroadcaster>>>` slot. `fn report_error(msg)` always calls `eprintln!` (CLI-friendly) AND invokes the broadcaster if set:

```rust
fn report_error(msg: String) {
    eprintln!("\x1b[33m[hook] {msg}\x1b[0m");
    let g = broadcaster_slot().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(f) = g.as_ref() {
        f(msg);
    }
}
```

`shared_session::run_worker` registers a closure that forwards to the GUI chat surface:

```rust
let err_tx = events_tx.clone();
crate::hooks::set_error_broadcaster(move |msg| {
    let _ = err_tx.send(ViewEvent::SlashOutput(format!("⚠ {msg}")));
});
```

So:
- **CLI**: hook errors visible via stderr only (terminal output)
- **GUI**: hook errors visible in chat AS A SLASH-OUTPUT EVENT (yellow ⚠ prefix) AND via stderr

Three error categories surface through this path:
1. **spawn failure** — binary not found, permission denied, etc. → `"<event> spawn failed: <io error>"`
2. **non-zero exit** — hook ran but returned non-zero → `"<event> exited non-zero (code N)"` (or `"signal"` if killed)
3. **timeout** — hook didn't exit within `timeout_secs` → `"<event> timed out after Ns — killed"`

The broadcaster uses `unwrap_or_else(|p| p.into_inner())` for poison-tolerance — same M6.31 PM4 pattern used by `tools::plan_state::fire`.

---

## 7. Security model

Hooks run **as the user**, with the user's full inherited environment. Three implications:

### Secret inheritance is intentional

Every hook child inherits `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, OAuth tokens, MCP server credentials. A malicious hook can exfiltrate these via `curl` / `nc`. **This is by design** — hooks are user-configured trust. The user authored the hook; the user implicitly trusted it with whatever the parent process holds.

A future `clean_env: true` opt-in could call `Command::env_clear()` before adding `THCLAWS_*` vars (preserving only PATH/HOME). Deferred (HOOK-IF-WIRED-8).

### Hook commands are untrusted by the model

Hooks live in user-edited config files (`.thclaws/settings.json`, `~/.config/thclaws/settings.json`). The agent's tool dispatch can't write hook config (the sandbox carve-outs for `.thclaws/` cover `todos.md`, `kms/`, `memory/`, `team/` — NOT `settings.json`). So the model can't inject hooks via Write/Edit. The hook surface is exclusively user-controlled.

### Hook output is invisible to the model

`Stdio::null()` on stdout/stderr means the model never sees hook output, even when the hook produces text. The hook is a side-effect channel, not a data channel. If a hook should affect the conversation, it must do so via filesystem writes the model can later Read.

### Per-tool no filtering

`pre_tool_use` fires for EVERY tool. There's no declarative `for_tools: ["Bash"]` config. Users wanting per-tool logic write shell:

```json
{ "hooks": { "pre_tool_use": "[ \"$THCLAWS_TOOL_NAME\" = Bash ] && audit-log \"$THCLAWS_TOOL_INPUT\"" } }
```

Declarative filtering is HOOK-IF-WIRED-13 in the deferred list.

---

## 8. M6.35 audit fixes (recap)

Ten fixes shipped together to wire up the orphaned subsystem and address every latent bug that would have surfaced the moment the wiring landed (`dev-log/150-hooks-m6-35-wired-up.md`):

| Fix | Severity | What changed |
|---|---|---|
| TOP-LEVEL | HIGH | Wired all 8 events at agent dispatch / session lifecycle / approval-deny / compaction sites |
| HOOK1 | HIGH | `Agent.hooks` field + `with_hooks` builder; pre/post_tool_use fire at dispatch |
| HOOK2 | HIGH | `session_start` / `session_end` fire at lead boot + EOF (CLI 2 sites + GUI input_rx exit) |
| HOOK3 | HIGH | `permission_denied` fires on explicit Deny |
| HOOK4 | MED | `pre_compact` / `post_compact` fire only when `compact()` actually trims |
| HOOK5 | HIGH | tokio reaper task per child — no zombie leak |
| HOOK6 | HIGH | `Stdio::null()` on all three — no terminal corruption |
| HOOK7 | MED | `tokio::time::timeout` 5s default + SIGKILL — no fd hold |
| HOOK8 | MED | `HookEvent::name()` snake_case |
| HOOK9 | MED | Byte-cap (8KB) truncation with marker, UTF-8 safe |
| HOOK10 | MED | GUI broadcaster forwards errors to chat |

---

## 9. Known gaps (deferred)

LOW-severity items from the audit, deferred for a future sprint:

- **Doc footgun** — `$THCLAWS_TOOL_INPUT` in user hook scripts needs careful quoting; the chapter's recipes mostly get it right but new users may not. Needs ch13 prose update (no code change).
- **Cwd not pinned** — hook child inherits parent's `current_dir` at spawn time. Sandbox handles most cases, but a lead's BashTool `cd /elsewhere` would shift hook cwd. Could pin to sandbox root.
- **Secret-strip opt-in** — `clean_env: true` config flag to call `env_clear()` before adding `THCLAWS_*` vars. Design decision (default trust is user-by-user).
- **Strict pre/post ordering** — `pre_tool_use` is fire-and-forget; for fast-running tools the hook may not have read its env before the tool returns. Documented as best-effort.
- **Declarative filtering** — `for_tools: ["Bash"]` schema. Today users write shell `[ "$X" = Bash ] && ...` logic.
- **Field-level merge** — project's `"hooks"` block fully replaces user's. No partial-overlay merge.
- **HookEvent serde** — enum is Copy/Clone/Debug/PartialEq/Eq only. Adding Serialize/Deserialize would unlock future "log all hook fires to JSONL" / "show hook activity in /usage".
- **Other denial paths** — BashTool `lead_forbidden_command`, sandbox `check_write` failures, plan-mode blocks, subagent recursion-limit refusal don't fire `permission_denied`. Documented gap.
- **Per-event timeout override** — `HooksConfig.timeout_secs` is global. Per-event might be useful (`pre_tool_use_timeout_secs: 1, session_start_timeout_secs: 10`).
- **Compaction strategy in env** — `THCLAWS_COMPACT_STRATEGY` could expose `"compact"` vs `"clear"` (M6.4 plan-mode boundary strategy) so hooks know which kind of trim happened. Cheap addition.

---

## 10. Testing surface

`hooks::tests` (11 tests, all passing):

| Test | What it pins |
|---|---|
| `get_returns_none_for_unconfigured_hooks` | Default `HooksConfig` returns None for every event |
| `get_returns_command_for_configured_hook` | `Some("echo test")` returns the command |
| `get_skips_empty_string` | `Some("")` treated as None (defensive) |
| `event_names_are_snake_case` | Every variant maps to its config-field name (HOOK8) |
| `truncate_for_env_passes_short_strings_unchanged` | < cap → no marker, unchanged |
| `truncate_for_env_appends_marker_when_oversize` | `> cap` → trimmed + `[truncated, originally N bytes]` (HOOK9) |
| `truncate_for_env_handles_multibyte_at_boundary` | Thai-script string with cut mid-codepoint → walks back to char boundary, no U+FFFD (HOOK9) |
| `fire_handles_missing_hook_gracefully` | Default config + `fire()` → no panic, no spawn |
| `fire_actually_executes_command` (tokio::test) | Hook runs `touch '<marker>'`; marker exists within 1s — proves the wiring + reaper works end-to-end |
| `fire_passes_env_vars_to_command` (tokio::test) | Hook writes `$THCLAWS_TOOL_NAME:$THCLAWS_HOOK_EVENT` to file; asserts `"Bash:pre_tool_use"` (HOOK1 + HOOK8 combined) |
| `fire_kills_hook_on_timeout` (tokio::test) | `sleep 30` hook with `timeout_secs: 1` returns from fire() immediately; reaper completes within 2.5s (HOOK7) |

7 of these (everything below "skips empty string") are M6.35-new. Pre-fix only the 4 default-config tests existed.

Integration-level tests not added:
- Per-event call site verification (would require spinning up full Agent + WorkerState; tested by code inspection)
- Subagent inheritance (factory + Agent both have `hooks` field tested separately; integration is mechanical)
- Error broadcaster end-to-end (GUI ViewEvent capture is heavy; verified by code inspection)
- Concurrent hook firing (tokio reaper handles parallelism; high-volume soak test not in unit suite)

---

## 11. What lives where (source-line index)

| Concern | File | Symbol |
|---|---|---|
| Schema | `hooks.rs` | `HooksConfig`, `HookEvent`, `DEFAULT_HOOK_TIMEOUT_SECS`, `MAX_HOOK_ENV_BYTES` |
| Snake-case event name | `hooks.rs` | `HookEvent::name()` |
| Core fire helper | `hooks.rs` | `fire(config, event, env)` |
| Convenience fires | `hooks.rs` | `fire_pre_tool_use`, `fire_post_tool_use`, `fire_permission_denied`, `fire_session`, `fire_compact` |
| UTF-8 byte truncation | `hooks.rs` | `truncate_for_env` |
| Error broadcaster | `hooks.rs` | `set_error_broadcaster`, `report_error`, `broadcaster_slot` |
| Config field | `config.rs` | `AppConfig.hooks` |
| Shell helper | `util.rs` | `shell_command_async` (used by fire) |
| Agent hooks field | `agent.rs` | `Agent.hooks`, `Agent::with_hooks` |
| Tool dispatch fires | `agent.rs::run_turn` | pre/post_tool_use + permission_denied calls |
| Compaction fires | `agent.rs::run_turn` | pre/post_compact gated on `pre_tokens > messages_budget` |
| Subagent inheritance | `subagent.rs` | `ProductionAgentFactory.hooks` + recursive propagation + `agent.with_hooks(h)` |
| GUI hooks_arc + broadcaster | `shared_session.rs::run_worker` | snapshot + `set_error_broadcaster` registration |
| GUI session_start/end | `shared_session.rs::run_worker` | post-WorkerState build / post-input_rx exit |
| GUI rebuild_agent | `shared_session.rs::WorkerState::rebuild_agent` | `.with_hooks(Arc::new(self.config.hooks.clone()))` |
| CLI hooks_arc | `repl.rs::run_repl_with_state` | snapshot before factory |
| CLI session_start | `repl.rs::run_repl_with_state` | before readline loop |
| CLI session_end | `repl.rs::run_repl_with_state` | both EOF handlers |
| CLI rebuild sites | `repl.rs::run_repl_with_state` | model-swap / provider-swap / MCP-add agent rebuilds (3 sites) |
| User-facing chapter | `user-manual/ch13-hooks.md` | configuration + recipes |
