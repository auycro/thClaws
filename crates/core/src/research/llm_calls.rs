//! Non-streaming LLM helpers for the research pipeline.
//!
//! Each function below is a single LLM call: build a prompt, drain the
//! `Provider::stream()` for `TextDelta` events into a String, parse,
//! return a typed result. No tool use, no agent loop, no message
//! history — research uses the LLM as a pure callable.
//!
//! The 4 calls per iteration:
//! - [`extract_subtopics`] — query + seed → initial subtopics list
//! - [`extract_next_subtopics`] — query + accumulated + eval notes →
//!   next-round subtopics (close gaps named in notes)
//! - [`evaluate`] — query + accumulated → completeness score (0-1) +
//!   free-form notes
//! - [`synthesize`] — query + accumulated → final markdown with `[N]`
//!   citations + URL list
//!
//! All four are robust to LLM weirdness: if parsing fails, fall back
//! to a sensible default rather than aborting the pipeline. The score
//! parser accepts both `score: 0.7` and `Score: 0.7` (any case) on the
//! first non-empty line; the subtopics parsers accept `1.` or `-` or
//! `*` bullets.
//!
//! Per-call timeout is enforced via `tokio::time::timeout` — provider
//! hangs don't stall the whole pipeline.

use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::providers::{Provider, ProviderEvent, StreamRequest};
use crate::types::Message;
use futures::StreamExt;
use std::time::Duration;

/// One search result harvested by the pipeline. Pared down from the
/// full WebSearch tool output to what the LLM needs for synthesis.
/// Indexed citations refer to entries in this list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchSource {
    /// `1`-based citation index, stable for the lifetime of the job.
    pub index: u32,
    pub title: String,
    pub url: String,
    /// Combined snippet + (if WebFetch was run) extracted body excerpt.
    /// Truncated by the pipeline to `MAX_SOURCE_BODY_CHARS`.
    pub body: String,
}

/// LLM verdict on accumulated results — a continuous score in 0.0-1.0
/// plus free-form prose notes that feed the next round's subtopic
/// generation.
#[derive(Debug, Clone, PartialEq)]
pub struct Evaluation {
    pub score: f32,
    /// Free-form prose. Used as input to [`extract_next_subtopics`]
    /// when the loop continues; surfaced in the final synthesis as
    /// "research notes" too.
    pub notes: String,
}

// ── Public API ───────────────────────────────────────────────────────

/// Generate the first round of subtopics from a broad query + seed
/// search results. Returns `n` subtopic strings (clamped at the
/// configured limit). Falls back to `[query]` if parsing yields zero
/// subtopics.
pub async fn extract_subtopics(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    seed: &[ResearchSource],
    n: u32,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<String>> {
    let prompt = build_extract_subtopics_prompt(query, seed, n);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let mut topics = parse_bulleted_list(&raw);
    topics.truncate(n as usize);
    if topics.is_empty() {
        topics.push(query.to_string());
    }
    Ok(topics)
}

/// Generate next-round subtopics targeting gaps named in `eval_notes`.
/// Same shape as [`extract_subtopics`] but conditioned on accumulated
/// research + the LLM's prior evaluation. Returns empty list when the
/// LLM can't think of anything more — pipeline takes that as a
/// natural stop signal.
pub async fn extract_next_subtopics(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    eval_notes: &str,
    n: u32,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Vec<String>> {
    let prompt = build_extract_next_subtopics_prompt(query, sources, eval_notes, n);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let mut topics = parse_bulleted_list(&raw);
    topics.truncate(n as usize);
    Ok(topics)
}

/// Score completeness of the accumulated source set against the query.
/// Returns a value in 0.0-1.0 plus free-form notes. The pipeline uses
/// `score >= threshold` AND `iter >= min_iter` as the early-stop
/// signal; notes feed [`extract_next_subtopics`] when the loop
/// continues.
///
/// The contract: the LLM's first non-empty line MUST start with
/// `score: <float>`. Everything after is treated as notes prose.
/// Parsing is permissive (case-insensitive, `0.7` or `7/10` accepted)
/// but if no number is found, defaults to `0.0` so the loop continues
/// rather than spuriously stopping.
pub async fn evaluate(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<Evaluation> {
    let prompt = build_evaluate_prompt(query, sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    let (score, notes) = parse_score_and_notes(&raw);
    Ok(Evaluation { score, notes })
}

/// Final markdown report synthesizing accumulated sources into a
/// coherent answer with `[N]` inline citations + URL list at the end.
/// The pipeline writes this verbatim to the KMS.
pub async fn synthesize(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    sources: &[ResearchSource],
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_synthesize_prompt(query, sources);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(raw.trim().to_string())
}

/// Derive a kebab-case KMS slug from a free-form query (max ~5 words).
/// Used when `JobConfig::kms_target` is `None`. LLM is the right tool
/// here because the query may be in any language; deriving a clean
/// English/transliterated slug from Thai prose with regex is fragile.
pub async fn derive_topic_slug(
    provider: &dyn Provider,
    model: &str,
    query: &str,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let prompt = build_derive_slug_prompt(query);
    let raw = oneshot(provider, model, prompt, timeout, cancel).await?;
    Ok(sanitize_slug(&raw))
}

// ── Prompt builders ──────────────────────────────────────────────────

fn build_extract_subtopics_prompt(query: &str, seed: &[ResearchSource], n: u32) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Research query: {query}\n\n\
         Seed search results ({} sources):\n",
        seed.len()
    ));
    for r in seed.iter().take(10) {
        s.push_str(&format!(
            "- {} ({})\n  {}\n",
            r.title,
            r.url,
            snippet(&r.body, 200)
        ));
    }
    s.push_str(&format!(
        "\nGenerate {n} focused subtopics to research in parallel. \
         Each subtopic should be a specific search-engine query (not a question), \
         covering a distinct facet of the original query.\n\n\
         Output ONE subtopic per line, numbered or bulleted, no commentary. Example:\n\
         1. quantum error correction surface codes\n\
         2. superconducting qubit decoherence mechanisms\n\
         3. ...\n"
    ));
    s
}

fn build_extract_next_subtopics_prompt(
    query: &str,
    sources: &[ResearchSource],
    eval_notes: &str,
    n: u32,
) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Accumulated so far ({} sources):\n",
        sources.len()
    ));
    for r in sources.iter().take(20) {
        s.push_str(&format!("[{}] {} ({})\n", r.index, r.title, r.url));
    }
    s.push_str(&format!(
        "\nPrior evaluation notes:\n{eval_notes}\n\n\
         Generate {n} next-round search queries to fill the gaps named above. \
         Avoid topics already well-covered. If no productive next searches come \
         to mind, output an empty response (the loop will stop).\n\n\
         Output ONE query per line, numbered or bulleted, no commentary."
    ));
    s
}

fn build_evaluate_prompt(query: &str, sources: &[ResearchSource]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Accumulated sources ({} total):\n",
        sources.len()
    ));
    for r in sources.iter().take(40) {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 300)
        ));
    }
    s.push_str(
        "Rate how completely the accumulated sources answer the query, \
         considering:\n\
         - **Source quantity**: enough distinct authoritative URLs (more is better)\n\
         - **Source diversity**: different domains / publishers, not all from one site\n\
         - **Fact verification**: key claims supported by 2+ sources, no obvious contradictions\n\
         - **Direct relevance**: sources address the query, not tangents\n\n\
         Output format — STRICT, the first non-empty line must be:\n\
         `score: 0.NN`\n\
         (a single number 0.0 to 1.0, where 1.0 means a complete, well-verified answer is possible)\n\n\
         Then on subsequent lines, write free-form notes explaining:\n\
         - What's covered well\n\
         - What's missing or under-supported\n\
         - Specific subtopics worth searching next\n\n\
         Be honest about uncertainty — if claims aren't well-verified, say so and score lower.",
    );
    s
}

fn build_synthesize_prompt(query: &str, sources: &[ResearchSource]) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "Original query: {query}\n\n\
         Sources ({} total):\n",
        sources.len()
    ));
    for r in sources {
        s.push_str(&format!(
            "[{}] {} ({})\n   {}\n\n",
            r.index,
            r.title,
            r.url,
            snippet(&r.body, 500)
        ));
    }
    s.push_str(
        "Write a comprehensive answer to the original query as Markdown. \
         Requirements:\n\
         - **Lead with a 1-2 sentence ABSTRACT** as the very first prose \
         line, BEFORE any `##` subtopic headings, before any list bullets, \
         before any blockquotes. The abstract must name the topic + state \
         your overall finding in one self-contained sentence (e.g. \
         \"OBON is a Japanese summer festival honoring ancestors, observed \
         13–16 August with regional variation in customs and dates [1][3].\"). \
         Do NOT start with a heading like `# Research:` or `## Overview` — \
         start directly with prose. This abstract is extracted verbatim as \
         the KMS index summary; if it's a heading or a meta-line, the index \
         loses signal.\n\
         - At least 200 words total\n\
         - Inline `[N]` citations after every factual claim, mapping to the source list above\n\
         - Cite multiple sources for important claims when available\n\
         - After the abstract, use Markdown headings (`##`) to organize by subtopic\n\
         - When sources contradict, surface the disagreement rather than picking arbitrarily\n\n\
         End with a `## Sources` section listing every cited source as `[N] <url>` (one per line). \
         Answer in the same language as the original query.",
    );
    s
}

fn build_derive_slug_prompt(query: &str) -> String {
    format!(
        "Convert this research query to a short kebab-case slug suitable as a \
         knowledge-base name. 2-5 lowercase ASCII words separated by hyphens. \
         No quotes, no punctuation, no commentary — just the slug.\n\n\
         Examples:\n\
         - 'How to use Tavily API' → tavily-api\n\
         - 'ค้นหาข่าว OBON' → obon-festival-news\n\
         - 'recent advances in quantum error correction' → quantum-error-correction\n\n\
         Query: {query}\n\
         Slug:"
    )
}

// ── Parsing helpers ──────────────────────────────────────────────────

/// Parse a numbered/bulleted list out of LLM prose. Accepts:
/// - `1. topic`
/// - `1) topic`
/// - `- topic`
/// - `* topic`
/// - bare `topic` lines (non-empty, non-prefixed)
///
/// Strips trailing punctuation and whitespace. Skips blank lines and
/// lines that look like commentary (start with `#`, `Note:`, etc.).
fn parse_bulleted_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // Skip Markdown headers and obvious commentary preambles.
        if line.starts_with('#')
            || line.to_ascii_lowercase().starts_with("note:")
            || line.to_ascii_lowercase().starts_with("here are")
        {
            continue;
        }
        let stripped = strip_list_marker(line);
        let cleaned = stripped.trim_end_matches(|c: char| c == '.' || c == ',' || c == ';');
        if !cleaned.is_empty() {
            out.push(cleaned.to_string());
        }
    }
    out
}

fn strip_list_marker(line: &str) -> &str {
    let mut chars = line.char_indices();
    if let Some((_, c)) = chars.next() {
        // Numbered: `1.`, `12.`, `1)`, `12)`
        if c.is_ascii_digit() {
            let mut last_digit_end = c.len_utf8();
            for (i, c2) in chars.by_ref() {
                if c2.is_ascii_digit() {
                    last_digit_end = i + c2.len_utf8();
                } else if c2 == '.' || c2 == ')' {
                    return line[i + c2.len_utf8()..].trim_start();
                } else {
                    break;
                }
            }
            // No marker found — keep the line as-is
            let _ = last_digit_end;
            return line;
        }
        // Bullet
        if c == '-' || c == '*' || c == '•' {
            return line[c.len_utf8()..].trim_start();
        }
    }
    line
}

/// Extract `score: 0.X` from the first non-empty line, return remainder
/// as notes. Permissive: accepts any case (`Score`, `SCORE`), accepts
/// `7/10` style as `0.7`, defaults to 0.0 on parse failure (so the
/// loop keeps going rather than spuriously stopping).
fn parse_score_and_notes(text: &str) -> (f32, String) {
    let mut lines = text.lines();
    let first = loop {
        match lines.next() {
            Some(l) if l.trim().is_empty() => continue,
            Some(l) => break l,
            None => return (0.0, String::new()),
        }
    };
    let lower = first.trim().to_ascii_lowercase();
    let (score, first_was_score_line) = if let Some(rest) = lower.strip_prefix("score:") {
        (parse_score_value(rest.trim()).unwrap_or(0.0), true)
    } else if let Some(rest) = lower.strip_prefix("score") {
        // Accept `score 0.7` (no colon) defensively. Only count as a
        // score line if the rest actually parses to a number — bare
        // prose starting with the word "Score" shouldn't get eaten.
        match parse_score_value(rest.trim()) {
            Some(v) => (v, true),
            None => (find_inline_score(text).unwrap_or(0.0), false),
        }
    } else {
        // No `score:` prefix — try to find one elsewhere in the body,
        // and keep the first line as part of the notes (it's prose
        // the LLM produced; losing it would discard signal the next
        // round wants).
        (find_inline_score(text).unwrap_or(0.0), false)
    };
    let mut notes_lines: Vec<&str> = lines.collect();
    if !first_was_score_line {
        notes_lines.insert(0, first);
    }
    let notes = notes_lines.join("\n").trim().to_string();
    (clamp01(score), notes)
}

fn find_inline_score(text: &str) -> Option<f32> {
    for line in text.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(idx) = lower.find("score:") {
            let rest = &line[idx + "score:".len()..];
            if let Some(s) = parse_score_value(rest.trim()) {
                return Some(s);
            }
        }
    }
    None
}

fn parse_score_value(s: &str) -> Option<f32> {
    // Accept `0.7`, `0.75`, `7/10`, `75%`, `75 / 100`.
    let token: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.' || *c == '/' || *c == '%')
        .collect();
    if token.is_empty() {
        return None;
    }
    if let Some((num, denom)) = token.split_once('/') {
        let num: f32 = num.parse().ok()?;
        let denom: f32 = denom.parse().ok()?;
        if denom > 0.0 {
            return Some(num / denom);
        }
        return None;
    }
    if let Some(stripped) = token.strip_suffix('%') {
        let n: f32 = stripped.parse().ok()?;
        return Some(n / 100.0);
    }
    token.parse().ok()
}

fn clamp01(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

fn sanitize_slug(raw: &str) -> String {
    let lower = raw.trim().to_ascii_lowercase();
    let mut out = String::new();
    let mut prev_dash = true;
    for c in lower.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            prev_dash = false;
        } else if (c == '-' || c == ' ' || c == '_') && !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "research".into()
    } else {
        trimmed
    }
}

fn snippet(body: &str, max: usize) -> String {
    if body.len() <= max {
        body.to_string()
    } else {
        let mut end = max;
        while !body.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}…", &body[..end])
    }
}

// ── Provider oneshot wrapper ────────────────────────────────────────

/// Drive a single LLM call to completion: build a `StreamRequest` with
/// no tools / no system prompt / one user message, drain the stream
/// for `TextDelta` events into a String, return. Cancellation (via
/// `CancelToken`) and timeout are honored; both produce
/// `Error::Tool("…cancelled" / "…timed out")`.
async fn oneshot(
    provider: &dyn Provider,
    model: &str,
    prompt: String,
    timeout: Duration,
    cancel: &CancelToken,
) -> Result<String> {
    let req = StreamRequest {
        model: model.to_string(),
        system: None,
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
        max_tokens: 4096,
        thinking_budget: None,
    };
    let stream_fut = provider.stream(req);
    let mut stream = match tokio::time::timeout(timeout, stream_fut).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(Error::Tool(
                "research LLM call timed out building stream".into(),
            ))
        }
    };

    let mut text = String::new();
    loop {
        if cancel.is_cancelled() {
            return Err(Error::Tool("research LLM call cancelled".into()));
        }
        let next = tokio::select! {
            ev = tokio::time::timeout(timeout, stream.next()) => ev,
            _ = cancel.cancelled() => {
                return Err(Error::Tool("research LLM call cancelled".into()));
            }
        };
        match next {
            Ok(Some(Ok(ProviderEvent::TextDelta(s)))) => text.push_str(&s),
            Ok(Some(Ok(ProviderEvent::MessageStop { .. }))) => break,
            Ok(Some(Ok(_))) => {} // ignore non-text events (tool use, thinking, etc.)
            Ok(Some(Err(e))) => return Err(e),
            Ok(None) => break, // stream ended without explicit MessageStop
            Err(_) => {
                return Err(Error::Tool(
                    "research LLM call timed out reading stream".into(),
                ))
            }
        }
    }
    Ok(text)
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_bulleted_list ────────────────────────────────────────

    #[test]
    fn parse_bulleted_list_handles_numbered() {
        let v = parse_bulleted_list(
            "1. quantum error correction\n2. superconducting qubits\n3. surface codes",
        );
        assert_eq!(
            v,
            vec![
                "quantum error correction",
                "superconducting qubits",
                "surface codes"
            ]
        );
    }

    #[test]
    fn parse_bulleted_list_handles_dash_and_star() {
        let v = parse_bulleted_list("- alpha\n* beta\n• gamma\n- delta.");
        assert_eq!(v, vec!["alpha", "beta", "gamma", "delta"]);
    }

    #[test]
    fn parse_bulleted_list_skips_headers_and_commentary() {
        let v = parse_bulleted_list(
            "Here are the subtopics:\n# Subtopics\n1. alpha\nNote: be careful\n2. beta",
        );
        assert_eq!(v, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_bulleted_list_accepts_paren_form() {
        let v = parse_bulleted_list("1) alpha\n2) beta");
        assert_eq!(v, vec!["alpha", "beta"]);
    }

    #[test]
    fn parse_bulleted_list_returns_empty_for_empty_input() {
        assert!(parse_bulleted_list("").is_empty());
        assert!(parse_bulleted_list("\n\n  \n").is_empty());
    }

    // ── parse_score_and_notes ─────────────────────────────────────

    #[test]
    fn parse_score_handles_decimal() {
        let (s, n) = parse_score_and_notes("score: 0.7\n\nWhat's covered: …");
        assert!((s - 0.7).abs() < 1e-6);
        assert!(n.starts_with("What's covered"));
    }

    #[test]
    fn parse_score_handles_case_insensitive() {
        let (s, _) = parse_score_and_notes("Score: 0.85");
        assert!((s - 0.85).abs() < 1e-6);
        let (s2, _) = parse_score_and_notes("SCORE: 0.42");
        assert!((s2 - 0.42).abs() < 1e-6);
    }

    #[test]
    fn parse_score_handles_fraction_form() {
        let (s, _) = parse_score_and_notes("score: 7/10\nnotes here");
        assert!((s - 0.7).abs() < 1e-6);
    }

    #[test]
    fn parse_score_handles_percent_form() {
        let (s, _) = parse_score_and_notes("score: 75%\nnotes");
        assert!((s - 0.75).abs() < 1e-6);
    }

    #[test]
    fn parse_score_clamps_above_one() {
        let (s, _) = parse_score_and_notes("score: 1.4\nover-eager LLM");
        assert!((s - 1.0).abs() < 1e-6);
    }

    #[test]
    fn parse_score_defaults_to_zero_on_failure() {
        let (s, n) = parse_score_and_notes("I think the answer is great");
        assert_eq!(s, 0.0);
        // Whole text becomes notes when no score line is found.
        assert!(n.contains("great"));
    }

    #[test]
    fn parse_score_finds_inline_when_first_line_isnt_score() {
        let (s, _) = parse_score_and_notes(
            "Coverage looks decent.\nSeveral sources agree.\nscore: 0.6\nnotes",
        );
        assert!((s - 0.6).abs() < 1e-6);
    }

    #[test]
    fn parse_score_skips_leading_blanks() {
        let (s, _) = parse_score_and_notes("\n\n   \nscore: 0.55");
        assert!((s - 0.55).abs() < 1e-6);
    }

    // ── sanitize_slug ──────────────────────────────────────────────

    #[test]
    fn slug_handles_thai_to_default() {
        // LLM SHOULD return ASCII; if it doesn't, we degrade to
        // "research" rather than emit non-ASCII filenames.
        assert_eq!(sanitize_slug("ค้นหาข่าว"), "research");
    }

    #[test]
    fn slug_kebab_cases_words() {
        assert_eq!(
            sanitize_slug("Quantum Error Correction"),
            "quantum-error-correction"
        );
        assert_eq!(sanitize_slug("tavily_api_usage"), "tavily-api-usage");
    }

    #[test]
    fn slug_strips_leading_trailing_hyphens() {
        assert_eq!(sanitize_slug("--alpha-beta--"), "alpha-beta");
    }

    #[test]
    fn slug_collapses_consecutive_separators() {
        assert_eq!(sanitize_slug("foo   bar___baz"), "foo-bar-baz");
    }

    // ── snippet ────────────────────────────────────────────────────

    #[test]
    fn snippet_truncates_at_char_boundary() {
        let s = snippet("hello world this is too long", 11);
        assert!(s.ends_with('…'));
        // Truncation must not split a multibyte char.
        let _ = s.chars().count(); // doesn't panic
    }

    #[test]
    fn snippet_passes_through_short_strings() {
        assert_eq!(snippet("short", 100), "short");
    }

    /// M6.39.5: pin the synthesize prompt's "lead with abstract"
    /// instruction. The KMS auto-index pulls a page's summary from
    /// `first_meaningful_line(body)`; that line MUST be substantive
    /// prose (the LLM-written abstract) for the index to be useful
    /// to the next session deciding which page to KmsRead. If this
    /// instruction drifts away the index regresses to "Research:
    /// <query>"-style summaries that signal nothing.
    #[test]
    fn synthesize_prompt_requires_abstract_first() {
        let prompt = build_synthesize_prompt("what is OBON", &[]);
        // The instruction must mention "abstract" specifically.
        let lower = prompt.to_ascii_lowercase();
        assert!(
            lower.contains("abstract"),
            "synthesize prompt must require an abstract"
        );
        // It must mention that the abstract goes BEFORE headings
        // (otherwise LLM may emit `## Overview\n<abstract>` which
        // first_meaningful_line strips back to `Overview`).
        assert!(
            lower.contains("before any `##`") || lower.contains("before any ##"),
            "synthesize prompt must say abstract goes before ## headings"
        );
        // It must explicitly forbid leading with `# Research:` or a
        // heading — the pre-fix body shape produced exactly that.
        assert!(
            lower.contains("do not start with a heading"),
            "synthesize prompt must forbid heading-first opening"
        );
    }
}
