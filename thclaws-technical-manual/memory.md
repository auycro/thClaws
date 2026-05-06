# Memory

A directory of markdown files the agent can read AND maintain as long-lived context. Auto-loaded into the system prompt every turn (categorized index + as-many-bodies-as-fit-in-budget); deferred entries become on-demand `MemoryRead` tool calls. The model owns the maintenance burden via `MemoryWrite` / `MemoryAppend` tools (sandbox carve-out) and `MEMORY.md` index is auto-maintained on every write/delete.

This doc covers: directory resolution + scope precedence, on-disk layout, frontmatter convention, system-prompt injection (categorized index + budget cap + tool affordances), the four memory tools (Read/Write/Append + auto-load), `/memory` slash commands (list/read/write/append/edit/delete), security model, sandbox carve-out, and migration from the pre-M6.26 read-only design.

**Source modules:**
- `crates/core/src/memory.rs` — `MemoryStore`, `MemoryEntry`, `MEMORY_*_BYTES`/`_LINES` constants, `default_path` resolution, `system_prompt_section` (categorized + budget-capped), `parse_frontmatter` + `write_frontmatter_map`, `writable_entry_path`, `write_entry` + `append_to_entry` + `delete_entry`, `update_memory_index_bullet` + `remove_memory_index_bullet`, `truncate_for_prompt`, `truncate_index`, `memory_sizes`
- `crates/core/src/tools/memory.rs` — `MemoryReadTool`, `MemoryWriteTool`, `MemoryAppendTool`
- `crates/core/src/shell_dispatch.rs` — `/memory` slash-command handlers (GUI path)
- `crates/core/src/repl.rs` — `SlashCommand::Memory*` enum, `parse_memory_subcommand`, CLI dispatch + editor flow (`build_memory_scaffold`, `spawn_editor_for_memory`)
- `crates/core/src/shared_session.rs` — Memory tools always-on registration at worker boot; system-prompt section appended

**Cross-references:**
- [`built-in-tools.md`](built-in-tools.md) §8 / §10 — `MemoryRead/Write/Append` tool surface
- [`context-composer.md`](context-composer.md) — `MemoryStore::system_prompt_section()` injection point
- [`kms.md`](kms.md) — sibling subsystem; M6.25 / M6.26 share design patterns (frontmatter, sandbox carve-out, categorized index, budget caps)
- [`commands.md`](commands.md) — `/memory` is a built-in slash command (not a `.md` template); `parse_frontmatter` is shared with command-template parsing
- [`skills.md`](skills.md) — same `parse_frontmatter` is reused by skill SKILL.md parsing

---

## 1. Overview

### Concept

Memory is the agent's **long-lived context**: facts about the user, behavioral feedback that should outlive a session, project state, references to external systems. Lives outside any single conversation; loaded into the system prompt every turn. The user can browse and edit; the agent can read and (M6.26+) write.

Pre-M6.26 the agent was a passive reader — entries auto-loaded into the prompt, but writing required hand-editing the `.md` files (and the project-scope path was sandbox-blocked even via `Write`/`Edit`). M6.26 closes that gap: dedicated `MemoryWrite`/`MemoryAppend` tools with a sandbox carve-out, slash commands for direct user authoring, and auto-maintained `MEMORY.md` index so the catalog stays consistent with on-disk entries.

### Lifecycle

```
USER  /memory                    → list current entries (name, type, description)
LLM   reads system-prompt index  → decides whether to MemoryRead a deferred entry
USER  asks "remember X"          → LLM calls MemoryWrite (approved via Ask gate)
                                   → MEMORY.md auto-updates with new bullet
LLM   accumulates an observation → MemoryAppend (frontmatter `updated:` bumps)
USER  /memory delete <name>      → confirm prompt → file + bullet removed
USER  /memory edit <name>        → opens $EDITOR pre-filled → save → write_entry
```

---

## 2. Directory resolution

`MemoryStore::default_path()` ([memory.rs:60-102](thclaws/crates/core/src/memory.rs#L60)) walks 4 candidates in order; returns the **first** one that exists, falls through to a synthetic global path otherwise:

| Order | Path | When picked |
|---|---|---|
| 1 | `<cwd>/.thclaws/memory/` | When `.thclaws/` exists in cwd → **project-scoped** (preferred; M6.26 made writable via tool carve-out) |
| 2 | `~/.claude/projects/<sanitized-cwd>/memory/` | Legacy Claude Code per-project (read-only fallback) |
| 3 | `~/.thclaws/projects/<sanitized-cwd>/memory/` | Legacy thClaws per-project (read-only fallback) |
| 4 | `~/.local/share/thclaws/memory/` | User-global (returned even when missing so first write can create it) |

Sanitized cwd = full path with `/` → `-`, leading `-` stripped. Project scope (#1) wins over user global (#4) when both could resolve — same precedence as project CLAUDE.md vs user CLAUDE.md.

---

## 3. On-disk layout

```
<root>/
├── MEMORY.md           # index: bullets pointing at entries (auto-maintained)
├── user_role.md        # entry with frontmatter + body
├── feedback_testing.md
└── project_state.md
```

`MEMORY.md` is the entrypoint. Free-form markdown by convention; the typical pattern is one bullet per entry:

```markdown
# Memory Index

- [user_role](user_role.md) — User is a senior backend engineer
- [feedback_testing](feedback_testing.md) — Don't mock the database
- [project_state](project_state.md) — Current sprint goals
```

M6.26 auto-maintains this on `write_entry` / `delete_entry` — bullets dedupe (no duplicates on replace) and removed entries lose their bullet. Entries the user adds by hand also benefit, since the auto-update reads existing bullets and only rewrites entries it knows about.

Topic files (one per entry) start with optional YAML-ish frontmatter:

```markdown
---
name: user_role
description: User is a senior backend engineer; new to React
type: user
category: identity
created: 2026-05-03
updated: 2026-05-03
---

Body content. Free markdown — paragraphs, lists, code blocks, links.
```

Reserved stem: `MEMORY` (case-insensitive) — `writable_entry_path` refuses it so `MemoryWrite` can't accidentally clobber the index.

---

## 4. Frontmatter convention

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Display name; auto-stamped to file stem if missing |
| `description` | string | One-line hook used in `MEMORY.md` bullet + system-prompt index |
| `type` | enum-ish | `user` / `feedback` / `project` / `reference` (free-form, unenforced) |
| `category` | string | Drives **categorized index** in system prompt (M6.26 BUG #4) |
| `created` | YYYY-MM-DD | Auto-stamped on first write; **preserved** on replace |
| `updated` | YYYY-MM-DD | Auto-stamped to today on every write/append |

### Parser

`memory::parse_frontmatter(s) -> (HashMap<String, String>, String)` ([memory.rs:496-535](thclaws/crates/core/src/memory.rs)) is intentionally permissive:

- `---` opens on first line, `---` closes on its own line
- `key: value` lines inside the block; trim whitespace; strip surrounding `"` / `'` quotes (M6.26 — round-trip safe with `write_frontmatter_map`)
- Non-`key: value` lines and missing fences both yield `(empty, original)`

Same parser is reused by [`skills.rs`](skills.md) for `SKILL.md` parsing and [`commands.rs`](commands.md) for command-template frontmatter — keeps the parsing surface consistent across markdown subsystems.

`memory::write_frontmatter_map(map, body) -> String` (M6.26) round-trips. Auto-quotes values containing `:`, `#`, leading whitespace, `"`, or `\n`. Sorted keys for deterministic output (matters for prompt-cache stability).

---

## 5. System-prompt injection

`MemoryStore::system_prompt_section()` is called by the worker at boot and `repl.rs::run_repl`/`run_print_mode`:

```rust
if let Some(store) = MemoryStore::default_path().map(MemoryStore::new) {
    if let Some(mem) = store.system_prompt_section() {
        system.push_str("\n\n# Memory\n");
        system.push_str(&mem);
    }
}
```

### Output shape (M6.26)

```markdown
# Memory

## Index
**identity**
- user_role — Senior backend engineer; new to React

**feedback**
- testing — Don't mock the database

**uncategorized**
- legacy_entry — (no description)

## user_role (user)
_Senior backend engineer; new to React_

<body, capped at 80 lines / 8 KB per entry>

## testing (feedback)
_Don't mock the database_

_(body deferred — 12,500 bytes; call `MemoryRead(name: "testing")` to fetch.)_

## Tools
- `MemoryRead(name: "<entry>")` — read full body of a deferred entry
- `MemoryWrite(name: "<entry>", content: "...")` — create or replace an entry
- `MemoryAppend(name: "<entry>", content: "...")` — append to an entry
Entries may carry YAML frontmatter (`name`, `description`, `type`, `category`,
`created`, `updated`). `MEMORY.md` index is auto-maintained on write/delete.
```

### Categorized index (BUG #4)

When at least one entry has frontmatter `category:`, `render_categorized_index` groups bullets under `**<category>**` headers (BTreeMap-sorted). Pages without `category:` go under `**uncategorized**`. **Falls back** to the raw `MEMORY.md` (capped) when no entry has `category:` — backwards compat with pre-M6.26 entries.

### Budget cap (BUG #5)

`MEMORY_TOTAL_INLINE_BYTES = 16_000` is the total byte budget for inlined entry bodies. Inlining order is alphabetical (deterministic). When the budget fills:

- Earlier entries inline fully (each still per-entry-capped at 80 lines / 8 KB by `truncate_for_prompt`)
- Later entries become deferred pointers: `_(body deferred — N bytes; call MemoryRead to fetch.)_`

The model still sees the categorized index (full) AND tool affordances, so it knows which entries exist and how to fetch deferred bodies.

Pre-M6.26 every entry's body was always inlined — at scale (50 entries × 8 KB = 400 KB) this burned unbounded tokens per turn. Post-M6.26 the worst case is bounded at ~16 KB of bodies + a few KB for the categorized index + tool affordances.

### Caps summary

| | Lines | Bytes | Source |
|---|---|---|---|
| `MEMORY.md` index (raw fallback) | 200 | 25 KB | M6.18 BUG M6 |
| Per-entry body | 80 | 8 KB | M6.18 BUG M5 |
| Total inlined bodies | n/a | 16 KB | M6.26 BUG #5 |

When a per-entry or index cap fires, an HTML-comment notice is appended (`<!-- ... truncated: N lines / M bytes → kept first ... -->`) so the model sees the truncation explicitly.

---

## 6. Tool surface (LLM-callable)

Three tools register **always** (not conditional on memory content) so the model can create the first entry on a fresh project:

| Tool | Approval | Purpose |
|---|---|---|
| `MemoryRead` | No | Fetch full body of a deferred entry. Returns frontmatter + body. |
| `MemoryWrite` | **Yes** | Create or replace an entry. `created:` preserved on replace; `updated:` always today. Auto-updates `MEMORY.md` bullet. |
| `MemoryAppend` | **Yes** | Append a chunk. Bumps `updated:`. Creates with bare body if missing. |

### Sandbox carve-out (BUG #1)

`MemoryWrite` and `MemoryAppend` deliberately bypass `Sandbox::check_write`. Rationale: project-scope memory lives at `.thclaws/memory/...` which the sandbox blocks (the `.thclaws/` reserved-dir rule). User-global memory at `~/.local/share/thclaws/memory/` is also outside any project root.

Path safety enforced at finer grain via `memory::writable_entry_path`:
- Reject `..`, path separators (`/`, `\`), control chars, absolute paths
- Reject reserved stems (`MEMORY` case-insensitive)
- Canonicalize parent inside the memory root (symlink-escape defeated)
- Refuse if the memory root itself is a symlink

Same intentional carve-out pattern as `TodoWrite` (`.thclaws/todos.md`) and `KmsWrite` (`.thclaws/kms/...`) — clear precedent in the codebase.

### Tool registration

Always-on at three sites (matching every other tool registration):
- `shared_session.rs::build_state` — gui worker boot
- `repl.rs:run_print_mode` — CLI one-shot
- `repl.rs:run_repl` — CLI interactive

No conditional registration based on entry count — the empty case is handled gracefully by the tools (write creates the first entry; read errors with "no memory entry named '<name>'").

---

## 7. Slash commands

| Syntax | Effect |
|---|---|
| `/memory` (or `/memory list`) | List entries with `name [type] — description` |
| `/memory read <name>` (or `show` / `cat`) | Print one entry's full body |
| `/memory write <name>` | Open `$EDITOR` pre-filled with frontmatter scaffold (CLI only) |
| `/memory write <name> --body "..."` | One-shot inline write (CLI + GUI) |
| `/memory write <name> --type <user\|feedback\|project\|reference> --description "..." --body "..."` | Pre-fill frontmatter via flags |
| `/memory append <name> --body "..."` (or `add`) | Append chunk to entry; bumps `updated:` |
| `/memory edit <name>` | CLI-only: open `$EDITOR` pre-filled with existing content |
| `/memory delete <name>` (or `rm` / `remove`) | Confirm prompt → remove file + bullet. Skip prompt with `-y` / `--yes`. |
| `# <name>: <body>` (M6.27) | Quick-write shortcut — equivalent to `/memory write <name> --body "<body>"`. Strict slug-name pattern (`[A-Za-z0-9_-]+`) so real markdown headers like `# Architecture Plan: build a REST API` fall through to the agent unchanged. |

### Editor flow (CLI)

`/memory write <name>` (no `--body`) and `/memory edit <name>`:
1. `build_memory_scaffold` produces the pre-fill: existing frontmatter + body for edit, blank template (`name`, `description`, `type` keys empty) for write.
2. `spawn_editor_for_memory` writes scaffold to `$TMPDIR/thclaws-memory-<name>.md`, spawns `$EDITOR` (default `vi`).
3. On editor exit (status=0): read post-edit content, route through `memory::write_entry`. On non-zero exit: treat as cancellation.
4. Empty post-edit content (only whitespace): cancelled message, no write.

Same UX pattern as `git commit` editor mode.

### GUI behavior

GUI dispatch supports `--body` shortcut + `delete` only (no editor surface yet). `MemoryEdit` returns "GUI /memory edit isn't implemented yet (CLI-only)..." with a hint to use `MemoryWrite` tool via chat instead. Future enhancement: frontend modal with pre-fill (similar to skill-edit modal).

### Quote-aware parsing

`tokenize_quoted` handles `--body "long string with spaces"` and `'single-quoted'` correctly so multi-word frontmatter values work without bash escaping. Backslash escapes inside quotes are NOT honored (keep it simple).

### `# <name>: <body>` shortcut (M6.27)

Claude Code parity: typing `# user_role: senior backend engineer` is equivalent to `/memory write user_role --body "senior backend engineer"`. Implemented as `parse_memory_shortcut` inside `parse_slash`, so both CLI (`repl.rs::run_repl`) and GUI (`shared_session.rs::handle_line`) routes converge on the same `MemoryWrite` dispatch.

**Strict matching rules** (so real markdown headers don't get hijacked):
- Input must start with `# ` (hash + space) OR `#` immediately followed by the name
- `<name>` must be slug-style: `[A-Za-z0-9_-]+` only — a name with spaces (`Architecture Plan`) or special chars (`user.role`) bails the match
- Colon separator required
- Body must be non-empty after trim
- Only the FIRST colon splits — body may contain colons, dashes, etc.

When the pattern doesn't match, `parse_slash` returns `None` and the line passes through to the agent unchanged. So `# Architecture Plan: build a REST API` reaches the model as a normal markdown-prefixed prompt.

GUI dispatch in `shared_session.rs::handle_line` adds a small intercept BEFORE the `/`-handler block:

```rust
if matches!(parse_slash(trimmed), Some(SlashCommand::MemoryWrite { .. }))
    && !trimmed.starts_with('/')
{
    shell_dispatch::dispatch(trimmed, ...).await;
    return;
}
```

The CLI already dispatches via `parse_slash` → `match cmd` so no change needed there — `MemoryWrite` lands in the existing arm regardless of whether it came from `/memory write` or the `#` shortcut.

---

## 8. Auto-maintained `MEMORY.md`

Pre-M6.26 the user had to keep `MEMORY.md` in sync with on-disk entries by hand — adding a bullet when creating an entry, removing one when deleting. M6.26 automates this:

- **`write_entry`** → `update_memory_index_bullet(store, name, description)`:
  - Drops any existing bullet matching `(<name>.md)` (dedupe)
  - Appends `- [<name>](<name>.md) — <description>` (or `- [<name>](<name>.md)` when no description)
- **`append_to_entry`** (new entry only) → same `update_memory_index_bullet`
- **`delete_entry`** → `remove_memory_index_bullet(store, name)`:
  - Drops bullet, rewrites file

Match is anchored to `(<name>.md)` so unrelated entries with similar prefixes don't cross-hit.

User-managed bullets coexist — the auto-update only touches lines containing the matching link target. Section headers, free-form text, comments are preserved across writes.

---

## 9. Security model

### Path validation

Every entry-name input goes through `writable_entry_path`:
- Reject empty / `..` / `/` / `\` / `\0` / control chars / absolute paths / reserved `MEMORY` stem (case-insensitive)
- Canonicalize parent dir inside memory root (catches symlink escape)
- Refuse if memory root itself is a symlink

`MemoryRead` uses `MemoryStore::get(name)` which doesn't validate the same way (legacy: just reads `<root>/<name>.md`). If you pass `..` to `MemoryRead`, the underlying `parse_entry_file` returns `None` because the path doesn't exist (the actual file would be at a non-target path). Defense in depth would add the same `writable_entry_path`-style validation to read; not currently a footgun because the read is a no-op on traversal attempts.

### Approval gating

`MemoryWrite` and `MemoryAppend` set `requires_approval(_) = true` — same posture as `Write` and `KmsWrite`. In `PermissionMode::Ask` (default), every call surfaces an approval modal showing the entry name + content preview. `MemoryRead` is non-destructive, no approval needed.

### Reserved name

`RESERVED_ENTRY_STEMS = ["MEMORY"]` — `writable_entry_path` rejects (case-insensitive). Prevents accidental clobber of the auto-maintained index.

---

## 10. Code organization

```
crates/core/src/
├── memory.rs (1000+ LOC)               ── core: MemoryStore, MemoryEntry,
│                                          default_path resolution,
│                                          system_prompt_section (categorized + budget),
│                                          parse_frontmatter + write_frontmatter_map,
│                                          writable_entry_path,
│                                          write_entry/append_to_entry/delete_entry,
│                                          update/remove_memory_index_bullet,
│                                          truncate_for_prompt + truncate_index,
│                                          memory_sizes
├── tools/
│   └── memory.rs (380 LOC)             ── MemoryReadTool, MemoryWriteTool, MemoryAppendTool
├── shell_dispatch.rs (selected lines)  ── /memory slash handlers (GUI)
├── repl.rs (selected lines)            ── SlashCommand::Memory* enum + parser
│                                          (parse_memory_subcommand,
│                                          parse_memory_write_args,
│                                          parse_memory_append_args,
│                                          parse_memory_delete_args,
│                                          tokenize_quoted) + CLI dispatch +
│                                          editor flow (build_memory_scaffold,
│                                          spawn_editor_for_memory)
└── shared_session.rs (selected lines)  ── Memory tool registration at worker boot
```

---

## 11. Testing

| Module | Tests | Coverage |
|---|---|---|
| `memory::tests` | 22 | Frontmatter parse round-trip, `MEMORY.md` truncation (line + byte), `list`/`get` semantics, `system_prompt_section` (empty / full / oversized cap / categorized / budget defer / tool advertisement), `writable_entry_path` (traversal + reserved), `write_entry` (create + replace + index dedup + created preservation), `append_to_entry`, `delete_entry` + idempotence, `write_frontmatter_map` quoting |
| `tools::memory::tests` | 7 | Write tool (create + index update + replace dedup + traversal/reserved rejection), append (create + extend with frontmatter preservation), read (frontmatter+body shape + unknown errors), approval-gating posture |
| `repl::tests` | 1 | `parse_slash_memory` covers list / read / write / append / delete syntax |

Tests for write/append/delete operations on the static `MemoryStore` use `tempdir()` + explicit `MemoryStore::new(path)` — no env mutation needed. Tool-level tests use the `scoped_home()` env-guard pattern shared with KMS to make `default_path()` resolve to a tempdir.

794 lib tests pass (was 777 after M6.25). 17 new + 0 amended.

---

## 12. Migration

### Backwards compatibility

Pre-M6.26 entries (no frontmatter, or only `description`/`type`) load and read normally:
- `MemoryRead` returns the body
- `system_prompt_section` falls back to raw `MEMORY.md` rendering when no entry has `category:`
- The 80-line / 8 KB per-entry cap and 16 KB total budget apply uniformly

### M6.26 changes

- **Tools added**: `MemoryRead`, `MemoryWrite`, `MemoryAppend` (always-on)
- **Slash commands added**: `/memory write`, `/memory append`, `/memory edit`, `/memory delete`
- **Auto-maintained `MEMORY.md`**: writes/deletes update the index; pre-existing entries don't trigger updates until the user (or model) writes them
- **Frontmatter `category:`** convention introduced; legacy entries without it group under `**uncategorized**`
- **Budget cap**: 16 KB total inlined bodies; overflow becomes deferred pointers
- **`created:` / `updated:` auto-stamping**: applied only on writes through the new helpers; pre-existing entries keep their (likely missing) stamps until next write

### Sprint chronology

- **Phase 13b** (initial) — read-only `MemoryStore`, `system_prompt_section`, `parse_frontmatter`, `/memory list`+`read`
- **M6.18 BUG M5/M6** (`dev-log/136`) — per-entry body cap (80 lines / 8 KB), MEMORY.md index cap (200 lines / 25 KB), shared `truncate_for_prompt` helper
- **M6.26** (`dev-log/144`) — LLM-maintainable: write/append/delete, auto-index, categorized prompt, budget cap, on-demand reads, slash command CRUD

### Known limitations

- **GUI `/memory edit` not implemented** — CLI only. Workaround: `/memory write <name> --body "..."` to overwrite, or ask agent via chat.
- **No multiple instances** — single memory per workspace. Can't have separate "personal" and "team" memory partitions. (KMS solves this via `/kms use <name>`; memory has no analog.)
- **`MemoryRead` doesn't validate path traversal** — defensively safe (no file at `..` paths) but inconsistent with `MemoryWrite` validation. Hardening candidate.
- **Inlining order is alphabetical, not relevance-sorted** — small entries early in the alphabet have an advantage when filling the budget. Users who want priority should keep their always-needed entries small enough to fit.
- **No file locking** on concurrent writes from multiple processes. Same posture as KMS pre-M6.26 / TodoWrite. Last-writer-wins.

---

## 13. Comparison with KMS

Memory and KMS are sibling subsystems — both directories of markdown files with frontmatter, both auto-loaded into the system prompt, both with their own R/W tools post-M6.25/M6.26. Differences:

| Aspect | Memory | KMS |
|---|---|---|
| Cardinality | Single global memory per workspace | Many KMSes; attach via `/kms use <name>` |
| Sources/page split | None (single layer) | Yes — `sources/` + `pages/` since M6.25 |
| Ingest workflow | Manual write only | `/kms ingest` from file/URL/PDF |
| Lint | None | `/kms lint` |
| Re-ingest cascade | n/a | Yes (frontmatter `sources:` triggers stale markers) |
| Schema injection | Tool-affordance block in prompt | `SCHEMA.md` content in prompt |
| Index in prompt | Categorized (M6.26) | Categorized (M6.25) |
| On-demand read | Yes (M6.26 budget cap) | Yes (always — KMS pages aren't auto-inlined) |
| Frontmatter parser | `memory::parse_frontmatter` (HashMap) | `kms::parse_frontmatter` (BTreeMap) |
| Frontmatter writer | `memory::write_frontmatter_map` (HashMap, sorted output) | `kms::write_frontmatter` (BTreeMap, naturally sorted) |

The two parsers are nearly identical (M6.26 brought memory's quote-stripping in line with KMS) but use different map types for historical reasons. Consolidation candidate for a future cleanup sprint.
