# Sessions

Append-only JSONL persistence for conversations. Each session = one `<id>.jsonl` file under `<cwd>/.thclaws/sessions/`. Worker writes events as the agent loop produces them (header at session-mint, one event per turn / rename / plan mutation / compaction); `load_from` replays the file to reconstruct the in-memory `Session`. The sidebar lists sessions by `updated_at`; `/load <id-or-name>` swaps the active conversation.

This doc covers: the JSONL format, every event type, the session lifecycle (new / save / load / rename / delete / gc), `SessionStore` discovery + resolution, plan-mode integration via the broadcaster, compaction checkpoint replay, the M6.14 absolute-path fix and M6.19 per-line salvage, the auto-model-switch on `/load`, the cross-process concurrency model, and the testing surface.

**Source modules:**
- `crates/core/src/session.rs` — `Session`, `SessionMeta`, `SessionStore`, JSONL serialization (`SessionHeader`, `MessageEvent`, `RenameEvent`, `PlanSnapshotEvent`, `CompactionEvent`), `Session::write_header_if_missing`, `append_to`, `append_rename_to`, `append_compaction_to`, `append_plan_snapshot`, `load_from`
- `crates/core/src/shared_session.rs` — `WorkerState.session` + `WorkerState.session_store`, `ShellInput::NewSession` / `LoadSession` / `SessionDeletedExternal` / `SessionRenamedExternal`, `save_history`, `build_session_list`, plan-state broadcaster setup
- `crates/core/src/gui.rs` — `session_load` / `session_rename` / `session_delete` IPC handlers, `build_session_list` (frontend payload)
- `crates/core/src/repl.rs` — CLI `/sessions`, `/save`, `/load`, `/resume`, `/rename`, `/fork` handlers
- `crates/core/src/tools/plan_state.rs` — broadcaster fired on every plan mutation (writes `plan_snapshot` events to the active session JSONL)
- `crates/core/src/compaction.rs` — `compact_with_summary` writes the `CompactionEvent` checkpoint via `Session::append_compaction_to`

**Cross-references:**
- [`agentic-loop.md`](agentic-loop.md) — what populates `state.session.messages` (the agent loop appends `assistant` and `user-with-tool-results` messages every iteration)
- [`context-composer.md`](context-composer.md) — `state.session_store` is consulted for sidebar refreshes; never read by the composer itself (sessions don't feed the system prompt)
- [`plugins.md`](plugins.md), [`skills.md`](skills.md) — separate persistence scopes; sessions live alongside but don't share registry

---

## 1. Overview

```
USER MESSAGE  → ShellInput::Line ("hello")  → handle_line
                                                  │
                                                  ▼
                                        Agent::run_turn (full pipeline)
                                                  │
                                                  ▼
                                        save_history(&state.agent, &mut state.session, ...)
                                                  │
                                                  ▼
                                        session.append_to(<store>/<id>.jsonl)
                                                  │
                                                  └── one MessageEvent per new message

OTHER MUTATIONS:
  /rename "..."        → session.append_rename_to(...)        → RenameEvent
  Plan tool call       → broadcaster → append_plan_snapshot(...) → PlanSnapshotEvent
  /compact             → session.append_compaction_to(...)   → CompactionEvent (replay checkpoint)
  worker spawn / new   → session.write_header_if_missing(...) → SessionHeader (idempotent)
```

```
USER CLICK on sidebar entry → session_load IPC → ShellInput::LoadSession(id)
                                                          │
                                                          ▼
                                              SessionStore::load(id)
                                                          │
                                                          ▼
                                              Session::load_from(<id>.jsonl)
                                                          │
                                                          └── replays every event in order
                                                              (header → messages → renames →
                                                               plan_snapshot → compaction
                                                               checkpoints → more messages)
```

Each thClaws project has its own session directory (`<project>/.thclaws/sessions/`). Switching workspace via the GUI sidebar's folder icon (or `/cwd` via the CLI) re-resolves the directory; the M6.14 fix wires this through `state.session_store` rebuilds.

### Where sessions DON'T go

Sessions are *not* part of the context composer's input — they're the persistence layer for the conversation, not a contributor to the system prompt. The agent reads `state.session.messages` only when populating `agent.history` after `LoadSession`. The composer ([`context-composer.md`](context-composer.md)) sees the same in-memory history via `agent.history_snapshot()` but doesn't touch the JSONL.

---

## 2. JSONL format

One JSON object per line. Lines are appended; the file is never rewritten in-place. Order matters — `load_from` replays sequentially.

### Header (one per session, written first)

```json
{"type":"header","id":"sess-18abd75ec646b160","model":"claude-sonnet-4-5","cwd":"/Users/jimmy/proj","created_at":1777751279}
```

| Field | Type | Notes |
|---|---|---|
| `type` | `"header"` | Sentinel |
| `id` | string | Format `sess-{nanos:x}` (16 hex digits). Globally unique within the project's session dir. |
| `model` | string | Model the session was created with — used for the auto-switch on `/load` |
| `cwd` | string | The cwd at session mint time, informational |
| `created_at` | u64 | Unix seconds |

### Message event (one per saved turn message)

```json
{"type":"user","content":[{"type":"text","text":"hello"}],"timestamp":1777751283}
{"type":"assistant","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"toolu_01","name":"Read","input":{"path":"src/main.rs"}}],"timestamp":1777751290}
{"type":"user","content":[{"type":"tool_result","tool_use_id":"toolu_01","content":"// fn main() {\n}\n","is_error":false}],"timestamp":1777751291}
```

| `type` | `"user"` / `"assistant"` / `"system"` | The conversational role |
| `content` | `Vec<ContentBlock>` | Same shape the agent loop persists to history (text / thinking / tool_use / tool_result / image) |
| `timestamp` | u64 | Unix seconds |

`content` is reused verbatim from `crate::types::ContentBlock` — multimodal blocks (image, tool_result with multipart) round-trip transparently.

### Rename event (zero or more)

```json
{"type":"rename","title":"shopflow audit","timestamp":1777751400}
```

Latest in file order wins. Empty / whitespace-only title clears back to `None`. Title is sanitized at write time (M6.19 BUG L1+L5): `\n`/`\r`/`\t` collapse to spaces, other control chars are stripped, then trimmed. So `"  multi\nline  "` persists as `"multi line"`.

### Plan snapshot event (one per plan-tool mutation)

```json
{"type":"plan_snapshot","plan":{"id":"plan-1","steps":[...]},"timestamp":1777751500}
{"type":"plan_snapshot","plan":null,"timestamp":1777751600}
```

Latest in file order wins. `null` plan means the active plan was cleared (`plan_state::clear()`).

**Critical: `plan_snapshot` events do NOT bump `updated_at` for sort recency.** Pre-M6.16.1 they did, which made every `/load` jump the just-clicked session to the top of the sidebar (since `restore_from_session` fires the broadcaster on load, writing a fresh-timestamped snapshot). The reader filters them out of the recency calc — only message / rename / compaction events count as activity.

### Compaction event (zero or more, written by `/compact`)

```json
{"type":"compaction","messages":[{"role":"user","content":[...]},...],"replaces_count":42,"timestamp":1777752000}
```

A replay checkpoint. `load_from` clears the in-memory `messages` Vec when it hits this line, then refills from `messages` in the event. Subsequent message events in the file append after the checkpoint.

The original message events that preceded the compaction stay on disk forever (audit trail) — they're just overridden by the checkpoint on load.

`replaces_count` is informational only; the load logic walks sequentially and resets on each checkpoint, so the field isn't strictly required.

---

## 3. The Session struct

```rust
pub struct Session {
    pub id: String,
    pub created_at: u64,
    pub updated_at: u64,
    pub model: String,
    pub cwd: String,
    pub messages: Vec<Message>,
    pub title: Option<String>,
    pub last_saved_count: usize,
    pub plan: Option<crate::tools::plan_state::Plan>,
}
```

`last_saved_count` is the number of messages already persisted; `append_to` only writes from `messages[last_saved_count..]` and bumps the field. After any `compaction`, this is reset to the post-checkpoint length.

`PartialEq` deliberately ignores `last_saved_count`, `title`, and `plan` — two `Session` values with identical id/messages/etc compare equal even if their persistence-bookkeeping fields differ. Used by tests.

### `SessionMeta` — sidebar payload

```rust
pub struct SessionMeta {
    pub id: String,
    pub updated_at: u64,
    pub model: String,
    pub message_count: usize,
    pub title: Option<String>,
}
```

What `SessionStore::list()` returns. Trimmed for sidebar rendering — no message bodies, no plan, no cwd.

---

## 4. `SessionStore`

```rust
pub struct SessionStore { pub root: PathBuf }
```

Just a wrapper around the sessions directory. `default_path()` returns `<cwd>/.thclaws/sessions/` — always project-scoped (M6.14 priority). The directory is created on first save; `default_path` doesn't materialize it just to list.

### `validate_id`

Every external-input path runs through this gate before touching the filesystem:

```rust
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() { return Err(...) }
    if id.len() > 249 { return Err(...) }              // POSIX 255 - ".jsonl" suffix
    if id.contains("..") || id.chars().any(forbidden_chars) { return Err(...) }
    if Path::new(id).is_absolute() { return Err(...) }
    Ok(())
}
```

`forbidden_chars` covers `/`, `\`, `\0`, and `is_control()`. So slash-traversal, NUL-injection, and control-char names are all blocked. All `path_for` callers go through `validate_id` (via `load`, `save`, `rename`, `delete`, or `resolve_id`'s own check); the `path_for` method itself is technically public but every external entry point gates on validate first.

### `resolve_id` — name + id resolution

`/load <name-or-id>` accepts:
1. Exact id match (fast path; even works with no title set)
2. Id prefix match if the input starts with `sess-` and matches exactly one session
3. Exact title match (case-insensitive) → unambiguous
4. Substring title match → unambiguous

Errors on ambiguous matches (multiple titles match) with a message naming the count. Validation-failing inputs (e.g. with `..`) are treated as "no exact match" so they fall through to title search rather than erroring — but never reach the filesystem.

### `list()` — sidebar feed

Iterates every `.jsonl` in the dir, calls `Session::load_from` on each, builds `SessionMeta`, sorts by `updated_at` desc. Errors from individual files are silently skipped (`if let Ok(s) = ...`).

**Known limitation (BUG M3 deferred from M6.19):** loads every file fully even though only header + last_timestamp + count + title is used. For projects with hundreds of sessions, sidebar refresh reads + parses MB of JSONL. Future fix: `Session::load_meta_from` that streams just the metadata.

### `latest()`

`list().first()` then `load(id)` — loads the file twice (once during list iteration, once for the actual load). Wasted work but bounded.

---

## 5. `load_from` — replaying the JSONL

The reader walks the file line by line. Per-line behavior (M6.19 BUG H1 — skip-with-warning instead of fail-whole-load):

```
for line in reader.lines():
  - utf8 error               → log + skip + continue
  - empty line               → continue
  - JSON parse error          → log + skip + continue
  - kind == "header"         → header = Some(...)  (last in file wins)
  - kind == "rename"         → title = ...; bump last_timestamp
  - kind == "plan_snapshot"  → plan = ...           (NOT bumping last_timestamp — M6.16.1)
  - kind == "compaction"     → messages.clear(); fill from checkpoint; bump last_timestamp
  - kind ∈ {user/assistant/system} → messages.push; bump last_timestamp
  - any other kind           → log + skip + continue
```

If `header` is missing after the loop, the **salvage path** (M6.14) infers:
- `id = file_stem` (filename without `.jsonl`)
- `model = "unknown"`
- `cwd = ""`
- `created_at = file mtime`

So even a totally-corrupt JSONL surfaces in the sidebar as a placeholder the user can delete.

`updated_at` = `last_timestamp` if any real activity event was found, else `header.created_at`. Skipped lines log to stderr (yellow ANSI); a final summary `[session] <path>: loaded with N corrupt line(s) skipped` fires when N > 0.

### Why per-line skip matters

Pre-M6.19 a single corrupt line failed the whole `load_from` → `SessionStore::list()` silently caught the err → session disappeared from sidebar. Triggers in the wild: disk full mid-`writeln!`, `kill -9` mid-write, cross-process race (CLI + GUI clients in the same project), invalid UTF-8 bytes from external editors.

The user had no surface to debug. The skip-with-warning fix preserves every recoverable message + logs the skip so it's visible when something goes wrong.

---

## 6. Worker integration

`WorkerState` holds the active session by value:

```rust
pub struct WorkerState {
    pub session: Session,
    pub session_store: Option<SessionStore>,
    ...
}
```

The session swap protocol uses `ShellInput` variants:

| Variant | Trigger | Worker action |
|---|---|---|
| `NewSession` | `/new` slash | `save_history` (flush old) → `clear_history` → `Session::new` → `write_header_if_missing` → `plan_state::clear` → `HistoryReplaced(empty)` + `SessionListRefresh` |
| `LoadSession(id)` | `/load <id>` slash, or `session_load` IPC from GUI sidebar click | Auto-switch model if needed (with M6.19 BUG M1 rollback on failure), `set_history(loaded.messages)`, repoint `plan_persist_path`, `restore_from_session(loaded.plan)`, `reset_step_attempts_external`, broadcast `HistoryReplaced` + `SessionListRefresh` |
| `SaveAndQuit` | `/quit` slash | `save_history`, then exit |
| `SessionDeletedExternal { id }` (M6.19) | After successful `session_delete` IPC | If `id == state.session.id`: `save_history` (no-op, the file is gone) → mint new → header → broadcast + `SlashOutput("active session was deleted; minted a fresh session")`. No-op otherwise. |
| `SessionRenamedExternal { id, title }` (M6.19) | After successful `session_rename` IPC | If `id == state.session.id`: update `state.session.title` in-place. No-op otherwise. |
| `ChangeCwd(new_cwd)` | GUI workspace switch | Rebuild `state.session_store` from new `default_path()`; reset plan_state; mint new session; broadcast `SessionListRefresh` (M6.14) |

The worker loop is single-threaded so all sessions writes are serialized within-process. Cross-process is the only concurrent-write surface.

### Auto-switch on `/load`

If the loaded session was recorded with a different provider (`ProviderKind::detect`), the worker auto-switches the model via `rebuild_agent(false)` so the wire format matches what's persisted. Refuses if the target provider has no API key configured (would just hard-error on the next turn). Persists the switch to `.thclaws/settings.json` so restart lands on the same model. The sidebar provider/model display refreshes immediately via `ProviderUpdate`.

M6.19 BUG M1 added `std::mem::replace` rollback: `prev_model = std::mem::replace(&mut state.config.model, loaded.model.clone())` so a failed `rebuild_agent` restores the config to match the still-active agent. Pre-fix the in-memory state would be inconsistent (config.model = new, agent = old); restart would silently lose the swap because settings.json was only written after `rebuild_agent` succeeded.

### Plan state restore

`plan_state::restore_from_session(state.session.plan.clone())` repopulates the in-memory plan_state. The broadcaster (set up at worker spawn) fires on every plan mutation, including this restore — but as noted in §2 (plan_snapshot event), restore-triggered snapshots get filtered out of the sort-recency calc so the just-loaded session doesn't jump to the top of the sidebar.

`plan_state::reset_step_attempts_external()` resets the per-step retry counter (M6.9 BUG E1) — the counter is process-global; without reset, a loaded session with overlapping step ids would inherit the previous session's attempt counts.

---

## 7. Plan-state broadcaster + session JSONL

At worker spawn (`shared_session.rs:625-635`):

```rust
let plan_persist_path: Arc<Mutex<Option<PathBuf>>> = Arc::new(Mutex::new(None));
{
    let plan_tx = events_tx.clone();
    let path_arc = plan_persist_path.clone();
    crate::tools::plan_state::set_broadcaster(move |plan_opt| {
        let _ = plan_tx.send(ViewEvent::PlanUpdate(plan_opt.clone()));
        if let Ok(g) = path_arc.lock() {
            if let Some(p) = g.as_ref() {
                let _ = crate::session::append_plan_snapshot(p, plan_opt.as_ref());
            }
        }
    });
}
```

The broadcaster:
1. Pushes `ViewEvent::PlanUpdate` so the GUI sidebar redraws
2. Appends a `plan_snapshot` event to the JSONL of whatever path `plan_persist_path` currently points at

`plan_persist_path` is updated:
- At spawn (initial session)
- On `NewSession`
- On `LoadSession`
- On `ChangeCwd` (model swap branch only)
- On `ReloadConfig` (model swap branch only)

Critical ordering at every site: `Session::write_header_if_missing(&path)` is called BEFORE `*g = Some(path)` (M6.14 BUG: pre-fix the broadcaster could fire and create the JSONL without a header, then `Session::append_to` would see `path.exists() == true` and skip its own header write, and the headerless file would silently disappear from `SessionStore::list`).

---

## 8. Compaction integration

`/compact` triggers `compaction::compact_with_summary` which:
1. Calls the provider to summarize the older messages into a compact text block
2. Returns a new `Vec<Message>` (summary + recent messages)
3. The worker calls `state.session.append_compaction_to(&path, &compacted)` which:
   - Writes a `compaction` event with the new message list
   - Sets `state.session.messages = compacted.to_vec()`
   - Resets `last_saved_count = compacted.len()` so subsequent `append_to` calls only emit new turns

The original message events stay on disk before the compaction line. On `load_from`, when the reader hits `compaction`, it clears `messages` and refills from the checkpoint. Subsequent message events in the file append after the checkpoint.

This gives both **forward correctness** (next load shows the compacted view) and **audit trail** (the original events are inspectable on disk).

---

## 9. Cross-process concurrency

The worker is single-threaded per process. Within-process, all session writes are serialized.

Cross-process (e.g. CLI + GUI clients open against the same project) is **not protected by file locking**. POSIX `O_APPEND` provides per-write atomicity for writes ≤ PIPE_BUF (~4 KB), which `writeln!` can exceed for tool_use lines with large content payloads.

If two writers race, lines can interleave and corrupt one or both. With M6.19 BUG H1's per-line skip-with-warning fix, the practical impact is "one corrupt line skipped + warning to stderr" rather than "session disappears from sidebar." So the practical pain is much lower than it sounds.

**BUG M4 deferred:** add `flock(LOCK_EX)` (Unix) / `LockFile` (Windows). Documented as known limitation rather than fixed — H1's skip-with-warning is the practical mitigation.

---

## 10. ID generation

```rust
fn generate_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("sess-{nanos:x}")
}
```

Nanosecond resolution → naturally chronologically sortable. Within-process collision is astronomically rare; two `Session::new` calls in the same nanosecond would need an NTP backward jump or extremely-rapid back-to-back calls. Not eliminated theoretically (BUG L4 deferred); not observed in practice.

`now_secs` for timestamps:

```rust
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
```

Saturates to 0 on `duration_since` failure (UNIX_EPOCH is always in the past on a real system, so this never fires in practice).

---

## 11. Code organization

```
crates/core/src/
├── session.rs                            ── ~1200 LOC, the persistence layer
│   ├── SessionHeader / MessageEvent / RenameEvent /
│   │   PlanSnapshotEvent / CompactionEvent           (wire types)
│   ├── Session                                       (id, created_at, updated_at, model, cwd,
│   │                                                  messages, title, last_saved_count, plan)
│   ├── Session::new                                  (mint with generate_id + now_secs)
│   ├── Session::sync                                 (replace messages + bump updated_at)
│   ├── Session::write_header_if_missing              (idempotent; M6.14 race-fix)
│   ├── Session::append_to                            (per-turn save; appends MessageEvent rows)
│   ├── Session::append_rename_to                     (RenameEvent + L1+L5 title sanitization)
│   ├── Session::append_compaction_to                 (CompactionEvent checkpoint)
│   ├── Session::load_from                            (replay JSONL with M6.19 H1 per-line skip)
│   ├── append_plan_snapshot                          (free fn for the broadcaster closure)
│   ├── SessionStore                                  (root: PathBuf)
│   ├── SessionStore::default_path                    (<cwd>/.thclaws/sessions)
│   ├── SessionStore::validate_id                     (no `..`, slashes, control chars, abs paths)
│   ├── SessionStore::save / load / list /
│   │                  resolve_id / load_by_name_or_id /
│   │                  latest / rename / delete       (CRUD surface)
│   ├── generate_id (nanosec-based)
│   └── tests                                         (~25 unit tests)
│
├── shared_session.rs
│   ├── WorkerState.session + session_store           (worker holds active session)
│   ├── ShellInput::NewSession / LoadSession /
│   │              SessionDeletedExternal /
│   │              SessionRenamedExternal /
│   │              ChangeCwd                           (session-mutation channel)
│   ├── save_history                                  (call agent.history_snapshot → session.sync → store.save)
│   ├── build_session_list                            (worker-side sidebar payload with current_id)
│   ├── plan_persist_path + broadcaster setup         (drives PlanSnapshotEvent writes)
│   └── per-handler logic for each ShellInput variant
│
├── gui.rs
│   ├── session_load / session_rename / session_delete IPC handlers
│   ├── build_session_list                            (same payload, no current_id;
│   │                                                  used by main-thread refreshes
│   │                                                  like config_poll)
│   ├── ShellInput::SessionDeletedExternal /
│   │              SessionRenamedExternal             (sent by IPC handlers after on-disk op)
│   └── pty_spawn → SendInitialState → initial sessions list
│
├── repl.rs
│   ├── /sessions handler                             (renders SessionStore::list)
│   ├── /save / /load <id> / /resume [id] handlers
│   ├── /rename "..." handler
│   └── /fork handler                                 (deep-copy current session into new id)
│
├── compaction.rs
│   └── compact_with_summary → session.append_compaction_to
│
└── tools/plan_state.rs
    └── set_broadcaster / restore_from_session /
        clear / reset_step_attempts_external          (broadcaster fires append_plan_snapshot)
```

---

## 12. Testing

`session::tests` ships ~25 unit tests:

**Roundtrip + persistence:**
- `new_session_has_fresh_timestamps_and_unique_id`
- `save_and_load_roundtrip`
- `append_only_adds_new_messages`
- `jsonl_format_has_correct_line_structure`
- `save_creates_parent_directories`
- `sync_bumps_updated_at_and_replaces_messages`

**Listing + sorting:**
- `list_returns_empty_when_store_missing`
- `list_sorts_newest_first`
- `latest_returns_most_recent_session`

**Resolution:**
- `resolve_id_prefers_exact_id_then_title`
- `resolve_id_errors_on_ambiguous_title`

**Robustness (M6.14, M6.19):**
- `load_skips_malformed_lines_and_keeps_recoverable_session` (M6.19 BUG H1)
- `load_salvages_pure_garbage_via_filename` (M6.19 H1 + M6.14 lenient header)
- `load_errors_on_missing_file`
- `load_salvages_headerless_files` (M6.14 lenient header)
- `write_header_if_missing_writes_on_empty_file` (M6.14 race fix)
- `write_header_if_missing_idempotent_on_populated_file` (M6.14)
- `plan_snapshot_does_not_bump_updated_at` (M6.16.1)

**Renames + sanitization:**
- `rename_appends_event_and_persists`
- `rename_strips_control_characters_from_title` (M6.19 BUG L1+L5)
- `rename_errors_on_unknown_session`

**Compaction:**
- `compaction_checkpoint_replays_on_load`

**Validation:**
- `delete_rejects_traversal_ids`
- `delete_is_idempotent_for_missing_files`

The IPC integration paths (`SessionDeletedExternal` / `SessionRenamedExternal`, the M6.19 BUG M1 LoadSession rollback) are GUI-flow / CLI-flow shape, covered by manual verification rather than unit tests since they require a fully-wired `WorkerState` fixture.

---

## 13. Migration / known limitations

### M6.14 fixes (`dev-log/132`)

- **Skill `parse_skill` absolute path** — same family as the session-write race; sessions also got the matching `write_header_if_missing` to prevent the broadcaster from creating files headerless.
- **Lenient salvage path in `load_from`** — files with no header still surface (id from filename, model = "unknown").
- **`ChangeCwd` rebuilds `session_store`** — pre-fix saves landed in the launch project's session dir after a workspace switch.

### M6.16.1 fix (`dev-log/134`)

- **`plan_snapshot` doesn't bump `updated_at`** — clicking a session no longer jumps it to the top of the sidebar.

### M6.19 fixes (`dev-log/137`)

| # | Severity | What | Where |
|---|---|---|---|
| H1 | HIGH | Single malformed line killed whole `load_from` → session disappeared from sidebar. | `session.rs::load_from` (per-line skip + stderr warning + summary count) |
| M1 | MED | `LoadSession` left config / agent inconsistent if `rebuild_agent` failed mid-switch. | `shared_session.rs` (mem::replace + rollback) |
| M2 | MED | `session_delete` of current session resurrected on next save. `session_rename` of current didn't sync in-memory title. | `gui.rs` IPC + new `ShellInput::SessionDeletedExternal` / `SessionRenamedExternal` worker handlers |
| L1+L5 | LOW | Title accepted control chars (`\n`/`\t`/`\x01`) that would break sidebar layout if rendered raw. | `session.rs::append_rename_to` (sanitize at write) |

### Deferred (still)

- **BUG M3 — `list()` loads every session file fully.** Sidebar perf concern for projects with hundreds of sessions. Future fix: streaming `Session::load_meta_from(path)` that stops parsing message bodies after extracting metadata.
- **BUG M4 — Cross-process writes can interleave.** No file locking. With H1's skip-with-warning the impact dropped from "session disappears" to "one corrupt line skipped + warning"; documented as known limitation rather than fixed.
- **BUG L3 — `now_secs` saturates to 0** on `duration_since` failure. Cosmetic; UNIX_EPOCH can't be in the future on a real system.
- **BUG L4 — `generate_id` no per-process counter.** Astronomically rare collision; acceptable.

### Sprint chronology

| Sprint | Dev-log | What shipped (sessions-relevant) |
|---|---|---|
| Phase 5 | (initial) | `Session`, `SessionStore`, append-only JSONL, `SessionHeader` + `MessageEvent` |
| Phase 11 (plan mode M1) | `~115` | `PlanSnapshotEvent` + broadcaster integration + restore-on-load |
| Plan mode M5 | `~120` | `RenameEvent` + sidebar rename UX |
| Compaction | `~125` | `CompactionEvent` checkpoint + replay logic |
| M6.9 (E1) | `~129` | `reset_step_attempts_external` on session swap (cross-session step-id collision) |
| M6.14 | `132` | `write_header_if_missing` race fix + lenient salvage + `ChangeCwd` rebuild |
| M6.16.1 | `134` | `plan_snapshot` excluded from sort recency |
| M6.19 | `137` | THIS sprint — H1 + M1 + M2 + L1 + L5 |
