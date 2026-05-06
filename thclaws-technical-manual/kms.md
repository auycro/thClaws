# KMS — Knowledge Management System

A KMS is a directory of markdown pages plus an `index.md` (table of contents), a `log.md` (append-only change history), a `SCHEMA.md` (page conventions), and split `pages/` + `sources/` subdirs. The LLM is both **reader** and **maintainer**: `KmsRead` / `KmsSearch` consult, `KmsWrite` / `KmsAppend` author, `/kms ingest` adds sources, `/kms lint` audits, `/kms file-answer` files explorations back. Karpathy-style: no embeddings, just grep + read + frontmatter.

This doc covers: the three-layer architecture, on-disk layout, YAML frontmatter convention, ingest semantics (split source/page + URL/PDF support), system-prompt injection, slash commands, the four KMS tools, lint operations, the re-ingest cascade, security model, Obsidian compatibility, and the cross-process behavior.

**Source modules:**
- `crates/core/src/kms.rs` — `KmsRef`, `KmsScope`, `create`, `resolve`, `list_all`, `ingest` + `ingest_url` + `ingest_pdf`, `write_page` + `append_to_page` + `writable_page_path`, `parse_frontmatter` + `write_frontmatter`, `lint` + `LintReport`, `system_prompt_section` + categorized index, `mark_dependent_pages_stale`
- `crates/core/src/tools/kms.rs` — `KmsReadTool`, `KmsSearchTool`, `KmsWriteTool`, `KmsAppendTool`
- `crates/core/src/shell_dispatch.rs` — `/kms` slash-command handlers (GUI path); `format_lint_report` + `sanitize_alias_for_dispatch`
- `crates/core/src/repl.rs` — `SlashCommand::Kms*` enum + parser + CLI dispatch
- `crates/core/src/shared_session.rs` — `kms_active`-driven tool registration at worker boot
- `crates/core/src/config.rs` — `kms_active` persistence in `.thclaws/settings.json` via `ProjectConfig::set_active_kms`

**Cross-references:**
- [`built-in-tools.md`](built-in-tools.md) §3 — `KmsRead` + `KmsSearch` + `KmsWrite` + `KmsAppend` tool surface
- [`context-composer.md`](context-composer.md) — `kms::system_prompt_section()` injects per-active-KMS Schema/Index/Tools blocks
- [`permissions.md`](permissions.md) — `KmsWrite` / `KmsAppend` `requires_approval()` posture (mutating; gated in Ask mode)
- [`sessions.md`](sessions.md) — `/kms file-answer` reads from `state.session.messages` (the live session)
- [`commands.md`](commands.md) — `/kms` is a built-in slash command (not a `.md` prompt template)

---

## 1. Overview

### Concept

A KMS implements the [llm-wiki pattern](../docs/llm-wiki.md): a persistent, **compounding** knowledge base maintained by the LLM. Most LLM-document workflows look like RAG — index a corpus, retrieve chunks at query time, generate. Knowledge gets re-derived on every question. A KMS instead has the LLM build and maintain a structured wiki between you and the raw sources: cross-references compiled once and kept current, contradictions flagged, summaries refreshed when sources change.

The maintenance burden is the LLM's job; the curation + question-asking + direction is yours.

### Three layers

```
┌── sources/           layer 1: raw, immutable. LLM reads, never modifies.
│   ├── article.md     The source of truth. CSV, txt, json, md, fetched HTML.
│   └── paper.txt
│
├── pages/             layer 2: the wiki. LLM-authored markdown with frontmatter.
│   ├── api-x.md       Curated summaries, entity pages, concept pages,
│   ├── paper-y.md     comparisons. Each page references its sources via
│   └── synthesis.md   frontmatter `sources:` field. Cross-links via
│                      [label](pages/other.md). LLM owns this layer.
│
├── SCHEMA.md          layer 3: the schema. Human-edited rules for layer 2.
├── index.md           Auto-maintained table of contents (one bullet per page).
└── log.md             Auto-maintained change history (## [date] verb | alias).
```

### Lifecycle

```
USER  /kms new mynotes              → create() seeds index/log/SCHEMA + dirs
USER  /kms use mynotes              → adds to .thclaws/settings.json kms_active
                                      → registers KmsRead/Search/Write/Append tools
                                      → next system prompt includes KMS block
USER  /kms ingest mynotes file.md   → copy to sources/, write stub in pages/
LLM   reads stub, KmsRead source,   → enriched page with curated summary +
      KmsWrite enriched page          frontmatter category/tags
USER  asks question                 → LLM consults index, KmsRead pages, answers
USER  /kms file-answer mynotes "X"  → assistant message → new page (compounds)
USER  /kms lint mynotes             → broken links / orphans / drift / missing FM
USER  /kms ingest mynotes file.md   → cascade marks dependent pages STALE
        --force                       (frontmatter sources: <alias>)
USER  /kms off mynotes              → unregisters tools, removes from kms_active
```

---

## 2. On-disk layout

### Two scopes

```
<project>/.thclaws/kms/<name>/     # Project scope — only visible from this project (DEFAULT)
~/.config/thclaws/kms/<name>/      # User scope — visible from any project (--user opt-in)
```

`KmsScope` is a closed enum (`User` | `Project`). Both directories are walked by `list_all()`. `resolve(name)` checks **project first**, falls back to user — so a project-scope `notes` overrides a user-scope `notes` for that project. Same precedence pattern as project CLAUDE.md vs user CLAUDE.md.

`/kms new <name>` defaults to project scope (a KMS is typically tied to the code you're working on, so `./.thclaws/kms/<name>` follows the repo). `--user` opts out into user-global. `--project` is accepted as a no-op alias so muscle memory from the old default doesn't break.

### Directory contents (`kms::create` seeds)

```
<kms_root>/
├── index.md      # "# <name>\n\nKnowledge base index — list each page with a one-line summary.\n"
├── log.md        # "# Change log\n\nAppend-only list of ingests / edits / lints.\n"
├── SCHEMA.md     # Starter schema content (edit this to set conventions)
├── pages/        # Wiki pages (LLM-authored, frontmatter-tagged)
└── sources/      # Raw source files (immutable; copied here by `/kms ingest`)
```

`SCHEMA.md`, `index.md`, `log.md` stems are reserved (`RESERVED_PAGE_STEMS`); ingest + write tools refuse those aliases so the LLM can't clobber them by mistake.

---

## 3. Frontmatter convention

Pages may begin with a YAML frontmatter block. The convention covers five fields; any others are stored verbatim and re-emitted on round-trip:

```markdown
---
category: research
tags: ai, retrieval
sources: paper-x, paper-y
created: 2026-05-03
updated: 2026-05-03
---

# Topic body
...
```

| Field | Meaning | Used by |
|---|---|---|
| `category` | One-word grouping | Categorized index in system prompt |
| `tags` | Comma-separated labels | Dataview queries (Obsidian) |
| `sources` | Comma- or space-separated source aliases | Re-ingest cascade (BUG #10) |
| `created` | YYYY-MM-DD; auto-stamped on first write | Audit |
| `updated` | YYYY-MM-DD; auto-stamped on every write | Sort / freshness |

### Parser

`kms::parse_frontmatter(s) -> (BTreeMap<String, String>, String)` is hand-rolled (no `serde_yaml` dep). Single-line string values only — no nesting, anchors, or multiline. Pages without frontmatter return `(empty, original)`.

`kms::write_frontmatter(map, body) -> String` round-trips. Auto-quotes values containing `:`, `#`, leading whitespace, `"`, or `\n`:

```rust
fm.insert("note".into(), "has: colon".into());
write_frontmatter(&fm, "body\n");
// → "---\nnote: \"has: colon\"\n---\nbody\n"
```

---

## 4. Operations

### `ingest` — adding raw sources (M6.25 BUG #2)

`kms::ingest(kref, source_path, alias, force)` does a **two-step split**:

1. Copy raw bytes to `sources/<alias>.<ext>` (immutable; never re-touched by LLM tools)
2. Write a stub page `pages/<alias>.md` with frontmatter pointing back at the source:

```markdown
---
category: uncategorized
created: 2026-05-03
sources: <alias>
updated: 2026-05-03
---
# <alias>

Stub page — raw source at `sources/<alias>.<ext>`. Summary line: <first content line>

_Replace this stub with a curated summary, key takeaways, cross-references to other pages, etc._
```

The LLM enriches the stub via `KmsWrite`. Pre-M6.25 ingest copied the source straight into `pages/`, conflating layer 1 (raw) with layer 2 (synthesis) — fixed in M6.25 ([dev-log/143](../dev-log/143-kms-m6-25-llm-wiki-alignment.md)).

`force=true` re-runs the copy + stub write AND triggers the re-ingest cascade (§4.6).

Allowed source extensions: `md`, `markdown`, `txt`, `rst`, `log`, `json` (`INGEST_EXTENSIONS`). Anything else → "not supported — allowed: …" error. URL + PDF flow through dedicated wrappers (§4.2 + §4.3).

### `ingest_url` — fetching remote sources (M6.25 BUG #8)

```rust
kms::ingest_url(kref, url, alias, force).await
```

Fetches via `reqwest::Client::builder().timeout(30s)`, prepends a `<!-- fetched from {url} on {date} -->` banner to the response body, stages to a temp `.md` file, routes through standard `ingest()`. Status check rejects non-2xx.

Alias derivation: explicit `--alias` wins; otherwise the last path segment (stripped of query string). Sanitized via `sanitize_alias` (`[A-Za-z0-9_-]` only; trim outer `_`).

### `ingest_pdf` — extracting PDF text (M6.25 BUG #8)

```rust
kms::ingest_pdf(kref, pdf_path, alias, force).await
```

Spawns `pdftotext -layout -enc UTF-8 <path> -` in a `tokio::task::spawn_blocking` (same shape as `PdfReadTool`), prepends `<!-- extracted from PDF '<path>' on <date> -->`, stages to temp, routes through `ingest()`. Requires `poppler-utils` installed locally.

Alias: explicit `--alias` wins; otherwise the file stem.

### `write_page` — author or replace a page (M6.25 BUG #1)

```rust
kms::write_page(kref, page_name, content) -> Result<PathBuf>
```

Create-or-replace. Parses frontmatter from `content`; merges with auto-stamped:
- `created:` (only on new pages — preserved on replace)
- `updated:` (always today's date)

User-supplied frontmatter keys win on conflict. Then writes the merged frontmatter + body to `pages/<page>.md`, updates `index.md`, appends a `## [date] wrote | <stem>` log entry.

Path validation: `kms::writable_page_path` rejects empty / `..` / path separators / control chars / absolute paths / reserved stems. Canonicalizes parent inside `pages_dir` (defeats symlink escape). Refuses if `pages/` itself is a symlink.

### `append_to_page` — incremental updates (M6.25 BUG #1)

```rust
kms::append_to_page(kref, page_name, chunk) -> Result<PathBuf>
```

If the page exists with frontmatter: bumps `updated:`, appends chunk after a separating newline, re-serializes. If exists without frontmatter: plain `OpenOptions::append`. If doesn't exist: creates with bare body (no frontmatter — LLM can rewrite via `KmsWrite` later to add metadata). Always appends a `## [date] appended | <stem>` log entry.

### `lint` — health check (M6.25 BUG #3)

```rust
kms::lint(kref) -> Result<LintReport>
```

Pure-read; no mutation. Walks `pages/`, returns five issue categories:

| Field | Meaning |
|---|---|
| `broken_links: Vec<(page, target)>` | `[label](pages/x.md)` where `pages/x.md` doesn't exist |
| `orphan_pages: Vec<String>` | Page on disk with no inbound link from any other page |
| `index_orphans: Vec<String>` | Index entry with no underlying file |
| `missing_in_index: Vec<String>` | Page on disk with no index entry |
| `missing_frontmatter: Vec<String>` | Page with no `---\n…\n---\n` block |

`LintReport::total_issues()` sums all. `format_lint_report(name, &report)` (in `shell_dispatch.rs`) renders the user-facing summary with per-category counts.

### Re-ingest cascade (M6.25 BUG #10)

When `ingest()` replaces an existing alias (`force=true` + `page_existed`), `mark_dependent_pages_stale(kref, changed_alias)` walks every page; if frontmatter `sources:` mentions the changed alias (comma- or whitespace-separated), the page's body gets a stale marker:

```markdown
> ⚠ STALE: source `<alias>` was re-ingested on 2026-05-03. Refresh this page.
```

The page's `updated:` field bumps. Returned in `IngestResult.cascaded`; surfaced in slash-command output: `"marked N dependent page(s) stale"`. The user (or LLM next turn) acts on the markers via `KmsWrite`.

---

## 5. System-prompt injection (M6.25 BUG #5 + #6)

`kms::system_prompt_section(active: &[String]) -> String` is called by [`context-composer`](context-composer.md) and the REPL system-prompt builder. Returns `""` when no active KMS or when names resolve to nothing.

Output shape (per active KMS):

```markdown
# Active knowledge bases

The following KMS are attached to this conversation. Their schemas + indices are below
— consult them before answering when the user's question overlaps. Treat KMS content
as authoritative over your training data for the topics it covers. You are both reader
AND maintainer: file new findings, update entity pages when sources contradict them,
and run `/kms lint <name>` periodically.

## KMS: mynotes (project)

### Schema
<first 100 lines / 5 KB of SCHEMA.md>

### Index
**research**
- [paper-a](pages/paper-a.md) — Paper A summary line
- [paper-b](pages/paper-b.md) — Paper B summary line

**api**
- [api-x](pages/api-x.md) — API X reference

### Tools
- `KmsRead(kms: "mynotes", page: "<page>")` — read one page
- `KmsSearch(kms: "mynotes", pattern: "...")` — grep across pages
- `KmsWrite(kms: "mynotes", page: "<page>", content: "...")` — create or replace a page
- `KmsAppend(kms: "mynotes", page: "<page>", content: "...")` — append to a page
Pages may carry YAML frontmatter (`category:`, `tags:`, `sources:`, `created:`, `updated:`).
Follow the schema above when authoring.
```

### Categorized index

`render_index_section(kref)` walks `pages/`, parses frontmatter, groups bullets under `**<category>**` headers (BTreeMap-sorted). Pages without `category:` go under `**uncategorized**`. **Falls back** to the raw `index.md` (capped per the M6.18 BUG M7 limits — 200 lines / 25 KB) when no page has frontmatter — preserves backwards compat with pre-M6.25 KMSes that haven't adopted frontmatter yet.

The categorized form is also capped at `MEMORY_INDEX_MAX_LINES` (200); if exceeded:

```
_… index truncated at 200 entries (total: 487)_
```

### Schema cap

`SCHEMA.md` injection is capped at **100 lines / 5 KB** via `read_text_capped` — schemas are meant to be brief instructions, not archives.

---

## 6. Slash commands

| Syntax | Effect |
|---|---|
| `/kms` (or `/kms list` / `/kms ls`) | List KMSes, mark active with `*` |
| `/kms new <name>` | Create **project-scope** KMS (default — `./.thclaws/kms/<name>`) |
| `/kms new --user <name>` | Create user-scope KMS (`~/.config/thclaws/kms/<name>`) |
| `/kms use <name>` | Attach (registers tools, includes in prompt, persists to `.thclaws/settings.json`) |
| `/kms off <name>` | Detach (drops tools when last KMS detaches) |
| `/kms show <name>` | Show scope + path + attached state |
| `/kms ingest <kms> <file.md>` | Standard text ingest |
| `/kms ingest <kms> <file.pdf>` | Auto-routed to `ingest_pdf` (`pdftotext`) |
| `/kms ingest <kms> https://...` | Auto-routed to `ingest_url` (HTTP fetch + 30s timeout) |
| `/kms ingest <kms> $` (M6.28) | `$` source = current chat session. Triggers an agent turn that summarizes history and calls `KmsWrite` to file the page. Page name resolves from session.title (sanitized) when set, else session.id (`sess-<hex>`); user can override via `as <alias>`. Frontmatter pre-set to `category: session, sources: chat`. |
| `/kms ingest <kms> <target> as <alias>` | Override derived alias |
| `/kms ingest <kms> <target> --force` | Replace + cascade dependents |
| `/kms lint <name>` (or `check` / `doctor`) | Health-check report |
| `/kms file-answer <kms> <title>` (or `file`) | File latest assistant message as a new page |

**Source auto-detection** in `parse_slash`: `t == "$"` → `KmsIngestSession` (M6.28); `t.starts_with("http://") || t.starts_with("https://")` → `KmsIngestUrl`; `t.to_ascii_lowercase().ends_with(".pdf")` → `KmsIngestPdf`; otherwise `KmsIngest`.

### `/kms ingest <name> $` — file the current chat session (M6.28)

Special source target `$` triggers an **agent turn**, not a synchronous ingest. The slash command rewrites itself into a prompt that instructs the model to:

1. Summarize the current conversation as a self-contained wiki page (200-1500 words, synthesized — not transcribed).
2. Call `KmsWrite(kms: "<name>", page: "<page>", content: "...")` with frontmatter `category: session, sources: chat, description: <one-line hook>`.
3. Confirm to the user with the resolved path.

Page name resolves at the call site via `repl::resolve_session_alias` with this precedence:

1. **User-supplied** via `as <alias>` (sanitized through `kms::sanitize_alias`)
2. **Session title** if `state.session.title` is set (sanitized — spaces and punctuation become `_`)
3. **Session id** as fallback (`sess-<hex>`, already slug-safe)

The provenance is passed alongside the resolved slug as `KmsIngestSessionAliasSource` so the prompt's "Page name:" hint tells the model where the slug came from — and lets it refine the slug only when the conversation has a clearer theme than the auto-derived one.

`--force` flag forwarded as a hint to overwrite on collision.

Implementation: `parse_kms_subcommand` returns `SlashCommand::KmsIngestSession` for the `$` target. Both CLI (`run_repl`) and GUI (`shared_session::handle_line`) intercept this variant in their **rewrite-before-match** blocks (alongside skill / command rewrites). The slash command `line` is replaced with the prompt from `repl::build_kms_ingest_session_prompt`, then the regular agent-turn pipeline takes over — the rewrite becomes the user prompt for that turn.

`shell_dispatch::dispatch` has a defensive arm for `KmsIngestSession` that emits a clear error if it's ever reached directly (which shouldn't happen in normal flow — the rewrite intercepts first).

Every dispatch handler exists in **two places** — `shell_dispatch.rs` (GUI worker, async) and `repl.rs` (CLI loop). Both call the same `kms::*` functions; only the output formatting differs (CLI uses `COLOR_DIM`/`COLOR_YELLOW` ANSI codes).

---

## 7. Tool surface (LLM-callable)

When at least one KMS is in `kms_active`, four tools register into the `ToolRegistry`:

| Tool | Approval | Purpose |
|---|---|---|
| `KmsRead` | No | Read a single page |
| `KmsSearch` | No | Regex grep across all pages in one KMS |
| `KmsWrite` | **Yes** | Create or replace a page |
| `KmsAppend` | **Yes** | Append to a page |

When `kms_active` empties (last `/kms off`), all four tools are removed from the registry so the model doesn't see stale affordances. See [`built-in-tools.md`](built-in-tools.md) §3 for the full input-schema definitions.

### Sandbox carve-out (M6.25 BUG #1)

`KmsWrite` and `KmsAppend` deliberately bypass `Sandbox::check_write`. Rationale: project-scope KMS lives at `.thclaws/kms/.../pages/...` which the sandbox blocks (the `.thclaws/` reserved-dir rule). User-scope KMS lives at `~/.config/thclaws/kms/...` which is also outside any project root.

Path safety is enforced at finer grain via `kms::writable_page_path` instead:
- Reject `..`, path separators, control chars, absolute paths, reserved stems
- Canonicalize the parent dir inside `pages_dir` (symlink-escape defeated)
- Refuse if `pages/` itself is a symlink

Same intentional carve-out pattern as `TodoWrite`'s `.thclaws/todos.md` write — clear precedent in the codebase.

### Tool registration sites

Three places register the four tools when `!config.kms_active.is_empty()`:
- `shared_session.rs:660` — gui worker boot (`build_state`)
- `shell_dispatch.rs:1330` — `/kms use` handler (live-register so the next turn sees them)
- `repl.rs:1706` + `repl.rs:1779` — CLI boot (one for `run_print_mode`, one for `run_repl`)

`/kms off` removes them via `tool_registry.remove("KmsRead" / "KmsSearch" / "KmsWrite" / "KmsAppend")` when the active list goes empty.

---

## 8. Security model

### Path traversal defense

Every page-name input goes through one of:
- `KmsRef::page_path(page)` — used by `KmsRead`; canonicalizes the resolved file (must exist) and verifies it's under the KMS root
- `kms::writable_page_path(kref, page_name)` — used by `KmsWrite` / `KmsAppend`; canonicalizes the parent dir, verifies inside `pages_dir`, refuses symlinked `pages/`

Both reject before touching the filesystem: empty / `..` / `/` / `\` / `\0` / control chars / absolute paths / reserved stems (`index`, `log`, `SCHEMA`).

### Symlink defense (multi-layer)

| Vector | Defense |
|---|---|
| `~/.config/thclaws/kms/evil` is a symlink to `/etc` | `resolve()` uses `symlink_metadata` + `is_symlink()` check → refuses |
| `pages/` itself is a symlink to `/etc` | `KmsSearch` + `writable_page_path` refuse via `symlink_metadata` |
| `pages/leak.md` is a symlink to `~/.ssh/id_rsa` | `KmsSearch` skips entries where `entry.file_type().is_symlink()`; `KmsRead` rejects via `page_path`'s canonicalize-then-verify-under-root check |

The `system_prompt_section` injection also refuses to read `index.md` / `SCHEMA.md` if they are symlinks (`read_index` + `read_text_capped` both check `symlink_metadata`).

### KMS-name validation

`kms::create(name, scope)` rejects names that contain `/`, `\`, `..`, `\0`, control chars, or start with `.` or that are absolute paths.

### Approval gating

`KmsWrite` and `KmsAppend` set `requires_approval(_) = true` — same posture as `Write`. In `PermissionMode::Ask` (default), every call surfaces an approval modal showing the page path and a content preview.

### Reserved aliases

`RESERVED_PAGE_STEMS = ["index", "log", "SCHEMA"]` — `ingest()`, `write_page`, `append_to_page` all refuse these (case-insensitive). Prevents accidental clobber of the seed files.

---

## 9. Obsidian compatibility

A KMS root opens cleanly as an Obsidian vault — pages, index, log, schema are all plain `.md` with valid YAML frontmatter:

1. Obsidian → "Open folder as vault" → `~/.config/thclaws/kms/<name>` (user) or `<project>/.thclaws/kms/<name>` (project — `.thclaws` is hidden, use the path bar).
2. Install **Dataview** plugin → query frontmatter:
   ```dataview
   LIST FROM "pages" WHERE category = "research"
   TABLE updated, sources FROM "pages" SORT updated DESC
   ```
3. Graph view shows edges from our standard `[label](pages/x.md)` links.

### Caveats vs hand-built Obsidian vault

- We emit standard markdown links, not wikilinks (`[[x]]`). Both render in graph view, but markdown links don't auto-update if you rename a page from inside Obsidian. The LLM can write either form via `KmsWrite` — Obsidian renders both, and our `lint` link-detection regex only catches markdown form (so wikilinks won't trigger broken-link warnings — fine for cross-references between local-only pages).
- `tags:` is single-string in our frontmatter (`tags: a, b`). Obsidian/Dataview also support list form (`tags: [a, b]`). If you want list form, write that via `KmsWrite` — our parser keeps the raw string and Dataview parses it correctly.

### Mutual coexistence

Obsidian creates `.obsidian/` config inside the vault root. KMS code never reads it. KMS creates `pages/`, `sources/`, `index.md`, `log.md`, `SCHEMA.md`. Obsidian renders all of those as regular files. No conflicts.

---

## 10. Cross-process behavior

### Concurrent reads

Multiple processes (CLI + GUI in same project, two GUIs) can read freely — `KmsRead`, `KmsSearch`, and `system_prompt_section` only `std::fs::read_to_string`.

### Concurrent writes

`KmsWrite`, `KmsAppend`, `ingest()` use plain `std::fs::write` / `OpenOptions::append` — **no file locking**. Same posture as the rest of `.thclaws/` reserved files (`todos.md`, plan_state). Last-writer-wins on `pages/<x>.md` and `index.md`. The `log.md` append uses `OpenOptions::append` which is per-write atomic for ≤ PIPE_BUF (~4KB); log entries are small headers so this is safe in practice.

If you run heavy concurrent edits across processes, add file locking via `fs2::FileExt::lock_exclusive` (the M6.24 sessions pattern) — not currently warranted by the access pattern.

---

## 11. Configuration

### `.thclaws/settings.json`

Active KMS list persists per-project:

```json
{
  "kms_active": ["mynotes", "team-wiki"]
}
```

Mutated only via `ProjectConfig::set_active_kms(Vec<String>)`, called by `/kms use` and `/kms off`. The list is consumed at:
- Worker boot (`shared_session.rs::build_state`) → registers KMS tools
- Every `kms::system_prompt_section(&config.kms_active)` call → builds the prompt block

### Settings layering (per [`config.rs`](../crates/core/src/config.rs))

`kms_active` is a project-scope-only setting — there's no user-scope or compiled-in default. New projects start with `kms_active: []` (no KMSes attached, even if the user has user-scope KMSes available).

---

## 12. Code organization

```
crates/core/src/
├── kms.rs (1500+ LOC)                  ── core: KmsRef, scopes, create/resolve,
│                                          ingest + ingest_url + ingest_pdf,
│                                          write_page + append_to_page,
│                                          parse/write_frontmatter,
│                                          lint + LintReport,
│                                          system_prompt_section + categorized index,
│                                          mark_dependent_pages_stale
├── tools/
│   └── kms.rs (430 LOC)                ── KmsRead, KmsSearch, KmsWrite, KmsAppend
├── shell_dispatch.rs (selected lines)  ── /kms slash handlers (GUI), format_lint_report,
│                                          sanitize_alias_for_dispatch
├── repl.rs (selected lines)            ── SlashCommand::Kms* enum + parser + CLI dispatch
├── shared_session.rs (selected lines)  ── KMS tool registration at worker boot
└── config.rs (selected lines)          ── kms_active persistence
```

`tools/kms.rs` keeps a process-wide test env-lock (`test_env_lock`) shared with `kms.rs`'s test module — both mutate `HOME` + cwd to scope test KMSes to a tempdir, which would race without the lock.

---

## 13. Testing

| Module | Tests | Coverage |
|---|---|---|
| `kms::tests` | 27 | create/resolve, scope precedence, traversal/symlink rejection, ingest split, ingest collision, frontmatter round-trip, write_page (new + replace + index dedup), append_to_page, writable_page_path, lint (issue detection + clean), system_prompt_section (schema injection + categorized index), re-ingest cascade |
| `tools::kms::tests` | 11 | read round-trip, missing extension fallback, unknown KMS, search semantics, write tool (create + traversal + unknown KMS), append tool (create + extend), approval-gating posture |

Tests use `scoped_home()` (drops on test end via `EnvGuard`) to set `HOME` + `USERPROFILE` + cwd to a fresh tempdir. Every test that mutates env acquires `test_env_lock` to serialize against parallel test execution.

Symlink-rejection tests are `#[cfg(unix)]` only (Windows symlinks need extra permissions); Windows is excluded from CI tests anyway (per `CLAUDE.md`).

---

## 14. Migration / known limitations

### Backwards compatibility

Pre-M6.25 KMSes (no frontmatter, no source/page split) load and read fine:
- `system_prompt_section` falls back to raw `index.md` rendering when no page has frontmatter
- `KmsRead` reads any `pages/*.md` regardless of frontmatter presence
- `KmsSearch` greps any `pages/*.md`

Re-ingesting an old file with `--force` produces the new split shape (raw → `sources/`, stub → `pages/`). One-shot upgrade per source.

### M6.25 changes (`dev-log/143`)

10 of 11 audit issues from `docs/llm-wiki.md` gap analysis shipped:
- BUG #1 — `KmsWrite` + `KmsAppend` tools (sandbox carve-out)
- BUG #2 — Source/page split in `ingest()`
- BUG #3 — `/kms lint` health check
- BUG #4 — `/kms file-answer` files latest assistant message
- BUG #5 — `SCHEMA.md` injected into system prompt
- BUG #6 — Categorized index by frontmatter `category:`
- BUG #7 — Log format → `## [date] verb | alias` (greppable)
- BUG #8 — URL ingest (HTTP) + PDF ingest (`pdftotext`)
- BUG #9 — YAML frontmatter parser/serializer
- BUG #10 — Re-ingest cascade marks dependent pages stale

### M6.18 fix (`dev-log/136`)

BUG M7 — `system_prompt_section` index cap. Three active KMSes each with an 80K index used to burn 240K tokens of system prompt every turn. Now capped at 200 lines / 25 KB per KMS via `crate::memory::truncate_for_prompt`. Schema injection (M6.25) reuses the same pattern with 100 lines / 5 KB.

### Deferred (not yet shipped)

- **BUG #11** — `qmd` hybrid search (BM25 + vector + LLM rerank). External dep ([github.com/tobi/qmd](https://github.com/tobi/qmd)), opt-in. Could ship as a `KmsSearchHybrid` tool that shells out to `qmd` if installed; not blocking core llm-wiki alignment.

### Known limitations

- **No file locking** on concurrent KMS writes from multiple processes (§10). Last-writer-wins. Not currently a footgun given the access pattern.
- **`tags:` single-string only** in our frontmatter parser. Obsidian list form (`tags: [a, b]`) is preserved verbatim but treated as one string by our `lint` (which doesn't query tags) and the categorized-index renderer (which only reads `category`).
- **Inbound link detection** in `lint` only matches `[label](pages/x.md)` markdown form. Wikilinks (`[[x]]`) won't trigger broken-link warnings; orphan detection won't credit them as inbound either. Acceptable since our generated pages use markdown form.
- **No `qmd` integration** — at scale (>500 pages), `KmsSearch` regex + read may be slow.

### Sprint chronology

- **M6.18** — system-prompt index cap (BUG M7) — `dev-log/136`
- **M6.25** — llm-wiki concept alignment, 10 of 11 audit issues — `dev-log/143`
