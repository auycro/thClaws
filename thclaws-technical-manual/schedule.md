# Schedule subsystem

Recurring agent jobs. Three composable layers — manual run, in-process scheduler, native daemon — plus three trigger types (cron, manual, workspace filesystem watch). Single user-level JSON store. Designed so each layer is shippable in isolation: the run primitive works without a scheduler, the in-process scheduler works without a daemon, and the daemon adds unattended fires + filesystem-driven triggers without breaking the simpler paths.

This doc covers the on-disk schema, the three trigger paths, the daemon lifecycle (PID file + supervisor integration), the IPC protocol the GUI add-modal uses, the slash-command surface, and the testing model. Audience is contributors and operators — for end-user instructions see [`user-manual/ch19-scheduling.md`](../user-manual/ch19-scheduling.md).

**Source modules:**
- [`crates/core/src/schedule.rs`](../thclaws/crates/core/src/schedule.rs) — single module owning the entire feature: `Schedule` / `ScheduleStore` types, `validate_cron`, `compute_next_fire` / `compute_next_n_fires`, `run_once` / `run_once_with` (subprocess fire), `InProcessScheduler` (Step 2 tick), `WatchManager` (Step 3 filesystem watcher), `run_daemon` (PID + signal lifecycle), `install_daemon` / `uninstall_daemon` (launchd plist + systemd unit emit)
- [`crates/core/src/bin/app.rs`](../thclaws/crates/core/src/bin/app.rs) — `Schedule(ScheduleCmd)` and `Daemon` clap subcommands, `run_schedule_subcommand` dispatch
- [`crates/core/src/ipc.rs`](../thclaws/crates/core/src/ipc.rs) — `schedule_add_submit` and `schedule_cron_preview` arms (~lines 158-300, 705-720)
- [`crates/core/src/repl.rs`](../thclaws/crates/core/src/repl.rs) — slash-command variants + parser + dispatch (`SlashCommand::Schedule*`, `parse_schedule_subcommand`, dispatch arms)
- [`crates/core/src/shell_dispatch.rs`](../thclaws/crates/core/src/shell_dispatch.rs) — same slash arms for the GUI Chat-tab path
- [`crates/core/src/shared_session.rs`](../thclaws/crates/core/src/shared_session.rs) — `ViewEvent::ScheduleAddOpen(String)` for the GUI modal trigger
- [`crates/core/src/event_render.rs`](../thclaws/crates/core/src/event_render.rs) — render arms that pass `ScheduleAddOpen` through to the chat dispatch
- [`frontend/src/components/ScheduleAddModal.tsx`](../thclaws/frontend/src/components/ScheduleAddModal.tsx) — schedule-add form modal (preset chips, live next-fire preview, watchWorkspace checkbox)

**Cross-references:**
- [`app-architecture.md`](app-architecture.md) — overall process model; the daemon adds a fourth surface that reuses the existing engine
- [`running-modes.md`](running-modes.md) — `thclaws daemon` is just another mode against the same `.thclaws/`
- [`hooks.md`](hooks.md) — scheduled fires inherit the project's hooks via `--print` subprocess

---

## 1. The three layers

| Layer | Trigger surface | When it fires | Shippable on its own |
|---|---|---|---|
| **Step 1 — store + manual run** | `thclaws schedule run <id>`, `/schedule run <id>` | Only when invoked by the user / external scheduler (cron, launchd, GitHub Actions) | ✅ — wire into existing crontab |
| **Step 2 — in-process scheduler** | tokio task ticking every 30s while a thclaws surface is open | Inside any process that doesn't pass `--no-scheduler` and isn't `--print` | ✅ — independent of daemon |
| **Step 3 — native daemon** | `thclaws daemon` (foreground), or supervisor (launchd / systemd-user) | Always — process supervised, restarts on crash | Builds on Step 1's `run_once`; in-process scheduler exists in parallel |

Three layers, but the fire mechanism is single-source-of-truth: every trigger calls `run_once_with(id, binary, store_path)` which spawns `thclaws --print` with the schedule's `cwd`. This keeps the agent loop / tool registry / approver / sandbox identical regardless of trigger.

```
                    ┌──────────────────────────────── triggers ────────────────────────────────┐
                    │                                                                            │
                    │  CLI / slash / external cron        InProcessScheduler::tick               │
                    │  thclaws schedule run <id>          (every 30s while GUI/CLI/serve up)    │
                    │  /schedule run <id>                                                        │
                    │                                                                            │
                    │  thclaws daemon (run_daemon)        WatchManager debounced fs event       │
                    │  └ spawn_scheduler_task             (notify watcher per                    │
                    │  └ spawn_watch_reconciler             watch_workspace=true schedule)      │
                    └────────────────────────────┬──────────────────────────────────────────────┘
                                                 │
                                                 ↓
                                ┌────────────────────────────────────┐
                                │  run_once_with(id, binary, path)   │
                                │   • load schedule from store        │
                                │   • spawn thclaws --print           │
                                │   • capture stdout+stderr to log    │
                                │   • update last_run + last_exit     │
                                └────────────────────────────────────┘
```

## 2. On-disk schema

User-level store at `~/.config/thclaws/schedules.json`. Hand-editable; daemon's reconciler picks edits up within one tick (~30s).

```json
{
  "version": 1,
  "schedules": [
    {
      "id": "morning-brief",
      "cron": "30 8 * * MON-FRI",
      "cwd": "/Users/jimmy/projects/web",
      "prompt": "summarize today's commits to ~/Desktop/brief.md",
      "model": "gpt-4o",
      "maxIterations": 30,
      "timeoutSecs": 600,
      "enabled": true,
      "watchWorkspace": false,
      "lastRun": "2026-05-06T08:30:04+00:00",
      "lastExit": 0
    }
  ]
}
```

| Field | Required | Default | Source of truth |
|---|---|---|---|
| `id` | yes | — | `Schedule.id` — also the per-id log directory name and slash-command lookup key |
| `cron` | yes | — | Validated at insert via `cron::Schedule::from_str` (after [normalization](#3-cron-handling)) |
| `cwd` | yes | — | Determines `.thclaws/settings.json`, sandbox root, memory dir, MCP config — same scoping the agent's filesystem tools enforce |
| `prompt` | yes | — | Passed as one positional arg to `thclaws --print` — multi-line is fine |
| `model` | no | absent → `cwd`'s settings | Resolved through `ProviderKind::resolve_alias` at fire time |
| `maxIterations` | no | absent → `cwd`'s `maxIterations` | Per-job iteration cap |
| `timeoutSecs` | no | 600 (CLI default) | Hard kill; fire reports `timed_out=true` |
| `enabled` | no | `true` (`default_true`) | Disabled = skip in scheduler + refuse in `schedule run` |
| `watchWorkspace` | no | `false` | Daemon-only; in-process scheduler ignores this flag |
| `lastRun` | no | absent | RFC 3339 — set by `run_once_with` after each fire |
| `lastExit` | no | absent | `Some(code)` on clean exit, `None` on timeout |

Skipped-when-`None` policy: every optional field has `#[serde(skip_serializing_if = ...)]` so a freshly-added entry serializes to a tidy 4-line JSON object rather than a wall of `null`s. `Schedule::default()` is derived for test fixtures + future literals — production callers always set the required fields explicitly.

`STORE_VERSION = 1`. Bumping the version means writing a `migrate_v1_to_v2` shim in `ScheduleStore::load_from`. None needed yet — every new field added so far has been Optional + serde-default-friendly, so v1 readers cope with v2-or-later writes.

## 3. Cron handling

5-field POSIX cron. Validated + parsed via the [`cron`](https://crates.io/crates/cron) crate, which expects 6+ fields (with seconds). `normalize_cron(expr)` prepends a `0` seconds field before parsing (`schedule.rs:208-217`). Error messages cite the user's original input, not the normalized form.

```rust
pub fn validate_cron(expr: &str) -> Result<()>;
pub fn compute_next_fire(cron_expr: &str, after: DateTime<Utc>) -> Option<DateTime<Utc>>;
pub fn compute_next_n_fires(cron_expr: &str, after: DateTime<Utc>, n: usize) -> Vec<DateTime<Utc>>;
```

`compute_next_n_fires` is what powers the modal's live preview — given the user's typed cron + `Utc::now()`, return the next 3 fire times. Pure parser call, no I/O.

**Schedule semantics for the in-process scheduler:**

In-memory cursor per schedule (`InProcessScheduler.cursors: HashMap<String, DateTime<Utc>>`). On first sight, seeded from:
- `parse_last_run(schedule)` if the entry has a `lastRun` (so manual catch-up via JSON edit works), else
- `now` (skip-catch-up — fresh schedule does NOT retroactively fire missed events).

Per tick: `compute_next_fire(cron, cursor) <= now` → fire, advance cursor to that fire time. Cursor advances by exactly one cron period per tick — multiple fires in a single `now` are possible across consecutive ticks if catching up.

Skip-overlap: `running: Arc<Mutex<HashSet<String>>>`. If a schedule's previous fire is still spawn_blocking-running, its next due fire is skipped. Concurrent fires of the same schedule are not queued.

## 4. Step 1 — manual run primitive

The fire is unconditionally synchronous, captures everything to a per-run log file, and updates the store on completion. Never goes through tokio — pure `std::process::Command`.

```rust
pub fn run_once(id: &str, binary_path: &Path) -> Result<RunOutcome>;
pub fn run_once_with(id: &str, binary_path: &Path, store_path: Option<&Path>) -> Result<RunOutcome>;

pub struct RunOutcome {
    pub log_path: PathBuf,
    pub exit_code: Option<i32>,
    pub duration: Duration,
    pub timed_out: bool,
}
```

Spawn shape (`spawn_job`, `schedule.rs:~340`):

```rust
let mut cmd = Command::new(binary_path);
cmd.arg("--print")
    .arg(&schedule.prompt)
    .current_dir(&schedule.cwd)        // determines `.thclaws/`, sandbox, memory, MCP
    .stdin(Stdio::null())
    .stdout(Stdio::from(log_file))
    .stderr(Stdio::from(log_file_for_err))
    .env("THCLAWS_SCHEDULE_ID", &schedule.id);
if let Some(ref m) = schedule.model { cmd.arg("--model").arg(m); }
if let Some(n) = schedule.max_iterations { cmd.arg("--max-iterations").arg(n.to_string()); }
#[cfg(windows)] cmd.creation_flags(0x0800_0000);   // CREATE_NO_WINDOW — no console flicker per fire
```

`THCLAWS_SCHEDULE_ID` is exported to the spawned process so hook authors / agent prompts can detect "I was launched by the scheduler."

Logs land at `~/.local/share/thclaws/logs/<id>/<ts>.log` — one file per fire. Filename uses `2026-05-06T08-30-04Z` (colons replaced with `-` so Windows accepts it). After completion, `lastRun` (RFC 3339) and `lastExit` (`Some(code)` or `None` on timeout) are written back to the store.

`run_once_with` exists for tests: takes an explicit `store_path` and routes the load+save through that, so the integration tests can use a tempdir-backed store without polluting `~/.config/thclaws/schedules.json`. Same pattern as `InProcessScheduler::with_store_path`.

Timeout enforcement: `wait_with_timeout(child, duration)` polls `child.try_wait()` every 100ms. On timeout: `child.kill()` + `child.wait()`, returns `(None, true)`. 100ms granularity is fine — minute-scale cron has no use for tighter polling.

## 5. Step 2 — in-process scheduler

```rust
pub struct InProcessScheduler {
    cursors: HashMap<String, DateTime<Utc>>,            // per-schedule "last considered" timestamp
    running: Arc<Mutex<HashSet<String>>>,               // skip-overlap guard
    binary: PathBuf,                                    // current_exe at spawn time
    store_path: Option<PathBuf>,                        // None = default user-level path
}
```

`spawn_scheduler_task(binary)` spawns a forever loop:

```rust
loop {
    tokio::time::sleep(Duration::from_secs(30)).await;     // TICK_INTERVAL
    sched.tick(Utc::now());
}
```

`tick(&mut self, now)` reloads the store, walks enabled schedules, applies cursor + skip-overlap, and fires due jobs on `tokio::task::spawn_blocking` so `std::process::Command::wait` doesn't park a tokio worker.

```rust
pub fn tick(&mut self, now: DateTime<Utc>) -> Vec<(String, JoinHandle<()>)>
```

Returns JoinHandles so tests can await fires deterministically; production drops them (fire-and-forget).

**Where it spawns:** `app.rs::main` after CLI parse, gated by `!cli.print && !cli.no_scheduler && cli.command.is_none()`. So it runs in:
- Default GUI mode (no subcommand)
- `--cli` REPL mode
- `--serve` (alone or `--serve --gui`)

It does NOT run when:
- `--print` (short-lived; subprocess noise from the scheduler would clutter the print output)
- `--no-scheduler` flag (explicit opt-out — used when the daemon is also installed)
- A `Command::Schedule(...)` or `Command::Daemon` subcommand is active (those handle their own surface lifecycle)

The daemon spawns its own scheduler internally (via `run_daemon` → `spawn_scheduler_task`), and the `cli.command` check above ensures the auto-spawn path doesn't double up.

## 6. Step 3 — native daemon

Long-running foreground process supervised by launchd (macOS) or systemd-user (Linux). Reuses the in-process scheduler and adds:
1. A PID file at `~/.local/state/thclaws/scheduler.pid` (so `schedule status` can answer "is the daemon up?" without IPC, and the next start can detect a stale file).
2. Filesystem watchers for `watchWorkspace=true` schedules (`WatchManager`).

### 6.1 Lifecycle

```rust
pub async fn run_daemon() -> Result<()>;
```

```
                run_daemon
                    │
                    ↓
        daemon_status() == Running(p) ?  ──→ Err("another daemon is already running (pid {p})")
                    │ NotRunning | Stale
                    ↓
        write_pid_file()      (~/.local/state/thclaws/scheduler.pid)
                    │
                    ↓
        spawn_scheduler_task(binary)        (the cron tick task — same struct as Step 2)
                    │
                    ↓
        spawn_watch_reconciler(binary)      (the WatchManager rebuilder — every 30s)
                    │
                    ↓
        select! {  SIGTERM   → log + fall through
                   SIGINT    → log + fall through  }   (Unix; ctrl_c on Windows)
                    │
                    ↓
        scheduler_handle.abort()
        watch_handle.abort()
        remove_pid_file()
        log "stopped cleanly"
        Ok(())
```

### 6.2 PID file

```rust
pub enum DaemonStatus { Running(u32), Stale(u32), NotRunning }
pub fn daemon_status() -> DaemonStatus;
pub fn pid_file_path() -> Option<PathBuf>;        // ~/.local/state/thclaws/scheduler.pid
```

PID liveness check via `libc::kill(pid, 0)` on Unix (`kill(pid, 0)` is the no-signal probe — returns success if the process exists or `EPERM` if it exists but is owned by another user). Windows path is stubbed (always returns `false`); `pid_alive` lives in a `#[cfg(unix)]` block with a `#[cfg(not(unix))]` fallback. See [section 9](#9-windows-status).

Atomic write: `write_pid_file` writes to `<path>.tmp` and renames into place so a half-written PID file can never be observed by `schedule status`.

Stale detection: if `daemon_status` returns `Stale(pid)`, `run_daemon` logs `[daemon] reclaiming stale PID file (last pid {pid})` and proceeds — the file gets overwritten by the new fire's atomic write. Manual cleanup is never required.

Double-daemon guard: `daemon_status() == Running(p)` short-circuits with an error before any state mutation. `launchctl bootstrap` invoking a second daemon while the first is alive errors clearly rather than silently corrupting the PID file.

### 6.3 Supervisor integration

```rust
pub struct InstallReport { pub supervisor_path: PathBuf, pub next_steps: Vec<String> }
pub fn install_daemon() -> Result<InstallReport>;
pub fn uninstall_daemon() -> Result<PathBuf>;
pub fn supervisor_file_path() -> Option<PathBuf>;
```

**macOS** (`#[cfg(target_os = "macos")]`):

`install_daemon` writes `~/Library/LaunchAgents/sh.thclaws.scheduler.plist` (the `DAEMON_LABEL` constant) with:

```xml
<key>ProgramArguments</key>
<array>
    <string>/absolute/path/to/thclaws</string>
    <string>daemon</string>
</array>
<key>RunAtLoad</key><true/>
<key>KeepAlive</key><true/>
<key>ProcessType</key><string>Background</string>
<key>StandardOutPath</key><string>~/.local/share/thclaws/daemon.log</string>
<key>StandardErrorPath</key><string>~/.local/share/thclaws/daemon.log</string>
```

`KeepAlive=true` + `RunAtLoad=true` means launchd auto-restarts if the daemon dies and starts it on every login.

After writing the plist, `install_daemon` runs `launchctl bootout gui/$UID <plist>` (idempotent — ignores errors when no prior plist) then `launchctl bootstrap gui/$UID <plist>`. UID via `libc::getuid()`. No sudo needed — `~/Library/LaunchAgents/` is the user's own directory.

**Linux** (`#[cfg(target_os = "linux")]`):

Writes `~/.config/systemd/user/thclaws-scheduler.service`:

```ini
[Unit]
Description=thClaws scheduler daemon
After=default.target

[Service]
Type=simple
ExecStart=/usr/local/bin/thclaws daemon
Restart=on-failure
RestartSec=5
StandardOutput=append:/home/x/.local/share/thclaws/daemon.log
StandardError=append:/home/x/.local/share/thclaws/daemon.log

[Install]
WantedBy=default.target
```

Returns `next_steps` listing the systemctl commands rather than auto-starting (Linux distros vary in how systemd-user is configured; we'd rather not assume `loginctl enable-linger` was set).

`uninstall_daemon` runs `launchctl bootout` (macOS) or `systemctl --user disable --now` (Linux) then removes the file. Tolerates already-gone supervisor entries — useful if the user ran `launchctl bootout` manually before reaching `schedule uninstall`.

### 6.4 Workspace-change trigger (`WatchManager`)

```rust
pub struct WatchManager {
    _debouncers: Vec<Debouncer<RecommendedWatcher>>,    // Drop = stop watching
    dispatch_handle: Option<JoinHandle<()>>,            // aborted on Drop
}

pub fn from_store(store: &ScheduleStore, binary: PathBuf) -> Result<Self>;
pub fn from_store_with_path(store, binary, store_path: Option<PathBuf>) -> Result<Self>;
```

One [`notify-debouncer-mini`](https://crates.io/crates/notify-debouncer-mini) debouncer per `enabled && watch_workspace` schedule. Debounce window: `WATCH_DEBOUNCE = 2s`. Each watcher is recursive on its schedule's `cwd`.

Cooldown: `WATCH_COOLDOWN = 60s` after a fire starts. Per-schedule `last_fire_at: HashMap<String, Instant>` — the dispatcher checks `(now - t).elapsed() < COOLDOWN` before firing. Combined with the `running` skip-overlap guard, this keeps the spawned `--print` job's own writes back into `cwd` from immediately re-triggering a fire-storm.

Hardcoded ignore list (`IGNORED_SEGMENTS`):

```
.thclaws .git .DS_Store node_modules target dist build .next .cache
```

Match logic: walk the path components between `cwd` and the leaf; if any component equals one of these segments, ignore the event. `is_path_ignored(root, changed)` canonicalizes both ends before the `strip_prefix` check (macOS FSEvents reports paths under `/private/var/folders/...` even when the watcher was registered against `/var/folders/...`, so without canonicalize the strip_prefix would mismatch and ignored events would slip through). Falls back to literal-equality strip when canonicalize fails (path may have been deleted between event and check).

Reconciliation: `spawn_watch_reconciler(binary)` rebuilds the entire `WatchManager` every 30s (same `TICK_INTERVAL` as the cron scheduler). Drop-then-replace, so old watchers stop before new ones spawn — no brief window of duplicate events:

```rust
let mut _current: Option<WatchManager> = None;
loop {
    let store = ScheduleStore::load()?;
    _current = None;                             // drop old watchers explicitly first
    _current = Some(WatchManager::from_store(&store, binary.clone())?);
    tokio::time::sleep(TICK_INTERVAL).await;
}
```

Failed-to-watch handling: if a schedule's `cwd` doesn't exist, or `notify` can't bind (e.g. inotify limit reached on Linux), the daemon logs an error and skips that schedule. Other schedules' watchers continue to work.

## 7. CLI surface

| Subcommand | Implemented at | Behavior |
|---|---|---|
| `thclaws schedule add <id> --cron …` | `bin/app.rs:run_schedule_subcommand` | Build a `Schedule`, call `store.add` (errors on duplicate id, validates cron), `store.save` |
| `thclaws schedule list` | same | Compact one-line-per-schedule with on/off + ok/err + `+watch` indicator |
| `thclaws schedule show <id>` | same | Pretty-print the entry as JSON |
| `thclaws schedule rm <id>` | same | Remove from store; does NOT delete the per-id log directory |
| `thclaws schedule run <id>` | same | Synchronous fire via `run_once`; returns child's exit code (`124` on timeout, mirroring GNU `timeout(1)`) |
| `thclaws schedule install` | same → `install_daemon` | Write supervisor file + auto-bootstrap (macOS) / print next-steps (Linux) |
| `thclaws schedule uninstall` | same → `uninstall_daemon` | Bootout + remove supervisor file |
| `thclaws schedule status` | same → `daemon_status` | Print Running / Stale / NotRunning + recent-fires table |
| `thclaws daemon` | `bin/app.rs::main` Command::Daemon arm → `run_daemon` | Foreground process; SIGTERM/SIGINT → graceful shutdown |

`--watch` flag on `schedule add` sets `watch_workspace: true`. Other flags: `--cwd` (defaults to current_dir), `--model`, `--max-iterations`, `--timeout` (default 600, `0` = no timeout), `--disabled`.

## 8. Slash-command surface

REPL + GUI Chat tab. Same handlers in `repl.rs` and `shell_dispatch.rs` (the GUI Chat-tab dispatch path). Parser lives in `repl::parse_schedule_subcommand`.

| Slash | `SlashCommand` variant | Behavior |
|---|---|---|
| `/schedule` or `/schedule list` (`/sched` alias) | `Schedule` | Same output as `thclaws schedule list` |
| `/schedule show <id>` | `ScheduleShow(String)` | JSON dump |
| `/schedule run <id>` | `ScheduleRun(String)` | Fire on `spawn_blocking` so the REPL stays responsive while the child runs |
| `/schedule status` | `ScheduleStatus` | Daemon status + recent fires |
| `/schedule pause <id>` / `resume <id>` | `SchedulePause(String)` / `ScheduleResume(String)` | Flip `enabled` via `toggle_schedule_enabled` helper |
| `/schedule rm <id>` (`remove`/`delete` aliases) | `ScheduleRm(String)` | Remove + save |
| `/schedule install` / `uninstall` | `ScheduleInstall` / `ScheduleUninstall` | `spawn_blocking(install_daemon)` / `spawn_blocking(uninstall_daemon)` so `launchctl` shell-out doesn't block readline |
| `/schedule add` | `ScheduleAdd` | **CLI:** prints help text pointing at the shell subcommand. **GUI Chat tab:** dispatches `ViewEvent::ScheduleAddOpen(payload)` carrying defaults JSON; the React modal subscribes and opens |

`/schedule add` is the only command with surface-divergent behavior. The CLI REPL prints a 5-line help blurb (multi-line prompt + nine flags don't fit on one readline line); the GUI Chat tab opens the modal.

## 9. Frontend modal — `ScheduleAddModal`

Self-contained React component, mounted once in `App.tsx` next to `ApprovalModal`. Subscribes to `schedule_add_open` from the backend, renders form, submits via `schedule_add_submit`, dismisses on `schedule_add_result`.

### 9.1 IPC protocol

Backend → frontend:

```json
// Triggered by `/schedule add` from a GUI surface (shell_dispatch.rs builds this):
{
  "type": "schedule_add_open",
  "defaults": {
    "cwd": "/Users/jimmy/projects/foo",
    "timeoutSecs": 600,
    "cron": "30 8 * * MON-FRI"
  }
}

// Response to schedule_add_submit:
{
  "type": "schedule_add_result",
  "ok": true,
  "id": "morning-brief"
}
{
  "type": "schedule_add_result",
  "ok": false,
  "error": "id is required; cron is required"
}

// Response to schedule_cron_preview:
{
  "type": "schedule_cron_preview_result",
  "cron": "0 9 * * *",
  "ok": true,
  "fires": ["2026-05-07T09:00:00+00:00", "2026-05-08T09:00:00+00:00", "2026-05-09T09:00:00+00:00"]
}
{
  "type": "schedule_cron_preview_result",
  "cron": "definitely not cron",
  "ok": false,
  "error": "invalid cron expression 'definitely not cron': …"
}
```

Frontend → backend:

```json
{
  "type": "schedule_add_submit",
  "id": "morning-brief",
  "cron": "30 8 * * MON-FRI",
  "prompt": "...",
  "cwd": "/Users/jimmy/projects/foo",
  "model": "gpt-4o",
  "maxIterations": 30,
  "timeoutSecs": 600,
  "disabled": false,
  "watchWorkspace": false
}

{
  "type": "schedule_cron_preview",
  "cron": "0 9 * * *"
}
```

The submit handler in `ipc.rs::handle_ipc` validates required fields (`id`, `cron`, `prompt`, `cwd`) + cron syntax + cwd existence in that order. Errors are joined with `; ` so a single invalid form surfaces every problem at once.

### 9.2 Modal helpers

**Cron preset chips** — frontend-only, instant. Hardcoded list in `ScheduleAddModal.tsx`:

```ts
const CRON_PRESETS = [
  { label: "Every 5 min",   cron: "*/5 * * * *" },
  { label: "Every 30 min",  cron: "*/30 * * * *" },
  { label: "Hourly",        cron: "0 * * * *" },
  { label: "Daily 9am",     cron: "0 9 * * *" },
  { label: "Weekdays 8:30", cron: "30 8 * * MON-FRI" },
  { label: "Weekly Mon 9am",cron: "0 9 * * MON" },
  { label: "Monthly 1st",   cron: "0 0 1 * *" },
];
```

Click → `setForm({...form, cron: preset.cron})`. Active chip (matching the current cron field exactly) lights up in `var(--accent)`.

**Live next-fire preview** — debounced 300ms. `useEffect` on `form?.cron` cleans up the previous timer and schedules a new `send({type: "schedule_cron_preview", cron})`. The result handler matches the response's `cron` field against the CURRENT form state and ignores stale responses (user has typed more characters since the IPC was sent).

**`watchWorkspace` checkbox** — `Run when file in workspace changes`. Submitted as `watchWorkspace: true`. Disabled when the daemon isn't installed, the in-process scheduler will silently ignore the field. Documented in the help blurb beside the checkbox.

## 10. Storage paths

| Path | Owner | Contents |
|---|---|---|
| `~/.config/thclaws/schedules.json` | `ScheduleStore::default_path` | The schedule store. Hand-editable, versioned (`"version": 1`), tidy when `Option::None` fields are stripped on save |
| `~/.local/share/thclaws/logs/<id>/<ts>.log` | `log_dir_for(id)` | One file per fire — combined stdout + stderr from the spawned `--print`. Filename ts is `2026-05-06T08-30-04Z` (Windows-safe colons) |
| `~/.local/state/thclaws/scheduler.pid` | `pid_file_path` | Daemon PID; checked by `daemon_status` + the double-daemon guard |
| `~/Library/LaunchAgents/sh.thclaws.scheduler.plist` (macOS) | `supervisor_file_path` | launchd plist; written by `install_daemon` |
| `~/.config/systemd/user/thclaws-scheduler.service` (Linux) | same | systemd-user unit |
| `~/.local/share/thclaws/daemon.log` | launchd `StandardOutPath` / systemd `StandardOutput` | The daemon's own stderr/stdout (separate from per-job logs in `logs/<id>/`) |

XDG-style on Linux; same paths on macOS to keep one consistent place to look across the two OSes. Windows uses `%USERPROFILE%\.config\thclaws\...` etc — unidiomatic for Windows users but functional. Daemon support on Windows is deferred (see [section 11](#11-windows-status)).

## 11. Trigger-source matrix

| Trigger | Step 1 (`schedule run`) | Step 2 (in-process) | Step 3 (daemon) |
|---|---|---|---|
| Manual CLI / slash `run <id>` | ✅ source | ✅ via `tick`-driven `run_once` | ✅ via `tick`-driven `run_once` |
| Cron schedule | ❌ — needs a scheduler | ✅ ticks every 30s | ✅ ticks every 30s |
| `watchWorkspace` filesystem | ❌ | ❌ — the in-process scheduler ignores the flag | ✅ via `WatchManager` |

The fire mechanism is identical across all rows — every trigger calls `run_once_with(id, binary, store_path)`. Only the trigger source differs.

## 12. Windows status

Step 1 + Step 2 should work on Windows (cross-platform Rust + std + tokio + chrono + cron + serde + notify-debouncer-mini). Untested in practice — CI excludes Windows because of path-separator assumptions in older tests, and the dev loop is macOS-first.

Step 3 is explicitly stubbed:

| Component | Windows behavior |
|---|---|
| `pid_alive` | `#[cfg(not(unix))]` returns `false` always → `daemon_status` reports `NotRunning` or `Stale` even when the process exists |
| `install_daemon` / `uninstall_daemon` | Hit the `#[cfg(not(any(target_os = "macos", target_os = "linux")))]` branch and return `"daemon install not yet supported on this platform (target_os=windows)"` |
| `run_daemon` signal handler | `tokio::signal::ctrl_c` (covers Ctrl-C only — service-stop on a real Windows Service uses SCM control codes, not handled) |
| `WatchManager` | Compiles, but never gets exercised because `install_daemon` errors out; the in-process scheduler runs fine but ignores `watchWorkspace` |

Adding real Windows support means: Task Scheduler XML emit (or `windows-service` crate for SCM integration), Named Pipe IPC if/when `schedule reload` becomes a thing, OpenProcess-based `pid_alive`, code-signing the binary to clear SmartScreen. ~2-3 days of focused work — deferred until someone actually needs it.

## 13. Test surface

`cargo test --features gui --lib` runs everything. The schedule module's tests are split into:

**Unit-level (fast, no FS / spawn):**
- `cron_validation_*` — accepts/rejects 5-field POSIX, prepended seconds, range/list syntax
- `compute_next_fire_*` and `compute_next_n_fires_*` — exact-time assertions against fixed `after` timestamps
- `parse_last_run_*` — RFC 3339 round-trip
- `schedule_store_roundtrip`, `add_*`, `remove_*` — pure data-layer tests
- `is_path_ignored_*` — four cases covering ignored segments, allowed paths, paths outside root
- `paths_equivalent_*` — canonicalize-vs-strict-equality covered for missing paths
- `pid_alive_detects_self`, `pid_alive_rejects_dead_pid` — `kill(pid, 0)` probe
- `launchd_plist_has_required_keys` (macOS) / `systemd_unit_has_required_keys` (Linux) — string-substring assertions on the rendered supervisor file
- `schedule_serde_omits_watch_workspace_when_false` — `skip_serializing_if` keeps the JSON tidy

**Integration-level (~1-5s each, real `std::process::Command` + tempdir + tokio runtime):**
- `spawn_job_captures_exit_and_writes_log` — drops a fake `#!/bin/sh` binary, asserts cwd is honored + log captures stdout + exit code is 7 (the script returns 7)
- `spawn_job_enforces_timeout` — sleeper script + 1s timeout → `timed_out=true`, `exit_code=None`
- `tick_lifecycle_end_to_end` — fresh schedule + due-cron schedule + disabled schedule in one tick: only the due one fires; cursor advances; second tick at same `now` walks past
- `watch_manager_fires_on_file_change` — real `notify` watcher, real fs write to the watched cwd, observe `lastRun` set in the tempdir-local store after 4s
- `watch_manager_ignores_internal_thclaws_writes` — writes into `.thclaws/` and `.git/` do NOT trigger fires
- `watch_manager_skips_watch_workspace_false` — schedules with the flag off don't get a watcher

**IPC-handler tests (in `ipc.rs`):**
- `schedule_add_submit_rejects_missing_fields` / `_rejects_bad_cron` / `_rejects_missing_cwd` — captures dispatched payloads via a `Mutex<Vec<String>>`, asserts the `ok: false` envelope shape and error content. Exercises validation without ever calling `ScheduleStore::save`, so the user's real `~/.config/thclaws/schedules.json` is untouched.
- `schedule_cron_preview_valid` / `_invalid` / `_empty` — same pattern for the cron preview IPC

Test-time isolation: `WatchManager::from_store_with_path` and `InProcessScheduler::with_store_path` thread an explicit `store_path` through the dispatcher to `run_once_with`, so integration tests can use a tempdir-backed store without polluting the user's real schedules.json. Per-id log dirs are cleaned up at the end of each test.

Total schedule-related tests: 24 unit + 6 integration + 8 IPC = ~38 of the project's 931 lib tests.

## 14. Operations notes

**Watching the daemon during development:**

```sh
tail -f ~/.local/share/thclaws/daemon.log
```

Color escapes are preserved in the log; `cat` works fine for review. Lines look like:

```
[36m[daemon] thclaws scheduler started (pid 88294, pid file ~/.local/state/thclaws/scheduler.pid)[0m
[36m[schedule] in-process scheduler running (tick 30s)[0m
[36m[watch] 'morning-brief': watching /Users/jimmy/projects/foo (debounce 2s, cooldown 60s)[0m
[36m[watch] 'morning-brief': fired (changed: /Users/jimmy/projects/foo/notes.md)[0m
[36m[schedule] 'morning-brief' fired — exit=0 duration=38.412s log=/Users/jimmy/.local/share/thclaws/logs/morning-brief/2026-05-06T08-30-04Z.log[0m
[36m[daemon] SIGTERM received — shutting down[0m
[36m[daemon] stopped cleanly[0m
```

**Debugging "schedule didn't fire":**
1. `thclaws schedule status` — daemon up?
2. `tail -50 ~/.local/share/thclaws/daemon.log` — any error?
3. `thclaws schedule show <id>` — `enabled: true`?  Reasonable cron?
4. If using `watchWorkspace`: is the path you changed inside an [ignored segment](#64-workspace-change-trigger-watchmanager)?
5. Skip-catch-up: a freshly-added schedule does NOT retroactively fire missed events. Edit `lastRun` to a timestamp before the events you want to replay.
6. macOS lid-closed: LaunchAgents don't fire while the laptop sleeps. Tune `WakeMonitor` in the plist if you need wake-on-schedule (deferred — not in v1).

**Killing a runaway fire:**
Per-fire is a child process — `ps aux | grep thclaws` to find it, `kill <pid>`. The daemon's `running` set will retain the id until the JoinHandle resolves; on the next tick that schedule will be eligible again.

**Disk usage:**
- `~/.local/share/thclaws/logs/<id>/*.log` grows unbounded (no rotation in v1)
- `~/.local/share/thclaws/daemon.log` grows unbounded (launchd doesn't rotate)
- Manual prune: `find ~/.local/share/thclaws/logs -mtime +30 -delete` for old per-job logs, `> ~/.local/share/thclaws/daemon.log` to truncate

## 15. Deferred / known gaps

- **Windows daemon** — see [section 12](#12-windows-status)
- **IPC socket** — daemon and CLI talk only via store + PID file. `schedule reload`, live `schedule logs --tail`, daemon-side metrics are deferred. The 30s reconciler picks up store edits within one tick.
- **Catch-up policy field** — only `skip-catch-up` is supported. Manual catch-up via `lastRun` editing is the workaround.
- **Log rotation** — v1 ships no rotation for either the daemon log or the per-job logs.
- **`.gitignore` integration for the watch trigger** — only the hardcoded segment list is honored. Users with unusual layouts have to drop `watchWorkspace` and lean on cron.
- **inotify exhaustion** — Linux `fs.inotify.max_user_watches` defaults to 8192 per user. Recursive watches on huge trees can blow that. The daemon logs the error and skips the watcher; other schedules continue. No retry / quota-bumping logic.
- **Cooldown is fixed (60s)** — not configurable per schedule. v1 trade-off; bump to a `cooldownSecs` field if a use case demands it.
- **No per-schedule paused-by-system semantics** — if the laptop is closed or the daemon is stopped, the schedule simply doesn't fire and `lastRun` stays where it was. There's no way to ask "fire all missed events on resume" beyond the manual `lastRun` workaround.
