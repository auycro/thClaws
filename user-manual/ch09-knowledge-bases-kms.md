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
├── index.md      ← table of contents, one line per page. The agent reads this every turn.
├── log.md        ← append-only change log (humans + agent write here)
├── SCHEMA.md     ← optional: shape rules for pages
├── pages/        ← individual wiki pages, one per topic
│   ├── auth-flow.md
│   ├── api-conventions.md
│   └── troubleshooting.md
└── sources/      ← raw source material (URLs, PDFs, notes) — optional
```

`/kms new` seeds all of the above with minimal starter content so you can start writing immediately.

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
- [auth-flow](pages/auth-flow.md) — JWT refresh pattern we use
- [api-conventions](pages/api-conventions.md) — REST style guide

To read a specific page, call `KmsRead(kms: "notes", page: "<page>")`.
To grep all pages, call `KmsSearch(kms: "notes", pattern: "...")`.
```

And `KmsRead` / `KmsSearch` are registered in the tool list.

## Slash commands

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

### `/kms new [--project] NAME`

Create a new KMS and seed starter files.

```
❯ /kms new meeting-notes
created KMS 'meeting-notes' (user) → /Users/you/.config/thclaws/kms/meeting-notes

❯ /kms new --project design-decisions
created KMS 'design-decisions' (project) → ./.thclaws/kms/design-decisions
```

- Default scope is **user** (available in every project)
- `--project` puts it in `.thclaws/kms/` (lives with the repo)

### `/kms use NAME`

Attach a KMS to the current project. The `KmsRead` / `KmsSearch` tools
are registered into the current session immediately and the
`index.md` is spliced into the system prompt — no restart, works in
the CLI REPL and either GUI tab.

```
❯ /kms use notes
KMS 'notes' attached (tools registered; available this turn)
```

### `/kms off NAME`

Detach a KMS. Also live — when the last KMS detaches, the `KmsRead` /
`KmsSearch` tools are dropped from the registry so the model stops
seeing them as options.

```
❯ /kms off archived-docs
KMS 'archived-docs' detached (system prompt updated)
```

### `/kms show NAME`

Print the KMS's `index.md` to inspect what's there.

```
❯ /kms show notes
# notes
- [auth-flow](pages/auth-flow.md) — JWT refresh pattern we use
- [api-conventions](pages/api-conventions.md) — REST style guide
...
```

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

## Consolidation: the `/dream` workflow

After a few weeks of work, your KMS accumulates duplicates: two pages on the same topic that drifted apart, an old entry contradicted by something you said yesterday, insights from sessions that never made it into a page. **`/dream`** is the slash command that fixes that — it dispatches a built-in `dream` agent as a side channel (Chapter 15) that consolidates the project's KMS in the background while you keep working.

```
/dream                 # consolidate everything
/dream auth            # bias the consolidation toward "auth"
/agents                # see the active dream + when it started
/agent cancel <id>     # stop a dream that's wandering
```

`/dream` is GUI-only (it needs the chat surface to render the side bubble). The dream agent runs concurrently with main, so you can keep prompting your main agent while it works.

### What it does

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

### Reviewing the changes

The dream agent runs with `permission_mode: auto` — it edits and deletes pages without prompting you. **The review step is `git diff`.** If your project KMS lives under git (which it should — `.thclaws/kms/` is just markdown):

```bash
git diff .thclaws/kms/                        # see what changed
git checkout -- .thclaws/kms/                 # discard the dream's work
git add .thclaws/kms/ && git commit -m "..."  # accept it
```

The `dream-YYYY-MM-DD.md` summary page is the agent's own narration of what it did — read that first, then spot-check the diffs that matter. If the summary says "no new insights" and writes a stub page, that's a valid no-op outcome.

### Customizing

The built-in dream agent is shipped inside the binary (its system prompt + tool whitelist). You can override it project-wide by creating `.thclaws/agents/dream.md` with your own frontmatter and instructions — the disk version always wins over the built-in. Use this if your team has a specific KMS curation policy (e.g. "never delete pages tagged `archive: keep`").

The default agent uses tools `KmsRead, KmsSearch, KmsWrite, KmsAppend, KmsDelete, Read, Glob, Grep, TodoWrite` — no `Bash`, no project-source `Edit`/`Write`, no `Memory*` tools. It can only modify the KMS.

## Writing pages: the ingest workflow

You don't need a special tool to add content — the agent writes markdown like it writes any other file. A typical ingest turn looks like:

```
❯ I just read https://example.com/oauth-guide. Ingest the key points into 'notes'.

[assistant] Reading the page…
[tool: WebFetch(url: "https://example.com/oauth-guide")]
[tool: Write(path: "~/.config/thclaws/kms/notes/pages/oauth-client-credentials.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/index.md", ...)]
[tool: Edit(path: "~/.config/thclaws/kms/notes/log.md", ...)]
Wrote pages/oauth-client-credentials.md, added entry to index.md, appended to log.md.
```

Karpathy's gist describes the workflow as three operations:

1. **Ingest** — read a source, extract distinct facts, write a page, update the index, append to the log
2. **Query** — answer a question from the wiki (the agent does this naturally when the KMS is attached)
3. **Lint** — periodically read all pages and suggest merges, splits, or orphans to fix

You run these via natural language; no special slash commands needed.

## Scaling limits and future direction

v0.2.x is intentionally embedding-free:

- Grep is fast enough up to a few hundred pages
- The `index.md`-first pattern means the agent can usually find relevant pages without searching
- Pages are markdown and human-readable — you can browse them without any tooling

When a KMS grows past ~200 pages or includes non-English content that grep won't cross-match cleanly, you can upgrade to hybrid RAG (hosted OpenAI embeddings) — planned for a future release. The client API stays the same.

## Thai-language notes

Grep over Thai works out of the box because the retrieval is substring-based, not tokenized. Your agent can search `"การยืนยันตัวตน"` across Thai notes and get results without any setup.

For mixed Thai/English technical content, stick with English tech terms and Thai prose in the same page — both will hit on relevant searches.

## Troubleshooting

- **KMS not visible in sidebar** — make sure the folder has a valid `index.md` (create one manually if you've built the KMS by hand) and that it lives in `~/.config/thclaws/kms/` or `.thclaws/kms/`.
- **Changes not reflected in agent responses** — the `index.md` is read on turn start; a running turn uses the snapshot taken before it began. Start a new turn.
- **"no KMS named 'X'"** error from a tool call — the name is case-sensitive and must match the directory name exactly. Check with `/kms list`.
- **Stale active list** — `.thclaws/settings.json` is the source of truth. Edit by hand if the sidebar checkboxes ever disagree with reality.

## Where to go next

- [Chapter 8](ch08-memory-and-agents-md.md) — memory and project instructions (the other two context mechanisms)
- [Chapter 10](ch10-slash-commands.md) — slash command reference including `/kms` family
- [Chapter 11](ch11-built-in-tools.md) — tool reference including `KmsRead` and `KmsSearch`
