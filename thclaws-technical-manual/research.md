# `/research` — background research pipeline

`/research <query>` spawns a background tokio task that orchestrates web search → LLM iteration → multi-page synthesis → KMS write. The pipeline runs entirely outside the agent loop — direct `Provider::stream()` calls, direct tool invocations, no agent state contamination. The output is a permanent artifact in a target KMS: multiple cross-linked pages with `[N]` citations and a cached source archive.

This doc covers: the slash command surface, the `ResearchManager` lifecycle, the `JobConfig` knobs, the iteration loop, the multi-page synthesis flow, the cross-link rewriter + sources-section regenerator + citation linkifier, the KMS write layout, the broadcaster pattern, the GUI sidebar wire-up, the HAL-first fetch routing, and the testing surface.

**Source modules:**
- `crates/core/src/research/mod.rs` — `JobId`, `JobStatus`, `JobConfig`, `JobView`, `ResearchManager`, `start()`
- `crates/core/src/research/pipeline.rs` — `ResearchTools` trait, `ProductionTools`, `run_with_tools()`, fetch routing, WebSearch markdown parser
- `crates/core/src/research/llm_calls.rs` — `extract_subtopics`, `extract_next_subtopics`, `evaluate`, `plan_pages`, `write_research_page`, `derive_topic_slug`, `synthesize` (legacy single-page); JSON parser, score parser, slug sanitizer
- `crates/core/src/research/kms_writer.rs` — multi-page write, cross-link rewriter, `## Sources` regenerator, citation linkifier, source caching
- `crates/core/src/research/test_helpers.rs` — shared `scoped_home` for tests that mutate `HOME` / `cwd`
- `crates/core/src/repl.rs` — `SlashCommand::{ResearchStart, ResearchList, ResearchStatus, ResearchShow, ResearchCancel, ResearchWait}`, `parse_research_subcommand`, CLI dispatch, completion auto-print on REPL prompt
- `crates/core/src/shell_dispatch.rs` — GUI dispatch arms
- `crates/core/src/shared_session.rs` — broadcaster wire-up: `ResearchManager::set_broadcaster` fires `ViewEvent::ResearchUpdate(json)` per state change + `ViewEvent::KmsUpdate` on Done transition
- `crates/core/src/gui.rs` — `build_research_update_payload` JSON envelope
- `frontend/src/components/ResearchSidebar.tsx` — right-edge verbose progress panel

**Cross-references:**
- [`kms.md`](kms.md) — knowledge-base storage; research pipeline writes via `kms::write_page` for index integration
- [`built-in-tools.md`](built-in-tools.md) — `WebSearch`, `WebFetch`, `WebScrape` (HAL) — the tools `ProductionTools` wraps
- [`agentic-loop.md`](agentic-loop.md) — research deliberately runs OUTSIDE the agent loop (direct `Provider::stream()`); contrast with how regular tool calls work

---

## 1. Concept

A research run is a fixed-shape Rust pipeline that calls the LLM as a pure callable (one prompt → one response, no agent loop). Running outside the agent gives:

- **No tool-approval friction** — `/research <query>` is the user's authorization for the whole run; per-call approvals would interrupt 15+ LLM calls.
- **Per-job cancellation** — each run carries its own `CancelToken`; main session's Cmd-C doesn't kill it (but `/research cancel <id>` does).
- **No agent context contamination** — main session's chat history isn't extended with 30+ search-result blocks.
- **Background-native** — `tokio::spawn`'d at start time; user keeps typing while it runs.

The pipeline shape (one `run_with_tools` call):

```
1. initial broad WebSearch (10 results)
2. extract_subtopics(query, seed)             → 3-5 subtopics
3. iteration loop (1..=max_iter):
     parallel WebSearch per subtopic
     fetch top-N per subtopic
     accumulate (dedup by URL, cap body to MAX_SOURCE_BODY_CHARS)
     evaluate(query, sources)                 → score + free-form notes
     stop conditions:
       iter >= min_iter AND score >= threshold → break
       iter == max_iter                        → break
       extract_next_subtopics returns []       → natural stop
     extract_next_subtopics(query, sources, notes)
4. plan_pages(query, sources, max_pages)       → Vec<PagePlan>
5. derive_topic_slug(query) [if --kms not passed]
6. parallel write_research_page per plan entry → Vec<String>
7. post-process per page:
     rewrite_cross_links → [[<run-prefix>__slug]]
     ensure_sources_section → canonical numbered list
     linkify_citations → [N] → [N](../sources/slug.md)
8. kms::write_page per page                   (uses index machinery)
9. append_run_section → _summary.md
10. cache cited sources → <kms>/sources/<url-slug>.md
```

Result page filename in `JobView::result_page` is the LAST page written (so `/research show <id>` opens something meaningful); `_summary.md` is the canonical entry-point.

---

## 2. `/research` slash command

| Syntax | Effect |
|---|---|
| `/research <query>` | Start a new run with default config |
| `/research [flags...] <query>` | Start with overrides |
| `/research` (or `/research list`) | List all jobs (newest first) |
| `/research status <id>` | Detailed view (phase, iter, score, error) |
| `/research show <id>` | Print synthesized result in chat / read from KMS |
| `/research cancel <id>` (or `stop` / `kill`) | Signal cancel; partial result discarded |
| `/research wait <id>` | Block CLI prompt until terminal (CLI-only) |

### Start flags

```rust
pub enum SlashCommand {
    ResearchStart {
        query: String,
        kms_target: Option<String>,
        min_iter: Option<u32>,
        max_iter: Option<u32>,
        score_threshold_pct: Option<u32>,  // 0-100, converted to f32 at dispatch
        max_pages: Option<u32>,
        budget_tokens: Option<u64>,         // reserved; not yet wired
        budget_time_secs: Option<u64>,
    },
    // ...
}
```

`score_threshold_pct` is `u32` percent (not `f32`) because `SlashCommand` derives `Eq` and `f32` can't satisfy that (NaN). Parser at `parse_research_start` accepts both decimal (`0.85`) and integer (`85`) — normalized to percent at parse time.

`--budget-tokens` is parsed but not yet plumbed to `JobConfig` (reserved for future cost-cap integration).

### Parse routing

`parse_research_subcommand` checks first token after `--user` flag:

```rust
match sub {
    "" | "list" => SlashCommand::ResearchList,
    "status" => SlashCommand::ResearchStatus { id },
    "show"   => SlashCommand::ResearchShow { id },
    "cancel" | "stop" | "kill" => SlashCommand::ResearchCancel { id },
    "wait"   => SlashCommand::ResearchWait { id },
    _        => parse_research_start(trimmed),  // bare query or with flags
}
```

---

## 3. `ResearchManager` + lifecycle

Process-wide singleton (`OnceLock<ResearchManager>`) holding all jobs. Each job has its own `CancelToken` + wall-clock deadline.

```rust
pub struct ResearchManager {
    jobs: RwLock<HashMap<JobId, Arc<RwLock<JobInner>>>>,
    broadcaster: Mutex<Option<Box<dyn Fn(&[JobView]) + Send + Sync>>>,
}

struct JobInner {
    view: JobView,
    cancel: CancelToken,
    deadline: Instant,
}
```

Lifecycle:

```
register(query, &config)        → JobStatus::Pending, returns (id, cancel_token)
update_phase(id, phase_str)     → flips to Running; broadcasts
record_iteration(id, iter, sources, score) → updates counters; broadcasts
finalize(id, status, page, err) → flips to terminal (Done/Cancelled/Failed);
                                  broadcasts; idempotent (second call no-op)
cancel(id)                      → fires CancelToken; broadcasts; pipeline
                                  exits at next check_alive()
prune_terminal(keep_recent)     → drops terminal jobs older than threshold
```

### `JobStatus`

```rust
pub enum JobStatus {
    Pending,    // registered, pipeline hasn't started
    Running,    // in iteration loop / synth / write
    Done,       // KMS write succeeded
    Cancelled,  // user signaled cancel
    Failed,     // any error in the pipeline
}
```

`Pending → Running` happens on first `update_phase` call. `Running → terminal` only via `finalize` (asserted with `debug_assert!`). The `is_terminal()` predicate gates idempotency on `finalize` and skips already-cancelled jobs.

### `JobView` (read-only snapshot)

The struct returned by `manager().get(id)` and `manager().list()`. Same shape goes into the JSON envelope the GUI sidebar consumes:

```rust
pub struct JobView {
    pub id: JobId,
    pub query: String,
    pub status: JobStatus,
    pub phase: String,
    pub iterations_done: u32,
    pub source_count: u32,
    pub last_score: Option<f32>,
    pub started_at: SystemTime,
    pub finished_at: Option<SystemTime>,
    pub kms_target: Option<String>,
    pub result_page: Option<String>,
    pub error: Option<String>,
}
```

### `start()` entry point

```rust
pub async fn start(
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
) -> Result<JobId>
```

Registers the job and spawns `pipeline::run` on a tokio task. Returns immediately with the new `JobId` — caller doesn't await. The spawned task calls `manager().finalize` on completion regardless of outcome (success → Done with result_page; error → Failed with err_str; cancel → Cancelled).

---

## 4. `JobConfig` knobs + defaults

```rust
pub struct JobConfig {
    pub min_iter: u32,             // 2  — hard floor
    pub max_iter: u32,             // 8  — hard ceiling
    pub score_threshold: f32,      // 0.75 — early-stop signal
    pub subtopics_per_iter: u32,   // 4  — fanout per iteration
    pub fetch_top_n: u32,          // 3  — pages fetched per subtopic
    pub max_pages: u32,            // 7  — KMS pages per run (ceiling)
    pub llm_timeout: Duration,     // 120s per LLM call
    pub time_budget: Duration,     // 15min total wall-clock
    pub kms_target: Option<String>, // auto-derive when None
}
```

Defaults pinned in `default_config_matches_documented_knobs` test — changes require updating the test deliberately.

`max_pages` is documented to the LLM as a *ceiling, not a target* — the `plan_pages` prompt says explicitly "Output FEWER if the material doesn't naturally split that many ways" + "Reject thin pages: if a candidate page would have <3 sources or repeats material covered better by another page in the plan, drop it."

---

## 5. The iteration loop

`run_with_tools` is the heart of `pipeline.rs`. Stop conditions:

```rust
for iter in 1..=config.max_iter {
    // ... search + fetch + accumulate ...
    let eval = llm_calls::evaluate(...).await?;
    let floor_passed = iter >= config.min_iter;
    let score_satisfied = eval.score >= config.score_threshold;
    if floor_passed && score_satisfied { break; }
    if iter == config.max_iter { break; }
    // ... extract_next_subtopics ...
}
```

`check_alive(job_id, &cancel)` is called between every await point — checks `cancel.is_cancelled()` and `manager().is_over_budget(job_id)`. Returns `Err(Tool("research job cancelled"))` or `Err(Tool("research time budget exhausted"))`; the spawned task in `start()` finalizes accordingly.

### Score parser

`parse_score_and_notes` is permissive — accepts `score: 0.7`, `Score: 0.85`, `score: 7/10`, `score: 75%`. The first non-empty line with a `score:` prefix wins; trailing prose is the notes string. Bad parse → defaults to 0.0 (loop continues, not spuriously stops).

---

## 6. Multi-page synthesis (M6.39.6)

Replaces single `synthesize` call with three steps:

### Step 6a — `plan_pages`

LLM groups accumulated sources into ≤ `max_pages` coherent topics. Output is JSON:

```json
[
  {"slug": "andrej-karpathy", "title": "Andrej Karpathy",
   "topic": "Karpathy as proponent of LLM Wiki",
   "source_indices": [1, 4, 7]},
  {"slug": "concept-overview", "title": "Concept Overview", ...},
  ...
]
```

`parse_page_plan` is permissive — handles `\`\`\`json` fences, leading commentary, trailing markup, dedupes slugs, filters out-of-range source indices. Empty result triggers single-page fallback.

### Step 6b — parallel `write_research_page`

```rust
let mut page_futures = Vec::with_capacity(plan.len());
for page in &plan {
    page_futures.push(tokio::spawn(async move {
        llm_calls::write_research_page(...)
    }));
}
let mut bodies: Vec<(usize, String)> = Vec::with_capacity(plan.len());
for (idx, fut) in page_futures.into_iter().enumerate() {
    bodies.push((idx, fut.await??));
}
```

Each per-page call gets:
- `query` — original research query
- `this_page: &PagePlan` — slug, title, topic, source_indices
- `all_pages: &[PagePlan]` — full plan, so the LLM can cross-link with `[[other-slug]]`
- `sources: &[ResearchSource]` — full source list, but the prompt only includes entries whose index is in `this_page.source_indices`

Prompt is explicit: lead with abstract, no leading heading, `##` sections, `[N]` citations, **don't write `## Sources` yourself** (pipeline regenerates it).

### Step 6c — post-process per page

Three stages, in order, each idempotent:

```rust
rewritten = rewrite_cross_links(body, &known_slugs, &run_prefix);
rewritten = ensure_sources_section(&rewritten, &sources_meta);
rewritten = linkify_citations(&rewritten, &sources_meta);
```

Order matters:
- `rewrite_cross_links` first — operates on `[[slug]]` patterns the LLM emitted, transforms to `[[<run-prefix>__slug]]`.
- `ensure_sources_section` next — strips whatever LLM wrote (often partial), regenerates from actual `[N]` usage. Format is numbered list (`1. [Title](path) — url`) — not `[N]` prefix — so the next step doesn't double-link.
- `linkify_citations` last — walks every `[N]` not followed by `(`, rewrites to `[N](../sources/<slug>.md)`. Skips multi-cite (`[1, 3]`), already-linked, and unresolved indices.

---

## 7. Cross-link rewriter

```rust
pub fn rewrite_cross_links(
    body: &str,
    known_slugs: &[&str],
    run_prefix: &str,
) -> String
```

Walks the body, finds `[[slug]]` and `[[slug|display]]` patterns. Slugs in `known_slugs` get rewritten to `[[<run-prefix>__slug]]` (display preserved); slugs NOT in the list pass through unchanged (could reference a previous research run's pages). Skip cases:
- inner > 120 chars → not a wikilink
- inner contains newline → not a wikilink

Filenames on disk include the run prefix (`2026-05-09-llm-wiki__karpathy.md`) so wikilinks must match. Display text gives Obsidian a clean human label.

---

## 8. Sources section regenerator

`ensure_sources_section` parses `[N]` citations from the body, regenerates the `## Sources` section deterministically:

```rust
pub fn ensure_sources_section(
    body: &str,
    sources: &[(u32, String, String)],  // (index, title, url)
) -> String
```

Steps:
1. `parse_citation_indices(body)` → `HashSet<u32>` of every cited index
2. `strip_sources_section(body)` → body without any LLM-written `## Sources` block
3. Build new section with one entry per cited index, sorted ascending:

```markdown
## Sources

1. [Title](../sources/<url-slug>.md) — https://upstream-url
3. [Other](../sources/<other-slug>.md) — https://other.com
```

Numbered list format (not `[N]`) avoids conflict with `linkify_citations`. Title is clickable (resolves to `<kms>/sources/<slug>.md`); upstream URL follows for human reading.

Unresolved indices (LLM hallucinated) get `99. (unknown source — index out of range)` so they're visible rather than silently dropped.

---

## 9. Citation linkifier

```rust
pub fn linkify_citations(
    body: &str,
    sources: &[(u32, String, String)],
) -> String
```

Inline rewrite: `[1]` → `[1](../sources/<url-slug>.md)`. Skip cases:
- `[1](path)` already linked (next char after `]` is `(`) — idempotent
- `[1, 3]` multi-cite — no unambiguous single-link rewrite
- `[99]` unresolved — sources section flags it
- `[non-numeric]` — not a citation
- `[[wikilink]]` — inner has letters, not citation pattern

UTF-8-safe: walks bytes for ASCII matching but pushes char-boundary slices for non-matching paths, so non-Latin queries (Thai, Chinese, etc.) survive intact.

---

## 10. KMS write layout

```
<kms-name>/
├── pages/
│   ├── <run-prefix>__<page-slug>.md     — type: research-page
│   ├── <run-prefix>__<page-slug>.md
│   └── _summary.md                       — type: research-summary
├── sources/                              — flat dir, no subfolders
│   ├── <url-slug>.md                     — type: research-source
│   └── <url-slug>.md
├── index.md                              — auto-managed by kms::write_page
├── log.md                                — auto-appended on every write
└── SCHEMA.md                             — user-editable
```

`<run-prefix>` = `<today>-<query-slug>` (e.g. `2026-05-09-llm-wiki`).
`<page-slug>` = LLM-derived slug (e.g. `andrej-karpathy`).
`<url-slug>` = deterministic slug of URL (e.g. `en-wikipedia-org-wiki-obon`).

Each research page has frontmatter:

```yaml
---
title: "Andrej Karpathy"
type: research-page
page_slug: andrej-karpathy
run: 2026-05-09-llm-wiki
query: "what is LLM-Wiki"
topic: "Karpathy as proponent of LLM Wiki"
created: 2026-05-09
updated: 2026-05-09
---
```

`type: research-page` discriminator lets `KmsSearch --type research-page` filter to research output specifically.

`_summary.md` accumulates one section per `/research` run:

```markdown
## 2026-05-09 — what is LLM-Wiki

- [[2026-05-09-llm-wiki__concept-overview|Concept Overview]] — Core idea …
- [[2026-05-09-llm-wiki__andrej-karpathy|Andrej Karpathy]] — Karpathy as proponent
- [[2026-05-09-llm-wiki__rag-comparison|vs RAG]] — Differences in storage model
```

Wikilinks resolve to the actual page filenames (run prefix included). `append_run_section` either creates the summary fresh (with frontmatter + intro paragraph) or appends a new `## YYYY-MM-DD — query` section to an existing file.

---

## 11. Source caching

After post-processing pages, the pipeline accumulates all cited indices across all pages and writes each to `<kms>/sources/<url-slug>.md`:

```rust
let mut all_cited_indices: HashSet<u32> = HashSet::new();
for (idx, body) in bodies {
    let rewritten = /* ... */;
    let cited = kms_writer::parse_citation_indices(&rewritten);
    all_cited_indices.extend(cited);
}
for s in &sources {
    if all_cited_indices.contains(&s.index) {
        let _ = kms_writer::write_source(/* ... */);
    }
}
```

Per-source errors are tolerated (best-effort archive). `write_source` builds the filename via `url_to_filename` (deterministic slug), so the same URL across multiple research runs maps to the same file — latest fetch wins, archive stays bounded.

Source body cap is `MAX_SOURCE_BODY_CHARS = 1_000_000` (1 MB); typical wiki / blog pages fit comfortably. Synthesis prompts further truncate via `snippet(body, N)` (500-600 chars) when composing, so larger source bodies don't bloat LLM cost — only the on-disk archive grows.

---

## 12. HAL-first fetch routing (M6.39.8)

`ProductionTools::fetch` checks `HAL_API_KEY` per call:

```rust
async fn fetch(&self, url: &str) -> Result<String> {
    if hal_key_available() {
        match self.scrape.call(json!({"url": url})).await {
            Ok(json_str) => {
                if let Some(markdown) = extract_hal_content(&json_str) {
                    return Ok(markdown);
                }
                // fall through to WebFetch
            }
            Err(_) => { /* fall through */ }
        }
    }
    self.fetch.call(json!({
        "url": url,
        "max_bytes": FETCH_MAX_BYTES,
    })).await
}
```

HAL's `WebScrape` returns clean Markdown via headless-browser scrape — much better archive quality than `WebFetch`'s HTML→Markdown conversion. `extract_hal_content` pulls the `content` field out of HAL's `/scrape/v1/url` JSON envelope.

Fallback path: `WebFetch` with `FETCH_MAX_BYTES = 1_048_576` (1 MB). Both budgets match `MAX_SOURCE_BODY_CHARS` so the in-memory cap and the per-fetch budget agree.

---

## 13. Broadcaster + GUI wire-up

`ResearchManager::set_broadcaster` is wired in `shared_session.rs::run_worker` at session bootstrap:

```rust
let known_done_ids: Arc<Mutex<HashSet<String>>> = /* ... */;
crate::research::manager().set_broadcaster(move |jobs| {
    let payload = crate::gui::build_research_update_payload();
    let _ = research_tx.send(ViewEvent::ResearchUpdate(payload.to_string()));

    // Also fire kms_update once per Done transition
    let mut new_done = false;
    if let Ok(mut known) = known_done_ids.lock() {
        for j in jobs {
            if j.status == JobStatus::Done && !known.contains(&j.id) {
                known.insert(j.id.clone());
                new_done = true;
            }
        }
    }
    if new_done {
        let kms_payload = crate::gui::build_kms_update_payload();
        let _ = research_tx.send(ViewEvent::KmsUpdate(kms_payload.to_string()));
    }
});
```

`ViewEvent::ResearchUpdate(json)` carries the same shape as `JobView` for every job (newest-first order). The Done-transition tracker triggers a `kms_update` so the sidebar's Knowledge panel refreshes when research auto-creates a KMS — only fires once per id (the closure remembers what it already announced).

The frontend's `ResearchSidebar.tsx` subscribes to `research_update`, accumulates a per-job `phaseLog` + `iterationHistory`, and renders the verbose progress view.

---

## 14. CLI completion auto-print

The CLI REPL in `repl.rs` runs a one-shot announcement loop before each `rl.readline()`:

```rust
for j in crate::research::manager().list() {
    if !j.status.is_terminal() { continue; }
    if notified_research.contains(&j.id) { continue; }
    notified_research.insert(j.id.clone());
    match j.status {
        JobStatus::Done => println!("[research done: id={} → {}] query: {}", ...),
        JobStatus::Cancelled => println!("[research cancelled: id={}]", j.id),
        JobStatus::Failed => println!("[research failed: id={}] {err}", ...),
        _ => {}
    }
}
```

`notified_research: HashSet<JobId>` is per-process state in the REPL loop — each id announced once. CLI users running long research jobs see a clean "[research done: …]" line above their next prompt.

---

## 15. Testing surface

Mocked end-to-end via two trait abstractions:

```rust
#[async_trait::async_trait]
pub trait ResearchTools: Send + Sync {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>>;
    async fn fetch(&self, url: &str) -> Result<String>;
}

// MockProvider implements crate::providers::Provider with canned responses
```

`run_with_tools(job_id, query, config, mock_provider, mock_tools)` lets tests exercise the full pipeline without network or real LLM. `MockProvider::new(vec!["canned response 1", "canned response 2", ...])` queues responses; each `stream()` call dequeues one.

Test categories:
- **Manager lifecycle** (`mod.rs::tests`) — register / update_phase / record_iteration / finalize / cancel / list / prune
- **LLM call helpers** (`llm_calls.rs::tests`) — `parse_bulleted_list`, `parse_score_and_notes`, `parse_page_plan`, `sanitize_slug`, `snippet`, prompt-shape assertions
- **KMS writer** (`kms_writer.rs::tests`) — page write, sources/ caching, summary append, cross-link rewriter, sources-section regenerator, citation linkifier
- **Pipeline integration** (`pipeline.rs::tests`) — happy path, hard floor, hard ceiling, cancel before loop, empty next-subtopics break, accumulate dedupe + truncation, WebSearch markdown parser, HAL key detection, HAL content extraction

Tests touching `HOME` / `cwd` use `research::test_helpers::scoped_home()` — a shared process-wide `Mutex` + `RwLock` guard pattern so kms_writer and pipeline tests don't race when run in parallel.

---

## 16. Phases shipped

- **M6.39.1** — core module: manager, pipeline scaffold, llm_calls, kms_writer with single-page synthesis (49 mock-driven tests)
- **M6.39.2** — `/research` slash + production tools + REPL completion notification
- **M6.39.3** — GUI sidebar panel + live broadcasting
- **M6.39.4** — `/system` debug command (separate but related)
- **M6.39.5** — abstract-first synthesis, `~/.claude/*` exclusion, KMS sidebar refresh on Done, drop `research-` prefix, cited sources/ archive, MANDATORY KMS consultation directive
- **M6.39.6** — multi-page synthesis with `[[wikilinks]]`, max_pages ceiling-not-target framing
- **M6.39.7** — clickable `[N](path)` citations + canonical Sources regenerator + 1 MB body cap
- **M6.39.8** — HAL-first fetch with WebFetch fallback

Each phase shipped as a separate commit pair (workspace + public mirror).
