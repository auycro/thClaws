//! Research pipeline driver.
//!
//! Orchestrates one job from start to KMS write:
//!
//! ```text
//!   initial WebSearch (broad)
//!     ↓
//!   extract_subtopics (LLM)
//!     ↓
//!   ┌→ parallel research_subtopic (search → fetch top-N → accumulate)
//!   │   ↓
//!   │  evaluate (LLM → score)
//!   │   ↓
//!   │  if iter < min_iter OR (iter < max_iter AND score < threshold):
//!   │     extract_next_subtopics (LLM, conditioned on eval notes)
//!   └─────────────────────┘
//!     ↓
//!   synthesize (LLM → markdown + [N] citations)
//!     ↓
//!   kms_writer::write
//! ```
//!
//! All four LLM helpers and both search tools are passed in by trait
//! object so the pipeline is fully unit-testable with mocks. The
//! production wiring in M6.39.2 will instantiate real `Provider` +
//! `WebSearchTool` + `WebFetchTool` and pass them through.

use super::llm_calls::{self, ResearchSource};
use super::{kms_writer, manager, JobConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::providers::Provider;
use std::sync::Arc;

const MAX_SOURCE_BODY_CHARS: usize = 1500;
const SEED_SEARCH_RESULTS: u32 = 10;

/// Trait abstraction over WebSearch / WebFetch so the pipeline can be
/// tested with deterministic fakes. Production impl wraps the real
/// `tools::WebSearchTool` / `tools::WebFetchTool` (M6.39.2 wires
/// these). One trait, two methods, both async — keeps the abstraction
/// layer thin.
#[async_trait::async_trait]
pub trait ResearchTools: Send + Sync {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>>;
    async fn fetch(&self, url: &str) -> Result<String>;
}

/// Minimal search-result shape the pipeline actually uses. Real
/// WebSearchTool returns Markdown — the production [`ResearchTools`]
/// impl parses that back into structured hits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Run one job to completion. Returns the relative raw-note filename
/// on success (caller surfaces as `JobView::result_page`).
pub async fn run(
    job_id: &str,
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
    cancel: CancelToken,
) -> Result<String> {
    let tools = production_tools();
    run_with_tools(job_id, query, config, provider, model, cancel, tools).await
}

/// Test-injectable variant — same logic, takes the tools impl by Arc
/// so unit tests can pass a deterministic mock.
pub async fn run_with_tools(
    job_id: &str,
    query: String,
    config: JobConfig,
    provider: Arc<dyn Provider>,
    model: String,
    cancel: CancelToken,
    tools: Arc<dyn ResearchTools>,
) -> Result<String> {
    let mgr = manager();

    // ── 1. Initial broad search ─────────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "iteration 1: initial search");
    let seed_hits = tools.search(&query, SEED_SEARCH_RESULTS).await?;
    let mut sources: Vec<ResearchSource> = Vec::new();
    accumulate(&mut sources, seed_hits, None);
    mgr.record_iteration(job_id, 1, sources.len() as u32, None);

    // ── 2. Extract initial subtopics ────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "iteration 1: extracting subtopics");
    let mut subtopics = llm_calls::extract_subtopics(
        provider.as_ref(),
        &model,
        &query,
        &sources,
        config.subtopics_per_iter,
        config.llm_timeout,
        &cancel,
    )
    .await?;

    let mut last_score: Option<f32> = None;
    let mut last_notes = String::new();

    // ── 3. Iteration loop ───────────────────────────────────────────
    for iter in 1..=config.max_iter {
        check_alive(job_id, &cancel)?;
        if mgr.is_over_budget(job_id) {
            return Err(Error::Tool("research time budget exhausted".into()));
        }
        if subtopics.is_empty() {
            // LLM signaled "no further subtopics" — accept that as a
            // natural stop signal even if score < threshold.
            break;
        }
        mgr.update_phase(
            job_id,
            format!(
                "iteration {iter}/{}: searching {} subtopics",
                config.max_iter,
                subtopics.len()
            ),
        );

        // Parallel search across subtopics. Errors are tolerated — a
        // bad subtopic shouldn't kill the whole iteration.
        let mut hits_per_topic: Vec<Vec<SearchHit>> = Vec::with_capacity(subtopics.len());
        for st in &subtopics {
            check_alive(job_id, &cancel)?;
            match tools.search(st, config.subtopics_per_iter).await {
                Ok(hits) => hits_per_topic.push(hits),
                Err(_) => hits_per_topic.push(Vec::new()),
            }
        }

        // Fetch top-N from each subtopic's hits. Sequential within
        // subtopic + sequential across subtopics — keeps memory + HTTP
        // load predictable. Could parallelize later if needed.
        for hits in hits_per_topic {
            for hit in hits.into_iter().take(config.fetch_top_n as usize) {
                check_alive(job_id, &cancel)?;
                let body = match tools.fetch(&hit.url).await {
                    Ok(b) => b,
                    Err(_) => hit.snippet.clone(),
                };
                accumulate(
                    &mut sources,
                    vec![SearchHit {
                        title: hit.title,
                        url: hit.url,
                        snippet: body,
                    }],
                    None,
                );
            }
        }
        mgr.record_iteration(job_id, iter, sources.len() as u32, last_score);

        // ── 4. Evaluate ─────────────────────────────────────────────
        check_alive(job_id, &cancel)?;
        mgr.update_phase(
            job_id,
            format!("iteration {iter}/{}: evaluating coverage", config.max_iter),
        );
        let eval = llm_calls::evaluate(
            provider.as_ref(),
            &model,
            &query,
            &sources,
            config.llm_timeout,
            &cancel,
        )
        .await?;
        last_score = Some(eval.score);
        last_notes = eval.notes.clone();
        mgr.record_iteration(job_id, iter, sources.len() as u32, last_score);

        // Stop conditions:
        // - hard floor: must run at least min_iter
        // - score threshold: between floor and ceiling
        // - hard ceiling: enforced by the for-loop bound
        let floor_passed = iter >= config.min_iter;
        let score_satisfied = eval.score >= config.score_threshold;
        if floor_passed && score_satisfied {
            break;
        }
        if iter == config.max_iter {
            break;
        }

        // ── 5. Generate next-round subtopics from eval notes ────────
        check_alive(job_id, &cancel)?;
        mgr.update_phase(
            job_id,
            format!("iteration {iter}/{}: planning next round", config.max_iter),
        );
        subtopics = llm_calls::extract_next_subtopics(
            provider.as_ref(),
            &model,
            &query,
            &sources,
            &eval.notes,
            config.subtopics_per_iter,
            config.llm_timeout,
            &cancel,
        )
        .await?;
    }

    // ── 6. Final synthesis ──────────────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "synthesizing final answer");
    let mut synthesized = llm_calls::synthesize(
        provider.as_ref(),
        &model,
        &query,
        &sources,
        config.llm_timeout,
        &cancel,
    )
    .await?;
    if !last_notes.is_empty() {
        synthesized.push_str("\n\n---\n\n## Research notes\n\n");
        synthesized.push_str(&last_notes);
    }

    // ── 7. KMS write ────────────────────────────────────────────────
    check_alive(job_id, &cancel)?;
    mgr.update_phase(job_id, "writing to KMS");
    let kms_name = match &config.kms_target {
        Some(n) => n.clone(),
        None => {
            let slug = llm_calls::derive_topic_slug(
                provider.as_ref(),
                &model,
                &query,
                config.llm_timeout,
                &cancel,
            )
            .await
            .unwrap_or_else(|_| "research".into());
            format!("research-{slug}")
        }
    };
    let query_slug = simple_slug(&query);
    let outcome = kms_writer::write(
        &kms_name,
        &query,
        &query_slug,
        &kms_writer::today_str(),
        &synthesized,
    )?;
    Ok(format!("{}/{}", outcome.kms_name, outcome.raw_note))
}

// ── Helpers ──────────────────────────────────────────────────────────

fn check_alive(job_id: &str, cancel: &CancelToken) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Tool("research job cancelled".into()));
    }
    if manager().is_over_budget(job_id) {
        return Err(Error::Tool("research time budget exhausted".into()));
    }
    Ok(())
}

/// Append hits to the running source list, deduping by URL and
/// truncating bodies to `MAX_SOURCE_BODY_CHARS`. `start_index_hint` is
/// reserved for future explicit indexing; today we always assign next
/// available `index` (1-based, monotonic).
fn accumulate(
    sources: &mut Vec<ResearchSource>,
    hits: Vec<SearchHit>,
    _start_index_hint: Option<u32>,
) {
    for hit in hits {
        if hit.url.is_empty() {
            continue;
        }
        if sources.iter().any(|s| s.url == hit.url) {
            // Dedupe on URL — same page from different subtopic
            // searches doesn't get a second citation slot.
            continue;
        }
        let next_idx = sources.len() as u32 + 1;
        let body = if hit.snippet.len() > MAX_SOURCE_BODY_CHARS {
            let mut end = MAX_SOURCE_BODY_CHARS;
            while !hit.snippet.is_char_boundary(end) && end > 0 {
                end -= 1;
            }
            format!("{}…", &hit.snippet[..end])
        } else {
            hit.snippet
        };
        sources.push(ResearchSource {
            index: next_idx,
            title: hit.title,
            url: hit.url,
            body,
        });
    }
}

/// Stop-gap query→slug used for the raw-note filename when we need
/// something filesystem-safe before the LLM-derived `topic_slug` (used
/// for KMS naming). Just lowercase + collapse non-alphanumeric to `-`.
/// The KMS name itself uses [`llm_calls::derive_topic_slug`] for a
/// nicer multilingual result.
fn simple_slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "query".into()
    } else {
        // Keep filenames reasonable.
        trimmed.chars().take(60).collect()
    }
}

/// Production wiring (M6.39.2). Wraps the real `WebSearchTool` +
/// `WebFetchTool` from the agent's tool registry, parsing
/// WebSearch's Markdown response back into structured hits.
pub fn production_tools() -> Arc<dyn ResearchTools> {
    Arc::new(ProductionTools::new())
}

pub struct ProductionTools {
    search: crate::tools::WebSearchTool,
    fetch: crate::tools::WebFetchTool,
}

impl ProductionTools {
    pub fn new() -> Self {
        Self {
            search: crate::tools::WebSearchTool::default(),
            fetch: crate::tools::WebFetchTool::new(),
        }
    }
}

impl Default for ProductionTools {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ResearchTools for ProductionTools {
    async fn search(&self, query: &str, max_results: u32) -> Result<Vec<SearchHit>> {
        use crate::tools::Tool;
        let raw = self
            .search
            .call(serde_json::json!({
                "query": query,
                "max_results": max_results,
            }))
            .await?;
        Ok(parse_websearch_markdown(&raw))
    }
    async fn fetch(&self, url: &str) -> Result<String> {
        use crate::tools::Tool;
        // 8 KB cap per page is a budget tradeoff: the LLM gets enough
        // body to verify claims, but the source list doesn't blow up
        // the synthesis prompt across 30+ accumulated sources.
        self.fetch
            .call(serde_json::json!({
                "url": url,
                "max_bytes": 8192,
            }))
            .await
    }
}

/// Parse `WebSearchTool`'s Markdown output back into structured hits.
/// Output shape (M6.38.8):
/// ```text
/// Source: Tavily (web search)
///
/// Answer: ...
///
/// 1. Title (https://url)
///    snippet text
/// 2. Title (https://url)
///    snippet text
/// ```
///
/// We skip the leading `Source:` header + optional `Answer:` block,
/// then walk numbered entries. A line like `12. Some title (https://x)`
/// starts a new hit; subsequent indented lines are appended as snippet
/// body.
fn parse_websearch_markdown(body: &str) -> Vec<SearchHit> {
    let mut hits: Vec<SearchHit> = Vec::new();
    let mut current: Option<SearchHit> = None;
    let mut snippet_buf = String::new();

    for line in body.lines() {
        let trimmed = line.trim();
        // Skip header / empty lines / "Answer:" preamble.
        if trimmed.starts_with("Source:") || trimmed.starts_with("Answer:") {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if let Some((title, url)) = parse_numbered_entry(trimmed) {
            if let Some(mut prev) = current.take() {
                prev.snippet = std::mem::take(&mut snippet_buf).trim().to_string();
                hits.push(prev);
            }
            current = Some(SearchHit {
                title,
                url,
                snippet: String::new(),
            });
            snippet_buf.clear();
        } else if current.is_some() {
            // Indented continuation = snippet body.
            if !snippet_buf.is_empty() {
                snippet_buf.push(' ');
            }
            snippet_buf.push_str(trimmed);
        }
    }
    if let Some(mut last) = current {
        last.snippet = snippet_buf.trim().to_string();
        hits.push(last);
    }
    hits
}

/// Recognize `<number>. <title> (<url>)` — the WebSearch entry shape.
/// Returns `(title, url)` on a hit, `None` otherwise. `url` validated
/// loosely (must look like http(s)).
fn parse_numbered_entry(line: &str) -> Option<(String, String)> {
    let mut iter = line.char_indices();
    // Consume leading digits + dot.
    let mut last_digit_end = 0;
    let mut saw_digit = false;
    for (i, c) in iter.by_ref() {
        if c.is_ascii_digit() {
            last_digit_end = i + c.len_utf8();
            saw_digit = true;
        } else if c == '.' && saw_digit {
            break;
        } else {
            return None;
        }
    }
    if !saw_digit {
        return None;
    }
    let after_dot = line[last_digit_end + 1..].trim_start();
    // Find the last `(http...)` — title is everything before, URL is
    // inside the parens.
    let open = after_dot.rfind(" (http")?;
    let title = after_dot[..open].trim().to_string();
    let url_with_close = &after_dot[open + 2..];
    if !url_with_close.ends_with(')') {
        return None;
    }
    let url = url_with_close[..url_with_close.len() - 1].to_string();
    if title.is_empty() {
        return None;
    }
    Some((title, url))
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use crate::types::Message;
    use async_trait::async_trait;
    use futures::stream;
    use std::sync::Mutex;

    /// Mock provider that returns a queue of canned responses, one per
    /// `stream()` call. Each canned response is a single TextDelta
    /// followed by MessageStop.
    struct MockProvider {
        responses: Mutex<Vec<String>>,
        calls: Mutex<u32>,
    }

    impl MockProvider {
        fn new(responses: Vec<&str>) -> Self {
            Self {
                responses: Mutex::new(responses.into_iter().map(String::from).collect()),
                calls: Mutex::new(0),
            }
        }
        fn call_count(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl Provider for MockProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            *self.calls.lock().unwrap() += 1;
            let mut q = self.responses.lock().unwrap();
            let body = if q.is_empty() {
                "score: 0.9\nfully covered".into()
            } else {
                q.remove(0)
            };
            let events: Vec<Result<ProviderEvent>> = vec![
                Ok(ProviderEvent::MessageStart {
                    model: "mock".into(),
                }),
                Ok(ProviderEvent::TextDelta(body)),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Mock tools that return canned hits. Counts search/fetch calls.
    struct MockTools {
        hits_per_query: Mutex<Vec<Vec<SearchHit>>>,
        fetch_returns: Mutex<Vec<String>>,
        search_calls: Mutex<u32>,
        fetch_calls: Mutex<u32>,
    }

    impl MockTools {
        fn new(hits: Vec<Vec<SearchHit>>, fetch_returns: Vec<&str>) -> Self {
            Self {
                hits_per_query: Mutex::new(hits),
                fetch_returns: Mutex::new(fetch_returns.into_iter().map(String::from).collect()),
                search_calls: Mutex::new(0),
                fetch_calls: Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl ResearchTools for MockTools {
        async fn search(&self, _query: &str, _max: u32) -> Result<Vec<SearchHit>> {
            *self.search_calls.lock().unwrap() += 1;
            let mut q = self.hits_per_query.lock().unwrap();
            if q.is_empty() {
                Ok(Vec::new())
            } else {
                Ok(q.remove(0))
            }
        }
        async fn fetch(&self, _url: &str) -> Result<String> {
            *self.fetch_calls.lock().unwrap() += 1;
            let mut q = self.fetch_returns.lock().unwrap();
            if q.is_empty() {
                Ok("body content".into())
            } else {
                Ok(q.remove(0))
            }
        }
    }

    fn hit(title: &str, url: &str) -> SearchHit {
        SearchHit {
            title: title.into(),
            url: url.into(),
            snippet: format!("snippet for {title}"),
        }
    }

    use crate::research::test_helpers::scoped_home;

    /// Happy path: one query, score ≥ 0.75 after iter 2 (hard floor),
    /// loop stops, synthesizes, writes to KMS.
    #[tokio::test]
    async fn happy_path_stops_at_floor_and_writes_kms() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            // iter 1 extract_subtopics
            "1. subtopic-a\n2. subtopic-b",
            // iter 1 evaluate (low score → continue)
            "score: 0.5\nneeds more sources",
            // iter 1 extract_next_subtopics
            "1. subtopic-c\n2. subtopic-d",
            // iter 2 evaluate (high score → stop)
            "score: 0.85\ncovered",
            // synthesize
            "Final answer [1] [2]\n\n## Sources\n[1] https://a.example\n[2] https://b.example",
            // derive_topic_slug
            "test-topic",
        ]));
        let tools = Arc::new(MockTools::new(
            vec![
                vec![
                    hit("Seed A", "https://a.example"),
                    hit("Seed B", "https://b.example"),
                ],
                vec![hit("Topic A", "https://c.example")],
                vec![hit("Topic B", "https://d.example")],
                vec![hit("Topic C", "https://e.example")],
                vec![hit("Topic D", "https://f.example")],
            ],
            vec![
                "page A body",
                "page B body",
                "page C body",
                "page D body",
                "page E body",
            ],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 8,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("test query".into(), &cfg);
        let outcome = run_with_tools(
            &id,
            "test query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools.clone(),
        )
        .await
        .unwrap();
        // 2 iterations × evaluate + first iteration extract_subtopics
        // + iter 1 extract_next + synthesize + derive_slug = 6 calls
        assert_eq!(provider.call_count(), 6);
        assert!(outcome.contains("research-test-topic"));
        assert!(outcome.ends_with(".md"));
    }

    /// Hard floor enforcement: even if iter 1 scores ≥ threshold, the
    /// loop must run min_iter rounds.
    #[tokio::test]
    async fn hard_floor_blocks_early_exit() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            "1. topic-a\n2. topic-b",      // extract_subtopics (iter 1)
            "score: 0.95\nlooks complete", // evaluate (iter 1) — high but floor blocks
            "1. topic-c\n2. topic-d",      // extract_next_subtopics
            "score: 0.95\nstill complete", // evaluate (iter 2) — now passes
            "Synth [1]",                   // synthesize
            "topic",                       // derive_topic_slug
        ]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 5],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 5,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("query".into(), &cfg);
        run_with_tools(
            &id,
            "query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools,
        )
        .await
        .unwrap();
        // 5 = subtopics + eval-1 + next-subtopics + eval-2 + synthesize + slug
        // (slug runs because kms_target was None)
        assert_eq!(provider.call_count(), 6);
    }

    /// Hard ceiling enforcement: low score for many iterations, loop
    /// must stop at max_iter.
    #[tokio::test]
    async fn hard_ceiling_stops_at_max_iter() {
        let _g = scoped_home();
        let mut canned: Vec<&str> = Vec::new();
        canned.push("1. t-a\n2. t-b"); // initial extract
        for _ in 0..3 {
            canned.push("score: 0.2\nstill missing things"); // evaluate
            canned.push("1. more-a\n2. more-b"); // next subtopics
        }
        canned.pop(); // last iteration doesn't get next-subtopics call (loop exits after eval)
        canned.push("Synth"); // synthesize
        canned.push("topic"); // slug
        let provider = Arc::new(MockProvider::new(canned));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 10],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 2,
            max_iter: 3,
            score_threshold: 0.75,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("query".into(), &cfg);
        run_with_tools(
            &id,
            "query".into(),
            cfg,
            provider.clone(),
            "mock-model".into(),
            cancel,
            tools,
        )
        .await
        .unwrap();
        let v = manager().get(&id).unwrap();
        assert_eq!(v.iterations_done, 3);
    }

    /// Cancellation: cancel before first iteration, pipeline returns
    /// Err so the spawn task marks Cancelled.
    #[tokio::test]
    async fn cancel_before_loop_returns_error() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec!["1. a\n2. b"]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]],
            vec![],
        ));
        let cfg = JobConfig::default();
        let cancel = CancelToken::new();
        cancel.cancel();
        let (id, _) = manager().register("q".into(), &cfg);
        let err = run_with_tools(&id, "q".into(), cfg, provider, "mock".into(), cancel, tools)
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("cancelled"));
    }

    /// Empty next-subtopics list = LLM signals "no more to search" =
    /// natural stop, even if score is below threshold.
    #[tokio::test]
    async fn empty_next_subtopics_breaks_loop() {
        let _g = scoped_home();
        let provider = Arc::new(MockProvider::new(vec![
            "1. a\n2. b",                       // extract initial
            "score: 0.4\nincomplete but stuck", // eval (iter 1)
            "",                                 // next subtopics — empty
            "Final synth",                      // synthesize
            "slug",                             // derive_topic_slug
        ]));
        let tools = Arc::new(MockTools::new(
            vec![vec![hit("S", "https://s.example")]; 5],
            vec!["b"; 10],
        ));
        let cfg = JobConfig {
            min_iter: 1, // floor=1 so iter 1 can early-exit if next-subtopics empty
            max_iter: 8,
            score_threshold: 0.9,
            subtopics_per_iter: 2,
            fetch_top_n: 1,
            llm_timeout: std::time::Duration::from_secs(5),
            time_budget: std::time::Duration::from_secs(60),
            kms_target: None,
        };
        let cancel = CancelToken::new();
        let (id, _) = manager().register("q".into(), &cfg);
        run_with_tools(&id, "q".into(), cfg, provider, "mock".into(), cancel, tools)
            .await
            .unwrap();
    }

    #[test]
    fn accumulate_dedupes_by_url() {
        let mut sources = Vec::new();
        accumulate(
            &mut sources,
            vec![
                hit("A", "https://a.example"),
                hit("B", "https://b.example"),
                hit("A again", "https://a.example"),
            ],
            None,
        );
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].index, 1);
        assert_eq!(sources[1].index, 2);
    }

    #[test]
    fn accumulate_truncates_long_bodies() {
        let mut sources = Vec::new();
        let big_body = "x".repeat(10_000);
        accumulate(
            &mut sources,
            vec![SearchHit {
                title: "big".into(),
                url: "https://big.example".into(),
                snippet: big_body,
            }],
            None,
        );
        assert!(sources[0].body.len() <= MAX_SOURCE_BODY_CHARS + 4); // + 4 for "…" UTF-8
        assert!(sources[0].body.ends_with('…'));
    }

    #[test]
    fn accumulate_skips_empty_urls() {
        let mut sources = Vec::new();
        accumulate(
            &mut sources,
            vec![SearchHit {
                title: "no url".into(),
                url: "".into(),
                snippet: "body".into(),
            }],
            None,
        );
        assert!(sources.is_empty());
    }

    #[test]
    fn simple_slug_basic() {
        assert_eq!(simple_slug("Hello World"), "hello-world");
        assert_eq!(simple_slug("foo  --  bar"), "foo-bar");
        assert_eq!(simple_slug("  trim  "), "trim");
        assert_eq!(simple_slug("###"), "query"); // fallback
    }

    #[test]
    fn simple_slug_caps_at_60() {
        let long_query = "the quick brown fox jumps over the lazy dog and then continues running through the forest";
        assert!(simple_slug(long_query).len() <= 60);
    }

    #[test]
    fn production_tools_constructs() {
        // Smoke test only — actual search/fetch require network +
        // configured keys, exercised via end-to-end manual testing.
        let _t = production_tools();
    }

    #[test]
    fn parse_numbered_entry_basic() {
        let (t, u) = parse_numbered_entry("1. Hello world (https://example.com/a)").unwrap();
        assert_eq!(t, "Hello world");
        assert_eq!(u, "https://example.com/a");
    }

    #[test]
    fn parse_numbered_entry_multidigit_index() {
        let (t, u) = parse_numbered_entry("12. Title (https://x.example)").unwrap();
        assert_eq!(t, "Title");
        assert_eq!(u, "https://x.example");
    }

    #[test]
    fn parse_numbered_entry_url_with_path_and_query() {
        let (t, u) = parse_numbered_entry(
            "3. Some long title with parens (extra) (https://example.com/path?q=1&r=2)",
        )
        .unwrap();
        assert_eq!(t, "Some long title with parens (extra)");
        assert_eq!(u, "https://example.com/path?q=1&r=2");
    }

    #[test]
    fn parse_numbered_entry_rejects_non_numbered() {
        assert!(parse_numbered_entry("Hello world").is_none());
        assert!(parse_numbered_entry("- bullet (https://x.example)").is_none());
        assert!(parse_numbered_entry("1. no url here").is_none());
    }

    #[test]
    fn parse_websearch_markdown_full_output() {
        let body = "Source: Tavily (web search)\n\
                    \n\
                    Answer: Some synthesized answer.\n\
                    \n\
                    1. First title (https://a.example)\n   \
                    First snippet line\n   \
                    second snippet line\n\
                    2. Second title (https://b.example)\n   \
                    Second snippet";
        let hits = parse_websearch_markdown(body);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "First title");
        assert_eq!(hits[0].url, "https://a.example");
        assert!(hits[0].snippet.contains("First snippet line"));
        assert!(hits[0].snippet.contains("second snippet line"));
        assert_eq!(hits[1].title, "Second title");
        assert_eq!(hits[1].url, "https://b.example");
    }

    #[test]
    fn parse_websearch_markdown_no_results() {
        let body = "Source: DuckDuckGo (web search)\n\nNo results found.";
        let hits = parse_websearch_markdown(body);
        assert!(hits.is_empty());
    }

    #[test]
    fn parse_websearch_markdown_after_fallback() {
        let body = "Source: DuckDuckGo (web search) — fallback after tavily: HTTP 429\n\
                    \n\
                    1. Only title (https://x.example)\n   \
                    snippet";
        let hits = parse_websearch_markdown(body);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "Only title");
    }

    #[allow(dead_code)]
    fn touch_message_assoc(_: Message) {}
}
