# `/dream` — built-in KMS consolidation

`/dream` is a slash command that dispatches a built-in `dream` agent as a side channel to consolidate the project's KMS by mining recent sessions. Where [`side-channel.md`](side-channel.md) covers the **infrastructure** for user-driven concurrent subagents, this doc covers the **first-class operation** built on top of it: a thClaws-shipped AgentDef that performs deduplication, insight surfacing, and audit-trail authoring across active KMS instances.

The feature is intentionally narrow: it composes existing primitives (side-channel spawn + AgentDef registry + KMS tools) and adds two new pieces — an embedded AgentDef compiled into the binary, and a `KmsDelete` tool — to enable real consolidation work.

| | `/agent NAME PROMPT` (general) | `/dream [FOCUS]` (built-in) |
|---|---|---|
| **Trigger** | User picks any AgentDef by name | User invokes the operation by name |
| **AgentDef source** | `~/.claude/agents/`, `.thclaws/agents/`, plugins | Embedded `default_prompts/dream.md` (override-able) |
| **Setup cost** | User authors `.md` first | Zero — works on any project with KMS attached |
| **System prompt** | User-controlled | thClaws-controlled (consistent across users) |
| **Mental model** | "Spawn agent X" | "Run the dream operation" |

Both spawn through the same `spawn_side_channel` pipeline ([side-channel.md §2](side-channel.md)). `/dream` is `/agent dream <prompt>` underneath, but reaches the user as a first-class command because the underlying agent ships with the binary and the dispatch chooses a sensible default prompt when none is given.

This doc covers: the embedded AgentDef seeding flow + override semantics, the `KmsDelete` tool added for consolidation, the slash-command surface, the dream agent's four-pass operating procedure, and the testing surface.

**Source modules:**
- [`crates/core/src/default_prompts/dream.md`](../thclaws/crates/core/src/default_prompts/dream.md) — the embedded AgentDef. Markdown frontmatter (`name`, `description`, `model`, `tools`, `permissionMode`, `maxTurns`, `color`) plus the four-pass system prompt body. Compiled into the binary via `include_str!`.
- [`crates/core/src/agent_defs.rs`](../thclaws/crates/core/src/agent_defs.rs) — `AgentDefsConfig::seed_builtins()` runs at the start of `load_with_extra` so disk-loaded agent defs (legacy JSON + user/project markdown dirs) override built-ins by name. `parse_agent_md_str` extracted from `parse_agent_md` so the same parser handles both file paths and embedded strings.
- [`crates/core/src/kms.rs`](../thclaws/crates/core/src/kms.rs) — `delete_page()` helper alongside `write_page` / `append_to_page`. Validates page name via `writable_page_path` (same path-safety carve-out as the write helpers), removes the file, prunes the matching bullet from `index.md` via `remove_index_bullet()`, and appends `## [date] deleted | <stem>` to `log.md` via `append_log_header`.
- [`crates/core/src/tools/kms.rs`](../thclaws/crates/core/src/tools/kms.rs) — `KmsDeleteTool`: `requires_approval = true`, takes `{kms, page}`, calls `kms::delete_page`. Sits alongside `KmsWriteTool` and `KmsAppendTool` as the third mutation tool.
- [`crates/core/src/tools/mod.rs`](../thclaws/crates/core/src/tools/mod.rs) — re-export `KmsDeleteTool`.
- [`crates/core/src/repl.rs`](../thclaws/crates/core/src/repl.rs) — `SlashCommand::Dream { focus: String }` variant + the bare `"dream"` arm in `parse_slash` (no subcommand routing — focus is everything after `/dream `). REPL dispatch prints a GUI-only hint. `/help` text gains a `/dream [FOCUS]` line. CLI registration of `KmsDeleteTool` matches the existing KmsRead/Search/Write/Append registrations.
- [`crates/core/src/shared_session.rs`](../thclaws/crates/core/src/shared_session.rs) — GUI registration of `KmsDeleteTool` alongside the rest of the KMS write surface.
- [`crates/core/src/shell_dispatch.rs`](../thclaws/crates/core/src/shell_dispatch.rs) — `SlashCommand::Dream { focus }` arm. Empty focus falls back to a hard-coded "consolidate everything" prompt; non-empty focus is passed verbatim. Dispatches via `crate::side_channel::spawn_side_channel("dream", prompt, ...)` reusing `state.agent_factory` and `state.agent_defs` set up at worker init.

**Cross-references:**
- [`side-channel.md`](side-channel.md) — the spawn pipeline, registry singleton, `AgentOrigin::SideChannel` attribution, and `ViewEvent::SideChannel*` plumbing that `/dream` rides on top of
- [`kms.md`](kms.md) — KMS architecture, page layout, sandbox carve-out, the `KmsRead` / `KmsSearch` / `KmsWrite` / `KmsAppend` tools that `/dream` uses (plus the new `KmsDelete`)
- [`subagent.md`](subagent.md) — the `AgentDef` registry that `seed_builtins` extends, and the `ProductionAgentFactory` build pipeline that `spawn_side_channel` drives

---

## 1. Embedded AgentDef seeding

The dream agent is shipped inside the binary as a markdown file with YAML frontmatter, then loaded at session startup. The seam point is `AgentDefsConfig::load_with_extra`:

```
0. seed_builtins()                          ← built-ins (lowest priority)
1. legacy ~/.config/thclaws/agents.json
2. agent_dirs() — user/project .md dirs     ← overrides built-ins by name
3. plugin-contributed dirs (no_clobber)     ← can't override anything
```

`seed_builtins` is a small, table-driven loop:

```rust
fn seed_builtins(&mut self) {
    const BUILTINS: &[(&str, &str)] = &[("dream", include_str!("default_prompts/dream.md"))];
    for (fallback_name, raw) in BUILTINS {
        if let Some(agent) = Self::parse_agent_md_str(raw, fallback_name) {
            self.agents.push(agent);
        }
    }
}
```

The first `&str` is the fallback agent name used if the embedded markdown's frontmatter has no `name:` key (it does, so this is defensive). The second is the embedded source compiled in via `include_str!`.

**Override semantics** are inherited from the existing markdown loader. `load_md_dir` finds an existing `dream` entry in `self.agents` and replaces it in place — so a user's `.thclaws/agents/dream.md` wins over the built-in. The plugin-contributed loader (`load_md_dir_no_clobber`) keeps existing entries, which means a plugin can't accidentally shadow either the user's dream override or the built-in.

**Parser refactor.** Before `/dream`, `parse_agent_md` did its own file-stem extraction inline. To support an in-memory parse path, it was split:

```rust
fn parse_agent_md(path: &Path) -> Option<AgentDef> {
    let raw = std::fs::read_to_string(path).ok()?;
    let fallback = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
    Self::parse_agent_md_str(&raw, fallback)
}

fn parse_agent_md_str(raw: &str, fallback_name: &str) -> Option<AgentDef> {
    let (frontmatter, body) = crate::memory::parse_frontmatter(raw);
    let name = frontmatter.get("name").cloned().unwrap_or_else(|| fallback_name.to_string());
    // ...
}
```

The disk path is now a thin wrapper. Behaviour is identical to before for on-disk loads.

---

## 2. The dream AgentDef

[`default_prompts/dream.md`](../thclaws/crates/core/src/default_prompts/dream.md) is what gets compiled in. The frontmatter:

```yaml
name: dream
description: Consolidate the project's KMS by mining recent sessions, deduping pages, and surfacing insights
tools: KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete, KmsCreate, Read, Glob, Grep, TodoWrite, SessionRename
permissionMode: auto
maxTurns: 120
color: purple
```

Notable choices:

- **No `model:` field.** The dream agent uses the session's active model. Pre-fix the agent def hardcoded `claude-opus-4-7`, which routed through the session's CURRENT provider — so users on OpenAI hit `404: model claude-opus-4-7 does not exist` even with an Anthropic key set. Long-context judgment models (Opus / GPT-4.1 / Sonnet 4.6) work best for this task; pick one before invoking `/dream` if you care. Override per-project via `model:` in `.thclaws/agents/dream.md`.
- **Tool whitelist is tight.** `Read`/`Glob`/`Grep` exist so the agent can mine `.thclaws/sessions/*.jsonl` files; `KmsRead`/`KmsSearch` for survey; `KmsWrite`/`KmsAppend`/`KmsDelete` for mutation; `KmsCreate` for bootstrapping the `dreams` audit KMS; `SessionRename` for the Pass-2 auto-rename; `TodoWrite` for tracking which pass it's on. Notably absent: `Bash`, `Edit`, `Write`, `Memory*`, `Task`, `WebSearch`, `WebFetch`. The dream agent can only modify the KMS + session metadata (titles) — it can't touch project source, can't recurse into more subagents, can't reach the network.
- **`permissionMode: auto`** — the agent's KMS mutations land directly. The user-facing review pattern is `git diff .thclaws/kms/`, not in-modal approval. A user who wants approval-gated dreaming can override the AgentDef.
- **`maxTurns: 120`** — consolidation across multiple KMS + 10 sessions can take many turns. Default is 200; 120 is a comfortable ceiling that still bounds runaway behavior.

### Five-pass operating procedure

Pre-fix the body was a four-pass loop. The current body is a **five-pass** loop with explicit skip-already-dreamed + auto-rename + scoped reconciliation logic:

1. **Survey + skip-already-dreamed.** Read the active KMS list + each `index.md`. `KmsSearch` the dedicated `dreams` KMS for the most recent prior `dream-` summary; extract its "Sessions processed" table. Glob `.thclaws/sessions/*.jsonl` (10 most recent by default, all with `--all`). Skip sessions whose recorded `last_message_at` ≥ current file mtime — no new chat content since prior dream.
2. **Read sessions + auto-rename.** Read each surviving session JSONL. If the session's title is missing or matches the auto-generated `sess-<8hex>` shape, propose a meaningful one-line title (≤ 70 chars) and call `SessionRename`. Skip sessions the user already gave a meaningful title to.
3. **Consolidate (writes to ACTIVE KMSes only).** For each insight: pick the right active KMS (`auth-conventions` ↔ project-knowledge, personal preferences ↔ personal-notes, etc.); `KmsSearch(kms: "<active-kms>", pattern: "...")` first; prefer `KmsAppend` over creating a new page; merge overlapping pages via `KmsWrite` + `KmsDelete`. **The `kms:` argument MUST be one of the active KMSes from the system prompt — never `dreams`.** If no active KMS is attached, Pass 3 is skipped entirely (noted in the summary) and the agent proceeds to Pass 4. Track which pages were written/appended/deleted (all of which live in active KMSes) — Pass 3b needs that list.
4. **Pass 3b — Targeted reconciliation (still in active KMSes).** For every page touched in Pass 3 only: `KmsRead` the full page, look for internal contradictions (two facts disagreeing, stale timestamps, "we use X" vs "we migrated away from X"), and `KmsWrite` a rewrite with a `## History` section preserving the old stance + reason for change. All Pass 3-touched pages are in active KMSes, so Pass 3b never reads or writes `dreams`. Do NOT touch unmodified pages — that's `/kms reconcile`'s job (full-vault sweep). Targeted scope keeps the diff cohesive for review.
5. **Summarize (writes the SINGLE summary page to `dreams` ONLY).** Always end the run by writing one summary page in the **`dreams`** KMS (NOT any active project / user KMS) at `dream-YYYY-MM-DD.md`. The summary carries a Sessions-processed table (load-bearing — next dream's Pass 1 reads it), plus pages added / updated / reconciled / deleted lists, sessions renamed, insights surfaced, and skipped reasons. The summary is the ONLY page that ever lands in `dreams`; knowledge pages from Pass 3 live in active KMSes.

### Two-way targeting invariant

The prompt enforces a strict two-way invariant via repetition + an explicit anti-pattern section:

- Pass 3 + Pass 3b: `kms:` MUST be an active KMS. NEVER `dreams`.
- Pass 4: `kms:` MUST be `dreams`. NEVER an active KMS.
- Pass 1 reads `dreams` (looking for prior summaries) and reads active KMSes' indices. Reads are exempt from the write-direction rule.

Historical failure mode (pre-fix): the prompt's Pass 3 wording said "the relevant KMS", which the agent interpreted loosely + saw `dreams` mentioned repeatedly elsewhere in the prompt → defaulted to filing knowledge pages in `dreams`. The current prompt adds explicit `kms: "<active-kms-name>"` placeholders in every Pass 3 example, a "Common mistakes to avoid" subsection enumerating the failure patterns (knowledge to `dreams`, summary to active KMS, cross-vault merge), and a Discipline rule stating the invariant in both directions.

### `--all` flag

`shell_dispatch.rs`'s Dream arm encodes the `--all` flag into the user message as `[scope: ALL_SESSIONS — process every .jsonl file under .thclaws/sessions/, not just the 10 most recent. Widen Pass 3b targeted reconciliation to every page Pass 3 touched.]`. The dream prompt reads this scope hint in Pass 1 to widen the glob; with no hint it defaults to "10 most recent".

The active KMS list reaches the dream agent through the same `kms::system_prompt_section` injection as any other agent — it sees `## Knowledge bases` listing the attached KMS by name, which it uses as the authoritative list to operate on.

### Dispatch auto-creates the `dreams` KMS

`shell_dispatch.rs`'s Dream arm calls `kms::create("dreams", KmsScope::Project)` before `spawn_side_channel` (idempotent). This is layer 1 of defense-in-depth — the agent itself also calls `KmsCreate({name: "dreams", scope: "project"})` as Pass 5 Step 0 (layer 2). Either layer alone is sufficient; both together survive stale binaries, filesystem races between dispatch and agent execution, etc.

---

## 3. The `KmsDelete` tool

`KmsDelete` is the third mutation tool in the KMS surface, added specifically for `/dream` (no other call site uses it yet). Same shape and approval posture as `KmsWrite` / `KmsAppend`:

```rust
fn input_schema(&self) -> Value {
    json!({
        "type": "object",
        "properties": {
            "kms":  {"type": "string"},
            "page": {"type": "string"}
        },
        "required": ["kms", "page"]
    })
}

fn requires_approval(&self, _input: &Value) -> bool { true }

async fn call(&self, input: Value) -> Result<String> {
    let kref = crate::kms::resolve(req_str(&input, "kms")?).ok_or(...)?;
    let path = crate::kms::delete_page(&kref, req_str(&input, "page")?)?;
    Ok(format!("deleted {}", path.display()))
}
```

`kms::delete_page` is the work-doing helper:

```rust
pub fn delete_page(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    let path = writable_page_path(kref, page_name)?;       // path-safety + reserved-name check
    if !path.exists() { return Err(Error::Tool("page not found: ...".into())); }
    let stem = path.file_stem()...;
    std::fs::remove_file(&path)...;
    remove_index_bullet(kref, &stem)?;
    append_log_header(kref, "deleted", &stem)?;
    Ok(path)
}
```

Path safety reuses `writable_page_path` — the same validator that `write_page` and `append_to_page` use. That means `KmsDelete` can't traverse outside the KMS pages dir, can't delete the reserved `index` / `log` / `SCHEMA` pages, and can't be tricked by `..` segments or absolute paths.

`remove_index_bullet` strips any line in `index.md` containing `(pages/<stem>.md)` and rewrites the file. `append_log_header` adds `## [YYYY-MM-DD] deleted | <stem>` to `log.md` so the `git diff .thclaws/kms/` review surface shows both the page removal and the log entry side-by-side.

Registration sites for `KmsDeleteTool`:
- [`repl.rs`](../thclaws/crates/core/src/repl.rs) — CLI `start_session` (gated on `!config.kms_active.is_empty()`) and the print-mode session builder
- [`shared_session.rs`](../thclaws/crates/core/src/shared_session.rs) — GUI worker init (same gate)
- [`shell_dispatch.rs`](../thclaws/crates/core/src/shell_dispatch.rs) — KMS attach (`/kms use NAME`) handler that hot-registers the KMS tool family when the first KMS is attached mid-session

The four registration sites mirror the existing `KmsWrite`/`KmsAppend` pattern; missing one would cause "tool not found" errors in that surface.

---

## 4. Slash-command surface

`SlashCommand::Dream { focus: String }` is added to the existing enum in [`repl.rs`](../thclaws/crates/core/src/repl.rs). The parser is one line:

```rust
"dream" => SlashCommand::Dream { focus: args.to_string() },
```

No subcommand routing — `/dream` and `/dream auth` and `/dream consolidate marketplace KMS` all parse to the same variant with progressively richer focus strings. Bare `/dream` produces `Dream { focus: "" }`, which is valid (the dispatch fills in a default prompt).

REPL dispatch prints a GUI-only hint; the CLI doesn't have a broadcast surface to fan side-channel events through. GUI dispatch in [`shell_dispatch.rs`](../thclaws/crates/core/src/shell_dispatch.rs):

```rust
SlashCommand::Dream { focus } => {
    let prompt = if focus.trim().is_empty() {
        "Consolidate the active KMS by mining recent sessions. \
         Follow your standard four-pass procedure.".to_string()
    } else {
        focus
    };
    match crate::side_channel::spawn_side_channel(
        "dream".to_string(), prompt,
        state.agent_factory.clone(), state.agent_defs.clone(),
        events_tx.clone(),
    ).await {
        Ok(id) => emit(events_tx, format!("✓ dreaming (id: {id})")),
        Err(e) => emit(events_tx, format!("/dream: {e}")),
    }
}
```

The `state.agent_defs` here is the `AgentDefsConfig` populated at worker init via `load_with_extra(&plugin_agent_dirs)` — which means the embedded `dream` AgentDef is already in the registry. `spawn_side_channel` resolves the agent by name and uses the standard side-channel pipeline (independent `CancelToken::new()`, `tokio::spawn` of the `Agent::run_turn` loop, `AgentOrigin::SideChannel { id, agent_name: "dream" }` tagged on every `ApprovalRequest`, five `ViewEvent::SideChannel*` emissions through `events_tx`).

Lifecycle for `/dream` from there is identical to any other side-channel agent — see [`side-channel.md` §2](side-channel.md). Cancel via `/agent cancel <id>`; list via `/agents`.

---

## 5. Dispatch flow

End-to-end, what happens when a user types `/dream auth`:

```
1. Frontend ChatView captures the line and sends it as a chat_input IPC envelope.
2. Worker dispatch parses it via parse_slash → SlashCommand::Dream { focus: "auth" }.
3. shell_dispatch arm:
     - non-empty focus → prompt = "auth"
     - spawn_side_channel("dream", "auth", state.agent_factory.clone(),
                          state.agent_defs.clone(), events_tx.clone())
4. spawn_side_channel:
     - Mints a new SideChannelId (random "side-XXXXXX")
     - Builds the Agent via agent_factory.build(...) — uses the embedded
       dream AgentDef from agent_defs (registered by seed_builtins)
     - Calls .with_origin(AgentOrigin::SideChannel { id, agent_name: "dream" })
     - Mints an independent CancelToken::new() (NOT a child of main)
     - Stores SideChannelHandle in the process-wide registry
     - tokio::spawn the agent's run_turn loop with prompt="auth"
     - Returns the id immediately
5. Worker emits "✓ dreaming (id: side-XXXXXX)" through events_tx.
6. Spawned task streams agent events as ViewEvent::SideChannelStart →
   SideChannelTextDelta* → SideChannelToolCall* → SideChannelDone
   (or SideChannelError on cancel/failure).
7. event_render fans those into chat_side_channel_* IPC envelopes.
8. ChatView renders a side-channel bubble keyed by id with the running
   stream and final result.
```

The four-pass dream procedure runs entirely inside step 6 — the agent uses its tool whitelist to read sessions, search KMS, write/append/delete pages, and finally write the audit-trail summary page.

---

## 6. Test surface

Tests added with `/dream`:

| File | Test | Purpose |
|---|---|---|
| `tools/kms.rs` | `delete_removes_page_and_index_bullet` | End-to-end: write a page, verify it + index entry exist, delete, verify both gone + log entry appended |
| `tools/kms.rs` | `delete_missing_page_errors` | `KmsDelete` on nonexistent page returns `Err` (not silent success) |
| `tools/kms.rs` | `delete_rejects_reserved_names` | `index`, `log` (and via the same path-safety chain, `SCHEMA`) cannot be deleted through the tool |
| `tools/kms.rs` | `write_and_append_require_approval` (extended) | `KmsDeleteTool::requires_approval` returns `true` |
| `agent_defs.rs` | `seed_builtins_includes_dream` | Built-in dream agent is seeded with name + non-empty instructions + `KmsDelete` in tool list |
| `agent_defs.rs` | `user_dream_md_overrides_builtin` | A `.thclaws/agents/dream.md` on disk replaces the built-in's instructions |
| `repl.rs` | `parse_slash_dream_with_focus` | `/dream <text>` → `SlashCommand::Dream { focus }` |
| `repl.rs` | `parse_slash_dream_bare` | `/dream` → `SlashCommand::Dream { focus: "" }` (valid, dispatch fills default) |

The full GUI test suite (`cargo test --features gui`) was 957 tests passing pre-feature; new tests are additive.

What's **not** unit-tested:
- The dispatch path (`shell_dispatch::dispatch_chat` for `SlashCommand::Dream`) — it's a thin shim over `spawn_side_channel`, which has its own existing test surface in `side_channel.rs`. End-to-end dream behavior is validated by manual GUI testing against a real KMS + sessions.
- The dream system prompt's actual consolidation behavior — that's prompt engineering, not unit-testable. Verification comes from running `/dream` in a project, reviewing `git diff .thclaws/kms/`, and tightening the prompt if the agent over- or under-consolidates.

---

## 7. Known gaps / future work

- **Session mining is bounded at 10 most-recent files.** A project with hundreds of sessions per week may miss insights from older sessions. Future: `/dream --since 2026-04-01` or sliding window driven by KMS frontmatter `last_consolidated`.
- **No KmsList tool.** The dream agent enumerates active KMS via the system-prompt section and pages via `index.md`. A first-class `KmsList` tool would be more robust if the system-prompt rendering ever changes shape.
- **No "candidate output" mode.** The dream agent edits in place and relies on git diff for review. The Anthropic Dreams pattern (input never modified; output is a new memory_store) would require a tempdir/branch parallel KMS — significantly more work, deferred to v2.
- **Single AgentDef.** Only one built-in agent (`dream`). The `seed_builtins` table is set up to take more (`&[(&str, &str)]`); future built-ins (e.g. `kms-lint`, `session-summarizer`) would slot in here.
- **Daemon-driven scheduled dreams.** `/dream` is user-driven only. A `/schedule add --cron '0 3 * * 0' --dream` shortcut could run weekly dreams via the existing schedule daemon (see [`schedule.md`](schedule.md)). Not implemented.
- **No CLI surface.** `/dream` is GUI-only because it needs the chat surface to render the side bubble. CLI users can run a long-form `Task(agent: "dream", prompt: "...")` but it blocks the parent's turn.

---

## 8. What lives where (source-line index)

| Concern | File | Notable |
|---|---|---|
| Embedded AgentDef | `default_prompts/dream.md` | Compiled via `include_str!` in `agent_defs.rs::seed_builtins` |
| Built-in seeding | `agent_defs.rs::seed_builtins` | Called first in `load_with_extra` so disk overrides win |
| Markdown parser split | `agent_defs.rs::parse_agent_md_str` | Handles both file-stem fallback (disk) and hard-coded fallback (embedded) |
| Override semantics | `agent_defs.rs::load_md_dir` | Existing replace-by-name logic transparently overrides built-ins |
| `KmsDelete` helper | `kms.rs::delete_page` + `remove_index_bullet` | Reuses `writable_page_path` for path safety; appends log entry via `append_log_header` |
| `KmsDelete` tool | `tools/kms.rs::KmsDeleteTool` | `requires_approval = true`; sits beside `KmsWrite`/`KmsAppend` |
| Tool registrations | `repl.rs` (CLI ×2) + `shared_session.rs` (GUI) + `shell_dispatch.rs` (attach hook) | Four sites mirror the existing `KmsWrite`/`KmsAppend` pattern |
| Slash variant | `repl.rs::SlashCommand::Dream` | Single-field `focus: String`, no subcommand routing |
| Parser arm | `repl.rs::parse_slash` | `"dream" => SlashCommand::Dream { focus: args.to_string() }` |
| REPL hint | `repl.rs` (dispatch arm) | "GUI-only" message; CLI has no broadcast surface for side-channel events |
| GUI dispatch | `shell_dispatch.rs` (Dream arm) | Empty focus → default prompt; `spawn_side_channel("dream", ...)` |
| Side-channel pipeline | `side_channel.rs::spawn_side_channel` | Reused as-is; no dream-specific changes |
| `/help` line | `repl.rs::help_text` | `/dream [FOCUS]   Consolidate KMS by mining recent sessions (GUI-only)` |
