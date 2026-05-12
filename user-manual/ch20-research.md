# Chapter 20 — Background research (`/research`)

`/research <query>` spawns a background job that:

1. searches the web,
2. iterates with the LLM on what's still missing,
3. groups findings into multiple cross-linked KMS pages,
4. caches every cited source as a local Markdown file.

The result lives in your knowledge base as a permanent artifact —
searchable across sessions, citable from chat, editable like any
other KMS page.

## Quick start

```
> /research what is the LangGraph agent strategy in local-deep-research
[research started: id=research-a3f1c2] query: what is the LangGraph …
  /research status research-a3f1c2     check progress
  /research show research-a3f1c2       stream result
  /research cancel research-a3f1c2     cancel
```

A live "Research" panel appears on the right edge of the GUI showing
phase, iteration progress, and per-iteration scores. CLI users see a
one-line completion announcement on the next prompt:

```
[research done: id=research-a3f1c2 → langgraph-agent-strategy/2026-05-09-…__concept.md]
```

After completion the target KMS auto-attaches to the session, so a
follow-up question like *"summarize the LangGraph approach"* triggers
the LLM to call `KmsRead` on the freshly-written pages instead of
answering from training data.

## Slash subcommands

```
/research <query>                          start a new run (default config)
/research [flags...] <query>               start with overrides
/research                                  list all jobs (newest first)
/research list                             same
/research status <id>                      detailed view (phase, iter, score)
/research show <id>                        print the synthesized page in chat
/research cancel <id>                      signal cancel; partial result discarded
/research wait <id>                        block CLI prompt until job is terminal
```

### Flags on the start path

| Flag | Default | What it does |
|---|---|---|
| `--kms <name>` | auto-derive from query | Target KMS. Auto-derived names use the LLM-extracted topic slug (e.g. `obon-festival`) — no `research-` prefix. |
| `--min-iter N` | 2 | Hard floor — pipeline runs at least this many iterations even if the LLM scores it complete earlier. |
| `--max-iter K` | 8 | Hard ceiling. |
| `--score-threshold 0.X` | 0.80 | Score the LLM evaluator must produce (0.0-1.0) to short-circuit between min and max. Accepts decimal (`0.85`) or integer percent (`85`). Bumped from 0.75 → 0.80 — LLMs tend to score generously after iter 2 and the old default cut research short before subtopic coverage stabilised. |
| `--max-pages N` | 7 | Cap on KMS pages emitted per run. *Ceiling, not target* — narrow queries produce 1-2 pages. |
| `--budget-time SEC|2m|1h` | 15m | Wall-clock budget. Past this, the job ends as `Failed` with a budget-exhausted message. |

## What ends up in the KMS

A `/research` run produces this layout inside the target KMS:

```
<kms-name>/
├── pages/
│   ├── <YYYY-MM-DD>-<query>__<page-slug>.md    ← one per page in the plan
│   ├── <YYYY-MM-DD>-<query>__<page-slug>.md
│   └── _summary.md                              ← per-run section index
├── sources/
│   ├── <url-slug>.md                            ← cached fetch body per cited URL
│   └── <url-slug>.md
├── index.md                                     ← auto-managed
├── log.md
└── SCHEMA.md
```

### Pages

Each page covers one coherent topic — an entity (person, paper,
organization), a concept, a comparison (X vs Y), a how-to, or a
timeline. Pages cross-link with Obsidian-style `[[slug]]` wikilinks
that work in Obsidian, GitHub, and `KmsRead`.

Every page leads with a 1-2 sentence abstract that becomes the KMS
index summary, followed by `##` subtopic sections, inline `[N]`
citations, and an auto-generated `## Sources` section listing each
cited source with a clickable link to its cached copy:

```markdown
LLM Wiki is a local, personal knowledge base where an LLM and the
user co-author markdown notes that compound over time, popularized
by [[andrej-karpathy|Andrej Karpathy]] [1](../sources/x-com-karpathy-status.md).

## Origins

The pattern emerged in early 2026 when …

## Sources

1. [Karpathy on LLM Wiki](../sources/x-com-karpathy-status.md) — https://x.com/karpathy/status/…
2. [Comparing LLM Wiki vs RAG](../sources/medium-com-llm-wiki-vs-rag.md) — https://medium.com/…
```

### Verify pass — guard against hallucinated citations

After each page is synthesised, the pipeline runs a **verify pass** — a separate LLM call that audits the generated page against its cited sources. It walks every factual claim and decides:

- `Supported` — the cited source clearly states this exact claim
- `Partial` — the source touches the topic but doesn't strictly back the specific wording
- `Unsupported` — the citation is wrong (the source does NOT say this); usually a hallucination or miscitation
- `NoCitation` — the page makes a factual assertion with no `[N]` attached

The pass writes two artifacts:

1. **`verification_score: 0.85`** in the page's frontmatter — fraction of factual claims rated `Supported`. Sort or filter your KMS by this field to spot pages that need re-checking.
2. **`## Verification` section** appended to the page body — listed ONLY for the flagged items (Partial / Unsupported / NoCitation). Each item shows the icon (🚫 / ⚠️ / ❓), the verdict, the cited `[N]`, the paraphrased claim, and the verifier's note:

```markdown
## Verification

Auto-verification pass found 2 claim(s) that don't strictly match their cited source. Review before relying on the page for downstream decisions.

- 🚫 **unsupported** [3]: X is 100x faster than Y — _[3] says faster but not "100x"_
- ❓ **no citation** (uncited): Y was released in 2024
```

Pages where every claim is `Supported` get the `verification_score` in frontmatter but no `## Verification` section — clean pages stay clean. If the verifier itself fails (parse error, provider timeout), the page is written without a `verification_score` (a missing field is honest — a fabricated 0.0 would be misleading), and the run continues.

**Why this matters** — the well-known critique of LLM-Wiki ("organised persistent mistakes") points at synthesisers that hallucinate facts then cite a real source that doesn't back them. The verify pass catches this class before the page lands in your KMS. Cost: ~+25% of total `/research` LLM cost for a typical 4-page run; soft-fails so it never aborts the pipeline.

### Sources

Every cited URL gets a cached copy in `<kms>/sources/<url-slug>.md`
with frontmatter that includes the original URL, citation index, and
fetch date. Filename is a deterministic slug of the URL — same URL
across multiple research runs maps to the same file, so the archive
stays bounded.

When `HAL_API_KEY` is configured (Settings → Providers → Service
keys → HAL Public API), `/research` fetches via HAL's headless
browser scrape — clean Markdown including code blocks, tables, and
nested lists. Without the key, it falls back to `WebFetch`'s direct
HTML→Markdown conversion (rougher but still usable).

### Run summary

`pages/_summary.md` accumulates one section per `/research` run:

```markdown
## 2026-05-09 — what is the LangGraph agent strategy

- [[2026-05-09-langgraph-agent__concept-overview|Concept overview]] — Core idea …
- [[2026-05-09-langgraph-agent__research-subtopic-tool|research_subtopic tool]] — Parallel fanout …
- [[2026-05-09-langgraph-agent__rag-comparison|vs RAG]] — Differences in …
```

Wikilinks render natively in Obsidian; in GitHub web view or
`KmsRead` the run-prefixed filenames let you open them by file.

## Live progress visibility

### GUI — right-edge sidebar

A "Research" panel mirrors the Plan / Todo sidebars on the right
edge:

- **Phase** — current step (`iteration 3/8: searching 4 subtopics`,
  `synthesizing 5 pages in parallel`, `writing pages to KMS`).
- **Iteration progress bar** — N segments, color-coded done /
  in-progress / pending.
- **Score history** — one row per completed iteration with a
  0-100% bar and the source count delta.
- **Phase log** — last 10 distinct phases, current one highlighted.
- **Footer** — `Show result` / `Cancel` buttons depending on status.

The panel auto-focuses the most-recent running job; when no jobs
are active it hides entirely so the right edge stays compact.

### CLI — completion line on next prompt

The CLI prints a one-line announcement above each readline prompt
for jobs that finished since the last prompt:

```
[research done: id=research-a3f1c2 → obon-festival/2026-05-09-…md]
[research failed: id=research-x9z8] HAL request failed: HTTP 429
```

Each id announced once per process. Use `/research show <id>` to
print the synthesized page in chat, `/research wait <id>` to block
until terminal (useful in scripts).

## How the pipeline works

1. **Initial broad search** — one WebSearch call for the raw query,
   10 results.
2. **Subtopic extraction** — LLM proposes 3-5 focused search queries
   from the seed.
3. **Iteration loop** (1..max_iter):
   - Per subtopic: parallel WebSearch, top-3 fetch, accumulate.
   - Evaluate (LLM scores 0.0-1.0 + free-form notes on gaps).
   - Stop when `iter ≥ min_iter AND score ≥ threshold`, or at
     `max_iter`, or when the LLM signals "no more subtopics".
   - Otherwise generate next-round subtopics from the eval notes.
4. **Page plan** — LLM groups accumulated sources into ≤ `max_pages`
   coherent pages. Page count is a *ceiling, not a target* — narrow
   queries produce fewer pages.
5. **Parallel page synthesis** — one LLM call per page. Each call
   sees the full plan so cross-links resolve.
6. **Cross-link rewrite** — `[[karpathy]]` becomes
   `[[<run-prefix>__karpathy]]` so it points at the actual file on
   disk. Display text preserved (`[[karpathy|Andrej Karpathy]]`).
7. **Sources section + citation linkifier** — pipeline rebuilds
   `## Sources` from actual `[N]` usage and rewrites inline `[N]` to
   clickable links pointing at cached source files.
8. **Write pages + update `_summary.md` + cache cited sources**.

## Performance + cost

A typical 4-iteration run with 4 subtopics and 5 emitted pages:

| Step | LLM calls |
|---|---|
| extract_subtopics | 1 |
| evaluate | 4 (one per iter) |
| extract_next_subtopics | 3 |
| plan_pages | 1 |
| derive_topic_slug (when no `--kms`) | 1 |
| write_research_page (parallel) | 5 |
| **Total** | **~15** |

Plus HTTP: ~17 web searches + ~12 page fetches.

Wall clock with `gpt-4.1-mini`: 3-5 minutes (pages synthesize in
parallel, so the page-count multiplier doesn't dominate).

`/research` runs entirely in the background — your main chat
session is unaffected, you can keep typing while it works.

## Tips

- **Pin the KMS** — pass `--kms <name>` if you want output to
  accumulate in a specific knowledge base across multiple runs.
  Without `--kms`, the pipeline derives a per-query slug.
- **Use `/kms use <name>`** before asking follow-up questions so the
  LLM consults the freshly-written pages. The KMS auto-activates on
  research completion in GUI sessions; CLI users may need to
  activate manually.
- **Inspect raw sources** when verifying claims — open
  `<kms>/sources/<slug>.md` directly. The cached body has the
  original URL in frontmatter for traceability.
- **Tune `--max-pages`** for topic breadth: 1-2 for narrow factual
  questions, 3-5 for medium topics, 7+ for broad overviews.
- **Configure HAL** for cleaner source archives. The clean-Markdown
  output beats `WebFetch`'s HTML conversion noticeably when the
  page has tables, code blocks, or complex structure.

## Troubleshooting

**"research time budget exhausted"** — bump `--budget-time` or
narrow the query. Default is 15 minutes.

**"all WebSearch backends failed"** — check `TAVILY_API_KEY` /
`BRAVE_SEARCH_API_KEY` in Settings, or run with no key (falls back
to DuckDuckGo, lower quality).

**Pages don't cross-link** — the LLM only links when relevant. If
your query is very narrow (one entity, one concept), there may
genuinely be only one page — wikilinks are unnecessary.

**Sources section has "(unknown source — index out of range)"** —
the LLM hallucinated a citation index outside the source list.
Rare; usually fixes itself on retry. The unresolved entry is
preserved so you can spot which claim is uncited.

**Job stuck in "synthesizing N pages"** — one of the parallel
page-synth LLM calls is slow. Check `/research status <id>` for
phase. Cancel with `/research cancel <id>` if it exceeds your
patience; partial results aren't kept.
