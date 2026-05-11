# Chapter 9 — Knowledge bases (KMS)

A **knowledge base** (KMS — Knowledge Management System) is a folder of markdown pages you curate, plus an `index.md` table of contents the agent reads on every turn. Inspired by Andrej Karpathy's [LLM wiki pattern](https://gist.github.com/karpathy/442a6bf555914893e9891c11519de94f), thClaws ships with KMS built in — no embeddings, no vector store, just grep + read.

Use cases:

- **Personal notes** — everything you've learned about an API, a library, a client's codebase
- **Project reference** — architectural decisions, design principles, common patterns for a specific repo
- **Team playbook** — standard operating procedures, onboarding checklists
- **Language-specific** — Thai-aware content (the default works out of the box for Thai thanks to the Grep-based retrieval)

## How it's different from memory or AGENTS.md

| | Scope | Size | Retrieval |
|---|---|---|---|
| **AGENTS.md** | Full text injected every turn | Small (<few KB) | No retrieval — always in prompt |
| **Memory** | Individual facts by type | Small (index + body refs) | Frontmatter indexed, body pulled on need |
| **KMS** | Entire wiki, lazy-loaded | Unbounded (thousands of pages fine) | Grep search + targeted page reads |

Rule of thumb: memory is for things about *you* and *how you work*. AGENTS.md is for project conventions. KMS is for *content* the agent looks things up in.

## Scopes

Two scopes, identical internal structure:

- **User** — `~/.config/thclaws/kms/<name>/` — available in every project
- **Project** — `.thclaws/kms/<name>/` — lives with the repo, follows it into git if tracked

When the same name exists in both scopes, the **project** version wins.

## Layout of a KMS directory

```
<kms_root>/
├── index.md       ← table of contents, one line per page. The agent reads this every turn.
├── log.md         ← append-only change log (humans + agent write here)
├── SCHEMA.md      ← optional: prose shape rules for pages
├── manifest.json  ← schema version + optional frontmatter requirements (see "Schema versioning")
├── pages/         ← individual wiki pages, one per topic
│   ├── auth-flow.md
│   ├── api-conventions.md
│   └── troubleshooting.md
└── sources/       ← raw source material (URLs, PDFs, notes) — optional
```

`/kms new` seeds all of the above with minimal starter content so you can start writing immediately.

## Adding content: capture and ingest

Three ways to put content into a KMS, in order of how much structure you give the agent up front. Pick whichever matches your situation.

### Natural language

Just talk. The agent writes markdown like it writes any other file:

```
❯ I just read https://example.com/oauth-guide. Ingest the key points into 'notes'.

[assistant] Reading the page…
[tool: WebFetch(url: "https://example.com/oauth-guide")]
[tool: Write(path: "~/.config/thclaws/kms/notes/pages/oauth-client-credentials.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/index.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/log.md", ...)]
Wrote pages/oauth-client-credentials.md, added entry to index.md, appended to log.md.
```

This works for anything — articles, screenshots, transcripts, tasks. The agent figures out where things go and writes the page, the index entry, and the log entry.

### Slash commands for common shapes

When the source has a fixed shape, a slash command saves you the prompt-engineering. Each is documented in its own section below — quick map here:

- **`/kms ingest NAME <file-or-url-or-$>`** — pull a file, URL, PDF, or the current chat session into the KMS as a stub page
- **`/kms dump NAME <text>`** — paste freeform content; the agent classifies the dump into chunks and routes each to the right destination
- **`/kms file-answer NAME <title>`** — file the latest assistant message as a new KMS page

### Karpathy's three operations

The conceptual model behind all of this:

1. **Ingest** — read a source, extract distinct facts, write a page, update the index, append to the log
2. **Query** — answer a question from the wiki (the agent does this naturally when the KMS is attached)
3. **Lint** — periodically read all pages and flag merges, splits, or orphans to fix

You can run all three via natural language. The slash commands are shortcuts.

## Multi-KMS: attach any subset to a chat

A project's active KMS list lives in `.thclaws/settings.json`:

```json
{
  "kms": {
    "active": ["notes", "client-api", "team-playbook"]
  }
}
```

Every active KMS's `index.md` is concatenated into the system prompt under a `## KMS: <name>` heading, each with a pointer to the `KmsRead` / `KmsSearch` tools. The agent sees:

```
# Active knowledge bases

The following KMS are attached to this conversation. Their indices are below —
consult them before answering when the user's question overlaps.

## KMS: notes (user)

# notes
- auth-flow → pages/auth-flow.md — JWT refresh pattern we use
- api-conventions → pages/api-conventions.md — REST style guide

To read a specific page, call `KmsRead(kms: "notes", page: "<page>")`.
To grep all pages, call `KmsSearch(kms: "notes", pattern: "...")`.
```

And `KmsRead` / `KmsSearch` (and the mutating `KmsWrite` / `KmsAppend` / `KmsDelete`) are registered in the tool list. **Several slash commands below require at least one KMS to be active** — without it, KMS tools aren't in the registry and the agent can't act on any KMS by name.

## Slash commands

The full surface, grouped by purpose:

- **Discovery and inspection**: `/kms`, `/kms show`
- **Lifecycle**: `/kms new`, `/kms use`, `/kms off`
- **Capture**: `/kms ingest`, `/kms dump`, `/kms file-answer`
- **Maintenance**: `/kms lint`, `/kms wrap-up`, `/kms reconcile`, `/kms migrate`
- **Decision support**: `/kms challenge`

### `/kms` (or `/kms list`)

List every discoverable KMS; `*` marks ones attached to the current project.

```
❯ /kms
* notes              (user)
  client-api         (project)
* team-playbook      (user)
  archived-docs      (user)
(* = attached to this project; toggle with /kms use | /kms off)
```

### `/kms show NAME`

Print the KMS's `index.md` to inspect what's there.

```
❯ /kms show notes
# notes
- auth-flow → pages/auth-flow.md — JWT refresh pattern we use
- api-conventions → pages/api-conventions.md — REST style guide
...
```

### `/kms new [--project] NAME`

Create a new KMS and seed starter files (including `manifest.json`).

```
❯ /kms new meeting-notes
created KMS 'meeting-notes' (user) → /Users/you/.config/thclaws/kms/meeting-notes

❯ /kms new --project design-decisions
created KMS 'design-decisions' (project) → ./.thclaws/kms/design-decisions
```

- Default scope is **user** (available in every project)
- `--project` puts it in `.thclaws/kms/` (lives with the repo)

### `/kms use NAME`

Attach a KMS to the current project. The `KmsRead` / `KmsSearch` / `KmsWrite` / `KmsAppend` / `KmsDelete` tools are registered into the current session immediately and the `index.md` is spliced into the system prompt — no restart, works in the CLI REPL and either GUI tab.

```
❯ /kms use notes
KMS 'notes' attached (tools registered; available this turn)
```

### `/kms off NAME`

Detach a KMS. Also live — when the last KMS detaches, the KMS tools are dropped from the registry so the model stops seeing them as options.

```
❯ /kms off archived-docs
KMS 'archived-docs' detached (system prompt updated)
```

### `/kms ingest NAME <file-or-url-or-$>`

Add a source. Auto-detects the source type and routes to the right ingest path. Two-step split: raw bytes go to `sources/<alias>.<ext>` (immutable), a stub page lands in `pages/<alias>.md` with frontmatter pointing back at the source. You then enrich the stub via natural prompting or another `/kms ingest --force`.

| Source pattern | Behaviour |
|---|---|
| `<file.md>` / `.txt` / `.json` / `.rst` / `.log` / `.markdown` | Plain text — copy bytes, write stub |
| `<file.pdf>` | Runs `pdftotext` first (requires `poppler-utils` installed locally), then ingest |
| `https://...` URL | HTTP fetch (30s timeout); response body gets a `<!-- fetched from <url> on <date> -->` banner, then ingest |
| `$` | Special — "the current chat session." Triggers an agent turn that summarizes the conversation as a wiki page (200–1500 words, synthesized) and calls `KmsWrite`. Page name resolves from `session.title` (sanitized) or `session.id` (`sess-<hex>`) — see below. |

Optional flags:

- `as <alias>` — override the auto-derived page stem. Useful when the filename or URL produces something ugly.
- `--force` — replace the existing page with the same alias, AND mark all pages whose frontmatter `sources:` references this alias with a `> ⚠ STALE` marker (the **re-ingest cascade**). Pages flagged STALE need refresh against the new source content; `/kms wrap-up` surfaces them.

```
❯ /kms ingest notes ~/Downloads/oauth-spec.pdf
ingested oauth-spec → pages/oauth-spec.md (12 KB extracted)

❯ /kms ingest notes https://example.com/articles/best-practices.html as best-practices
ingested best-practices → pages/best-practices.md (4.2 KB)

❯ /kms ingest notes ~/Downloads/updated-spec.pdf as oauth-spec --force
re-ingested oauth-spec; marked 3 dependent page(s) stale
```

For multi-paragraph paste with no specific source file, `/kms dump` is a better fit.

#### `/kms ingest NAME $` — file the current chat session

Special source target `$` triggers an **agent turn** that summarizes the live conversation. The slash rewrites itself into a structured prompt instructing the agent to:

1. Summarize the conversation as a self-contained wiki page (200–1500 words, synthesized — not transcribed)
2. Call `KmsWrite(kms: "<name>", page: "<page>", content: "...")` with frontmatter `category: session, sources: chat`
3. Confirm with the resolved path

Page name resolves with this precedence:

1. **User-supplied** via `as <alias>` (sanitized to a kebab-case stem)
2. **Session title** if your session has one
3. **Session id** (`sess-<hex>`) as final fallback

Use `--force` to replace if the resolved page already exists.

### `/kms dump NAME <text>`

Capture freeform content and route it. The agent classifies the dump into chunks (one decision, one observation, one new source per chunk), announces its routing plan in plain text, then executes via `KmsWrite` / `KmsAppend`.

> Requires KMS tools — run `/kms use <name>` first if no KMS is attached. Without it the command refuses with a clear error.

```
❯ /kms dump notes Big standup. Decision: defer Redis migration — Tom raised cost
  concerns, Sarah agreed. Win: auth refactor praised by manager. Risk:
  backend cap shrinks next sprint, may push deadline.

(/kms dump notes → routing 198 char(s))

[agent] I'll route this:
- Append to redis-migration.md — decision to defer with Tom's cost rationale
- Append to brag-doc.md — manager praise on auth refactor
- Append to team-capacity.md — backend cap risk for next sprint
- Skip "big standup" header — too generic to file

[KmsAppend ×3 fire]

**Created**: none
**Appended**: redis-migration.md, brag-doc.md, team-capacity.md
**Skipped**: "big standup" — too generic
```

Multi-line paste works in either CLI or GUI. The **announce-then-execute** pattern is built into the prompt: the agent prints its plan before any tool calls so you can ⌃C to abort. Hard rules on the agent: no inventing sources, no `KmsDelete`, every new page must reference at least one existing page (otherwise the chunk gets deferred).

`capture` is an alias for `dump` if it reads more naturally to you.

### `/kms file-answer NAME <title>`

File the latest assistant message in your chat as a new KMS page. Useful when the agent has just produced something worth keeping (a synthesis, a comparison table, a debugging recap) and you want it in the wiki rather than scrolling chat history later. Aliases: `file`.

```
❯ /kms file-answer notes oauth-debugging-recap
filed answer → /Users/you/.config/thclaws/kms/notes/pages/oauth-debugging-recap.md (1428 bytes)
```

Page name is `<title>` sanitized to a stem. Frontmatter pre-set to `category: answer, filed_from: chat`. Body is the latest assistant message verbatim under an H1 with the title.

### `/kms lint NAME`

Pure-read health check. Walks `pages/` and reports six categories of issue: broken markdown links to other pages, pages with no inbound links (orphans), index entries pointing at missing files, pages on disk missing from the index, pages without YAML frontmatter, and (when `manifest.json` declares `frontmatter_required`) missing required fields per page category.

```
❯ /kms lint notes
KMS 'notes': 3 issue(s)

broken links (1):
  - oauth-flow → pages/sso-config.md (missing)

pages missing from index (1):
  - tracing-conventions

missing required frontmatter fields (1):
  - paper-x: 'sources' (required by research)
```

`/kms lint` aliases: `/kms check`, `/kms doctor`.

### `/kms wrap-up NAME [--fix]`

Session-end review. Combines lint with a scan for stale-marker pages — pages flagged by the re-ingest cascade (`> ⚠ STALE: source <alias> was re-ingested on YYYY-MM-DD`) that are awaiting a refresh against the new source content.

```
❯ /kms wrap-up notes
KMS 'notes': wrap-up — 3 lint issue(s), 1 stale marker(s)

broken links (1):
  - oauth-flow → pages/sso-config.md (missing)

stale pages awaiting refresh (1):
  - summary: source `topic` re-ingested on 2026-05-08 (page not yet refreshed)

next steps: ask the agent to refresh stale pages and fix lint issues, or run `/kms lint <name>` again after edits.
```

`--fix` dispatches the built-in **`kms-linker`** subagent (see "Maintenance subagents" below) to act on the report — search for the intended target of broken links, append missing index bullets, refresh stale pages from their sources. Hard rules: no inventing, no deletion, leaves orphans alone (often intentional). GUI-only — the CLI prints the report and tells you to invoke from the GUI.

> Requires KMS tools — run `/kms use <name>` first. The `--fix` branch refuses with a clear error if no KMS is attached, since the subagent inherits the parent's tool registry and would otherwise spawn with no usable tools.

### `/kms reconcile NAME [<focus>] [--apply]`

Auto-resolve contradictions. Dispatches the built-in **`kms-reconcile`** subagent which runs four passes (claims / entities / decisions / source-freshness), classifies each finding (clear-winner / ambiguous / evolution), and either rewrites the outdated page with a `## History` section or creates a `Conflict — <topic>.md` page for genuinely-ambiguous cases. Dry-run by default; `--apply` executes writes. Optional second positional arg narrows the pass to a specific topic. GUI-only.

> Requires KMS tools — run `/kms use <name>` first if no KMS is attached.

```
❯ /kms reconcile notes
✓ kms-reconcile dispatched (id: side-7e2a, dry-run)

[subagent reports back]

**Auto-resolved (3):**
- `oauth-flow.md`: "tokens expire 15min" → "tokens expire 30min" (newer source 2026-04 supersedes 2025-09)
- `team-sarah-chen.md`: role updated from "Eng Lead" to "Director" per Q2 standup notes
- `redis-config.md`: cite `redis-2026-spec.md` instead of `redis-2025-spec.md`

**Flagged for user (1) — Conflict pages would be created:**
- `Conflict — auth-token-rotation.md`: paper-x says rotate every 24h, paper-y says
  every 7d. Both peer-reviewed. Needs human judgment.

**Stale pages updated (2):**
- `architecture-overview.md`: now cites `2026-arch-rfc.md` (was `2025-arch-rfc.md`)
- `db-migrations.md`: same

this was a dry-run preview. re-run with `--apply` to execute.
```

`kms-reconcile`'s tool whitelist is **strictly narrower than `dream`** — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite` only. No `KmsDelete` (reconcile preserves every original claim, either in `## History` on the rewritten page or in the Conflict page). Hard rules: never invent dates or sources; "someone changed their mind" classifies as Evolution, not contradiction.

### `/kms migrate NAME [--apply]`

Schema migration. Defaults to dry-run (prints the plan without writing); pass `--apply` to execute. Idempotent — running on a KMS already at the latest version reports `already at schema version X — nothing to migrate`.

```
❯ /kms migrate legacy-notes
KMS 'legacy-notes': migration plan (0.x → 1.0, 1 step(s))

0.x → 1.0:
  - write /Users/you/.config/thclaws/kms/legacy-notes/manifest.json (schema_version: 1.0, frontmatter_required: empty)

this was a dry-run preview. re-run with `--apply` to execute.

❯ /kms migrate legacy-notes --apply
KMS 'legacy-notes': migration applied (0.x → 1.0, 1 step(s))

0.x → 1.0:
  - write /Users/you/.config/thclaws/kms/legacy-notes/manifest.json (schema_version: 1.0, frontmatter_required: empty)

logged to log.md. /kms lint to verify.
```

When schema changes ship in a future release, `/kms migrate` chains through every step from your current version to the latest. The current 0.x → 1.0 step only writes `manifest.json`; no page bodies are touched.

### `/kms challenge NAME <idea>`

Pre-decision red-team. Given an idea or plan, the agent searches the KMS for past failures, reversed decisions, and contradictions on the topic, then produces a structured Red Team analysis citing specific pages. Read-only — no writes. Aliases: `redteam`.

> Requires KMS tools — run `/kms use <name>` first if no KMS is attached.

```
❯ /kms challenge notes I should ship the auth refactor this week without
  the new test harness in place.

[agent searches across the KMS]

**Your position:** Ship auth refactor this week without the new test harness.

**Counter-evidence from your vault:**
- `incident-2026-01-12` (date: 2026-01-12): "Auth incident traced to insufficient
  integration test coverage. Decision: never ship auth changes without the test harness."
- `1-1-Sarah-2026-04-08` (date: 2026-04-08): Sarah explicitly flagged "ship-without-tests
  is a recurring pattern that bites you every quarter."

**Blind spots:** You may be discounting the integration test gap because the unit
tests pass. Past auth incidents in your vault show the failure mode is at the
integration boundary.

**Verdict:** The vault suggests caution. Past incidents and a recent 1:1 both
point to the same risk. Recommend at minimum a manual smoke pass before merge.
```

The agent's prompt explicitly tells it "don't be agreeable" — push back when the vault gives ammunition. The output is a written analysis, not vault writes; nothing is filed.

## Schema versioning and frontmatter rules

`manifest.json` is the KMS's machine-readable schema. New KMSes get one automatically:

```json
{
  "schema_version": "1.0",
  "frontmatter_required": {}
}
```

Two things live here:

- **`schema_version`** — anchors `/kms migrate`. When thClaws ships a schema change, the migrator detects your current version from this field and walks the chain up to the latest.
- **`frontmatter_required`** — optional enforcement. Empty by default; edit it to declare which YAML frontmatter fields each page category must have. `global` applies to every page; per-category keys apply only to pages whose `category:` field matches.

```json
{
  "schema_version": "1.0",
  "frontmatter_required": {
    "global": ["category", "tags"],
    "research": ["sources"]
  }
}
```

`/kms lint` reports violations:

```
missing required frontmatter fields (1):
  - paper-x: 'sources' (required by research)
```

Pages without any frontmatter at all are flagged separately under `pages without YAML frontmatter` and skipped from per-field checks — one fix at a time.

Legacy KMSes (created before manifests existed) have no `manifest.json` and silently skip the per-field check. Run `/kms migrate <name> --apply` to bring them to v1.0; the migration is purely additive (writes the manifest file, doesn't touch pages).

## Sidebar (GUI)

The sidebar's **Knowledge** section lists every discoverable KMS with a checkbox per entry. Tick to attach, untick to detach — the same underlying toggle as `/kms use` / `/kms off`.

The `+` button prompts for a name, then asks for scope (OK = user, Cancel = project). A new KMS is created with starter files ready to edit.

## Tools the agent calls

### `KmsRead(kms: "name", page: "slug")`

Reads `<kms_root>/pages/<slug>.md`. The `.md` extension is added if missing. Path traversal is rejected (`..`, absolute paths, anything outside `pages/`).

The agent calls this after spotting a relevant entry in `index.md`:

```
[assistant] I'll check the auth-flow page first…
[tool: KmsRead(kms: "notes", page: "auth-flow")]
[result] (page content)
```

### `KmsSearch(kms: "name", pattern: "regex")`

Grep-style scan across `<kms_root>/pages/*.md`. Returns matching lines as `page:line:text`, one per line.

```
[assistant] Let me search for "bearer" across my notes…
[tool: KmsSearch(kms: "notes", pattern: "bearer")]
[result]
auth-flow:12:Bearer tokens expire after 15 minutes
api-conventions:34:Always include "Authorization: Bearer <token>"
```

### `KmsWrite`, `KmsAppend`, `KmsDelete`

The mutation surface used by the agent (and by the `/dream` consolidator below). All three require approval by default.

- `KmsWrite(kms, page, content)` — create-or-replace a page. Preserves YAML frontmatter, bumps `updated:`, refreshes the `index.md` bullet, appends a `wrote | <page>` entry to `log.md`.
- `KmsAppend(kms, page, content)` — extend a page in place. Faster than `KmsWrite` for incremental updates (logs, journal entries, accumulated notes). Bumps `updated:` if the page has frontmatter.
- `KmsDelete(kms, page)` — remove a page, prune its `index.md` bullet, append `deleted | <page>` to `log.md`. Used during consolidation to retire duplicates or stale entries.

Page names are validated path-segments — no separators, no traversal, and the reserved names `index`, `log`, `SCHEMA` cannot be used as a page name (they're managed by the KMS itself).

## Maintenance subagents

Three built-in subagents handle KMS upkeep. They run as side channels (Chapter 15) — concurrent agents in their own context windows, so the heavy walking doesn't pollute your main conversation.

| Agent | Trigger | Scope | When to use |
|---|---|---|---|
| `dream` | `/dream` | All active KMSes | Periodic deep consolidation — mines recent sessions, dedupes pages, restructures |
| `kms-linker` | `/kms wrap-up <name> --fix` | One KMS, one report | Targeted fixes — acts on a concrete lint + stale-marker report |
| `kms-reconcile` | `/kms reconcile <name> [--apply]` | One KMS | Auto-resolves contradictions across pages — rewrites with `## History`, flags ambiguous as Conflict pages |

> All three need at least one KMS in `kms_active` so the KMS tools register before the subagent spawns. Run `/kms use <name>` first; without an active KMS the dispatch refuses with a clear error rather than spawning a tool-less subagent.

You can also run these on a schedule via [Chapter 19's pre-packaged presets](ch19-scheduling.md) — `nightly-close`, `weekly-review`, `contradiction-sweep`, `vault-health`. Note that scheduled fires use natural-language tool directives (not slash commands) because the daemon fires via `thclaws --print` which doesn't run slash dispatch.

### Broad consolidation: `/dream`

After a few weeks of work, your KMS accumulates duplicates: two pages on the same topic that drifted apart, an old entry contradicted by something you said yesterday, insights from sessions that never made it into a page. **`/dream`** is the slash command that fixes that — it dispatches a built-in `dream` agent as a side channel that consolidates the project's KMS in the background while you keep working.

```
/dream                 # consolidate everything
/dream auth            # bias the consolidation toward "auth"
/agents                # see the active dream + when it started
/agent cancel <id>     # stop a dream that's wandering
```

`/dream` is GUI-only (it needs the chat surface to render the side bubble). The dream agent runs concurrently with main, so you can keep prompting your main agent while it works.

#### What it does

The dream agent runs four passes:

1. **Survey** — reads the active KMS list (from its system prompt) and the `index.md` of each KMS to enumerate existing pages.
2. **Read sessions** — `Glob`s the 10 most recently modified files under `.thclaws/sessions/*.jsonl` and reads them. Each session is a JSONL of message events; the agent skims for stable facts the user worked through that aren't already in KMS.
3. **Consolidate** — for each insight, it `KmsSearch`es the relevant KMS first; if a page covers the topic, it `KmsAppend`s rather than creating a new page. If two pages overlap heavily, it merges via `KmsWrite` and `KmsDelete`s the duplicate.
4. **Summarize** — writes a `dream-YYYY-MM-DD.md` page in the project KMS listing every change (pages added, updated, deleted, plus skipped insights and reasons). This is your audit trail.

```
❯ /dream
✓ dreaming (id: side-9c4f1e)

[dream] surveying 2 active KMS (project-knowledge, scratch)…
[dream] reading 10 most recent sessions…
[dream] consolidating project-knowledge:
[dream]   appended 4 lines to auth-flow.md
[dream]   merged old-deployment.md into deployment.md, deleted old-deployment.md
[dream]   added 2 new pages: tracing-conventions.md, kafka-topics.md
[dream] writing dream-2026-05-07.md…
[dream] ✓ done in 3m12s. See dream-2026-05-07.md for the change log.
```

#### Reviewing the changes

The dream agent runs with `permission_mode: auto` — it edits and deletes pages without prompting you. **The review step is `git diff`.** If your project KMS lives under git (which it should — `.thclaws/kms/` is just markdown):

```bash
git diff .thclaws/kms/                        # see what changed
git checkout -- .thclaws/kms/                 # discard the dream's work
git add .thclaws/kms/ && git commit -m "..."  # accept it
```

The `dream-YYYY-MM-DD.md` summary page is the agent's own narration of what it did — read that first, then spot-check the diffs that matter. If the summary says "no new insights" and writes a stub page, that's a valid no-op outcome.

#### Customizing

The built-in dream agent is shipped inside the binary (its system prompt + tool whitelist). You can override it project-wide by creating `.thclaws/agents/dream.md` with your own frontmatter and instructions — the disk version always wins over the built-in. Use this if your team has a specific KMS curation policy (e.g. "never delete pages tagged `archive: keep`").

The default agent uses tools `KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete, Read, Glob, Grep, TodoWrite` — no `Bash`, no project-source `Edit`/`Write`, no `Memory*` tools. It can only modify the KMS.

### Targeted fixes: `kms-linker`

Where `/dream` is the broad pass over all active KMSes, **`kms-linker`** is the narrow one — it acts on a single concrete lint report from `/kms wrap-up <name> --fix`. Different rhythms:

- `/dream` is *exploratory*: mines sessions for new content, restructures pages, dedupes. Best run periodically (weekly, end-of-sprint).
- `/kms wrap-up --fix` is *closing the loop*: hand it the lint+stale findings and it patches what's straightforwardly fixable. Best run at session end before stepping away.

The agent's operating procedure (encoded in its prompt):

| Lint category | Action |
|---|---|
| Broken link `(page → target)` | `KmsSearch` for the target stem; if exactly one strong match, rewrite the link, otherwise defer |
| Stale page `(stem, source, date)` | `KmsRead` the source's stub page and the stale page; rewrite the stale page preserving structure, drop the `> ⚠ STALE` line |
| Missing-in-index page | `KmsAppend` a one-line bullet to `index.md` under the matching category section |
| Missing required field | Only fill if derivable from page body or sources; otherwise defer |
| Orphan page | Don't act — orphans often exist for good reason. List in the final report |

The final message follows a fixed contract — `**Fixed**` block listing every change, `**Skipped (need human judgment)**` block listing what was left for you. Hard rules same as dream: no `KmsDelete`, no inventing sources. Tool whitelist is strictly narrower than dream — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite` only — because `kms-linker` works only on what wrap-up handed it, never reads sessions or external files.

Override with `.thclaws/agents/kms-linker.md` if your team needs different policy.

### Auto-reconcile: `kms-reconcile`

A third subagent that operates on contradictions rather than lint findings. Where `kms-linker` fixes broken links and stale markers from `/kms wrap-up`, **`kms-reconcile`** runs four parallel passes to detect contradictions, classifies each, and resolves them with full history preservation.

The four passes (encoded in the agent's prompt):

| Pass | Detects |
|---|---|
| Claims | Concept and project pages with overlapping factual claims that disagree |
| Entities | Entity pages where role, company, title, or relationship has drifted |
| Decisions | Decision pages contradicted by later pages without a `supersedes:` link |
| Source-freshness | Wiki pages citing old sources when newer sources on the same topic exist in the KMS |

Per finding, the agent classifies as:

- **Clear winner** — newer + more authoritative side rewrites the older page; an `## History` section preserves what changed and why
- **Genuinely ambiguous** — both sides have evidence, neither clearly authoritative; a `Conflict — <topic>.md` page with `status: open` is created with both positions documented
- **Evolution** — not a contradiction; the user changed their mind, treated as growth via a `## Timeline` section

Tool whitelist matches `kms-linker` — `KmsRead, KmsSearch, KmsWrite, KmsAppend, TodoWrite`. **No `KmsDelete`** (reconcile preserves every original claim, either in `## History` or in the Conflict page). Override with `.thclaws/agents/kms-reconcile.md` if your team needs different policy (e.g., "always create Conflict pages instead of auto-resolving").

`/kms reconcile` defaults to dry-run; `--apply` executes writes. Optional second positional arg narrows the pass to a topic or entity.

## Vault artifacts you'll see

Subagents and slash commands write specific patterns into your KMS. When you find these in your pages, here's what wrote them and what they mean:

| Artifact | Written by | Meaning |
|---|---|---|
| `## History` section appended to a page | `kms-reconcile` (clear-winner classification) | Page was rewritten with newer info; the History block preserves the previous claim and the reason for the update |
| `## Timeline` section appended to a page | `kms-reconcile` (evolution classification) | User's thinking on this topic changed over time; Timeline shows the chronological progression |
| `Conflict — <topic>.md` page with `status: open` | `kms-reconcile` (ambiguous classification) | Two pages disagreed but neither was clearly authoritative; the Conflict page captures both positions for human judgment |
| `> ⚠ STALE: source ...` line in a page body | `mark_dependent_pages_stale` after re-ingest cascade | Source was re-ingested with `--force`; this page references it via frontmatter `sources:` and needs a refresh |
| `dream-YYYY-MM-DD.md` page | `/dream` consolidation pass | Audit trail of one dream session — what was added, updated, deleted, with reasons |
| Stub page in `pages/<alias>.md` ending with `_Replace this stub with a curated summary..._` | `/kms ingest` (file/URL/PDF) | Raw source landed in `sources/<alias>.<ext>`; this stub points at it. Enrich via natural prompting or `KmsWrite`. |
| `## [date] verb \| <alias>` line in `log.md` | All KMS write operations | Append-only change log. Greppable: `grep "^## \[" log.md \| tail -20` for recent activity. |

## Browse, graph, and HTML export (v0.8.5+)

KMS gained three browse-time surfaces in v0.8.5 — all in the Desktop GUI, none change the underlying file format.

### KMS browser sidebar

Click the title of any KMS in the left sidebar (not the checkbox) and a 260-px panel slides in on the right edge listing every page and source archive. Clicking a file opens the in-app viewer over the main content tab. Tabs underneath stay mounted so xterm / chat state is preserved. Closing the browser, switching tabs, or hitting `ESC` returns you to the active tab.

The viewer renders Markdown via `marked`, with custom CSS for editorial-style typography: heading borders, accent-tinted blockquotes, bordered tables with zebra stripes, and three link styles — external (solid underline), internal `[[wikilinks]]` (dotted underline + accent pill), and inline citation chips `[N]` (small rounded pill).

### Obsidian-style graph view

The browser sidebar has a "Graph View" button above the page list. Clicking it replaces the main pane with a force-directed graph: pages are circles, `[[wikilinks]]` are edges, and an "Include sources" checkbox (default on) adds source archives as muted diamond nodes connected to the pages that cite them.

- Drag empty space to pan; mouse wheel to zoom around the cursor.
- Drag a node to reposition it — it pins to the mouse, neighbors react via spring forces.
- Click a node to open the file in the viewer.
- Hover highlights the node + its connected neighbors; everything else dims.
- Force simulation auto-stops once the layout settles (annealed damping; cap ~3 s).

### `/kms html NAME [OUT]` — single-file interactive site

Generates a self-contained HTML site from a KMS's pages and writes it to your workspace (default `./<NAME>-site/index.html`). Unlike the in-app viewer, this is a **derived artifact** you can share, commit to git, host on S3, or hand to colleagues — it has no thClaws dependency.

The agent runs a three-phase workflow:

1. **Explore** — `KmsRead` an index/manifest if one exists, then `KmsSearch` to enumerate page slugs. Reads 4–8 representative pages to understand the content style. Source files stay closed by default — citations alone don't justify reading.
2. **Design components** — sketches a component vocabulary in chat as plain prose: site shell, page reader, navigation, citation chip, wikilink chip, etc. Picks design tokens (typography, accent color, dark mode). Prints the sketch so you can ⌃C if it's going off course.
3. **Assemble** — reads remaining pages, writes one `index.html` containing inlined `<style>`, inlined `<script>` with hash-routing for multi-view (`#/`, `#/page/<slug>`), and a JSON data island with every page body + frontmatter. Citations render as plain `[N]` markers; pages are the substance of the site.

Hard rules the prompt enforces:

- Single file. No external CSS/JS, no CDN, no `fetch`. Double-click-to-open works offline.
- Pages are primary content; source bodies are NOT embedded.
- Multi-view via hash routing so deep links work.
- Plain HTML + CSS + vanilla JS — no frameworks.

Customize the output dir with the second positional argument:

```
/kms html llm-wiki              # → ./llm-wiki-site/index.html
/kms html llm-wiki ../shareable # → ../shareable/index.html (relative to cwd)
/kms html llm-wiki /tmp/site    # → /tmp/site/index.html
```

The agent uses your active session model (commonly Opus / GPT-4.1 / Sonnet 4.6 — long-context models work best because the agent embeds every page body in the JSON island).

## Scaling limits and future direction

KMS is intentionally embedding-free:

- Grep is fast enough up to a few hundred pages
- The `index.md`-first pattern means the agent can usually find relevant pages without searching
- Pages are markdown and human-readable — you can browse them without any tooling

When a KMS grows past ~200 pages or includes non-English content that grep won't cross-match cleanly, hybrid RAG (BM25 + vector + LLM rerank via [`qmd`](https://github.com/tobi/qmd)) is on the roadmap as an opt-in fallback. The client API stays the same.

## Thai-language notes

Grep over Thai works out of the box because the retrieval is substring-based, not tokenized. Your agent can search `"การยืนยันตัวตน"` across Thai notes and get results without any setup.

For mixed Thai/English technical content, stick with English tech terms and Thai prose in the same page — both will hit on relevant searches.

## Troubleshooting

- **"no KMS attached to this session"** — `/kms challenge`, `/kms dump`, `/kms reconcile`, and `/kms wrap-up --fix` need at least one KMS in `kms_active` so KMS tools register. The error message names the target KMS — run `/kms use <name>` first.
- **KMS not visible in sidebar** — make sure the folder has a valid `index.md` (create one manually if you've built the KMS by hand) and that it lives in `~/.config/thclaws/kms/` or `.thclaws/kms/`.
- **Changes not reflected in agent responses** — the `index.md` is read on turn start; a running turn uses the snapshot taken before it began. Start a new turn.
- **"no KMS named 'X'"** error from a tool call — the name is case-sensitive and must match the directory name exactly. Check with `/kms list`.
- **Stale active list** — `.thclaws/settings.json` is the source of truth. Edit by hand if the sidebar checkboxes ever disagree with reality.
- **`/kms wrap-up --fix` says "nothing actionable"** — the fix subagent skips dispatch when only orphan pages and missing-frontmatter issues exist (those need human judgment, not mechanical fixes). Address those manually.
- **Scheduled preset fires but nothing happens** — preset prompts are natural-language directives, not slash commands. The cwd's `.thclaws/settings.json` must have the target KMS in `kms_active` so KMS tools register before the agent starts. See [Chapter 19](ch19-scheduling.md).

## Where to go next

- [Chapter 8](ch08-memory-and-agents-md.md) — memory and project instructions (the other two context mechanisms)
- [Chapter 10](ch10-slash-commands.md) — slash command reference including `/kms` family
- [Chapter 11](ch11-built-in-tools.md) — tool reference including `KmsRead` and `KmsSearch`
- [Chapter 15](ch15-subagents.md) — subagents and side channels (deeper dive on `dream`, `kms-linker`, `kms-reconcile`)
- [Chapter 19](ch19-scheduling.md) — scheduling, including pre-packaged KMS-maintenance presets (`nightly-close`, `weekly-review`, `contradiction-sweep`, `vault-health`)
