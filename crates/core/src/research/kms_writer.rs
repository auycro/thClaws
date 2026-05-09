//! Write a research result to KMS as a timestamped raw note + update
//! the topic-level `_summary.md` consolidated view.
//!
//! Two-step write so consolidation can never lose data:
//! 1. **Raw note** — `<YYYY-MM-DD>-<query-slug>.md` with the full
//!    synthesized markdown verbatim. Timestamped filename means
//!    repeated `/research` calls on similar queries don't collide.
//! 2. **Summary** — `_summary.md` updated to reference the raw note
//!    (in M6.39.1, just appends a bullet; M6.39.2+ may add an LLM
//!    consolidation pass that merges raw notes into a coherent
//!    summary).
//!
//! KMS resolution: if `kms_target` is provided and exists, use it;
//! otherwise create a project-scoped KMS with that name. If no target
//! given, caller supplies an LLM-derived slug via
//! [`super::llm_calls::derive_topic_slug`].

use crate::error::Result;
use crate::kms::{self, KmsRef, KmsScope};
use std::path::PathBuf;

/// Result of a successful KMS write.
#[derive(Debug, Clone)]
pub struct WriteOutcome {
    /// Filename (no path) of the timestamped raw note inside the KMS.
    pub raw_note: String,
    /// Absolute path on disk where it landed (useful for log lines).
    pub raw_note_path: PathBuf,
    /// Name of the KMS the note went to (handy when caller passed
    /// `None` and we auto-derived).
    pub kms_name: String,
}

/// Build the raw note filename from today + a query slug. Format:
/// `2026-05-09-obon-festival.md`. Timestamp prefix sorts naturally so
/// `ls` shows newest at the bottom.
pub fn make_raw_filename(today: &str, query_slug: &str) -> String {
    format!("{today}-{}", query_slug)
}

/// Resolve or create the target KMS. Always project-scoped — research
/// is workspace-specific by default. M6.39.2 may add `--user-kms` to
/// flip to user scope.
fn resolve_or_create_kms(name: &str) -> Result<KmsRef> {
    if let Some(k) = kms::resolve(name) {
        return Ok(k);
    }
    kms::create(name, KmsScope::Project)
}

/// Write the synthesized result + update summary. Returns the relative
/// raw-note filename (caller surfaces as `JobView::result_page`).
pub fn write(
    kms_name: &str,
    query: &str,
    query_slug: &str,
    today: &str,
    synthesized: &str,
) -> Result<WriteOutcome> {
    let kref = resolve_or_create_kms(kms_name)?;
    let filename = make_raw_filename(today, query_slug);
    let body = build_raw_note_body(query, today, synthesized);
    let raw_path = kms::write_page(&kref, &filename, &body)?;
    update_summary(&kref, today, query, &filename)?;
    Ok(WriteOutcome {
        raw_note: format!("{filename}.md"),
        raw_note_path: raw_path,
        kms_name: kref.name.clone(),
    })
}

/// Compose the raw note body with frontmatter that the KMS schema-aware
/// lint (M6.37) can validate against. The `type: research` discriminator
/// lets users filter `KmsSearch` to research notes only.
fn build_raw_note_body(query: &str, today: &str, synthesized: &str) -> String {
    // M6.39.5: body leads with synthesized content directly. Pre-fix
    // the body opened with `# Research: <query>` H1 + `**Query:**` +
    // `**Date:**` lines that just restated frontmatter. The KMS
    // auto-index pulls each page's "summary" from
    // `first_meaningful_line(body)`, which strips Markdown markers
    // and returned just `Research: <query>` — useless for the LLM
    // trying to decide whether the page is relevant.
    //
    // With this layout, the synthesize prompt's required 1-2
    // sentence abstract becomes the first meaningful line, and the
    // index summary actually describes what's inside.
    let _ = query;
    format!(
        "---\n\
         title: \"Research: {}\"\n\
         type: research\n\
         query: \"{}\"\n\
         created: {today}\n\
         updated: {today}\n\
         ---\n\n\
         {}\n",
        escape_yaml_string(query),
        escape_yaml_string(query),
        synthesized.trim()
    )
}

/// Append a one-line bullet to `_summary.md` so users have an index of
/// research entries in this KMS without opening each file. M6.39.1
/// keeps it dumb (chronological bullet list); future revs can add an
/// LLM consolidation pass over the bullets.
fn update_summary(kref: &KmsRef, today: &str, query: &str, raw_filename: &str) -> Result<()> {
    let line = format!(
        "- {today} — [{}]({}.md) — {}\n",
        truncate_for_summary(query, 80),
        raw_filename,
        raw_filename
    );
    let summary_name = "_summary";
    // KMS pages live under `<root>/pages/<stem>.md` — checking
    // `root.join("_summary.md")` (an earlier version of this check)
    // always returned "missing" and triggered a destructive
    // `write_page` (create-or-replace) on the second call, wiping
    // the first call's bullet. The real existence check is in
    // pages_dir.
    let summary_path = kref.pages_dir().join(format!("{summary_name}.md"));
    if !summary_path.exists() {
        let body = format!(
            "---\n\
             title: \"Research summary\"\n\
             type: research-summary\n\
             updated: {today}\n\
             ---\n\n\
             # Research summary\n\n\
             Auto-maintained index of research notes in this knowledge base. \
             Each line links to the timestamped raw note generated by `/research`.\n\n\
             {line}"
        );
        kms::write_page(kref, summary_name, &body)?;
    } else {
        kms::append_to_page(kref, summary_name, &line)?;
    }
    Ok(())
}

fn truncate_for_summary(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i >= max - 1 {
            out.push('…');
            break;
        }
        out.push(c);
    }
    out
}

/// YAML string escaping — research queries can contain quotes,
/// backslashes, control chars. Keep it simple: escape `"` and `\` and
/// drop ASCII control chars (newlines etc).
fn escape_yaml_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Bridge into the system-time → date-string conversion used by the
/// rest of KMS. Callers in tests pass an explicit date; production
/// uses [`crate::usage::today_str`].
pub fn today_str() -> String {
    crate::usage::today_str()
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::research::test_helpers::scoped_home;

    #[test]
    fn make_raw_filename_format() {
        assert_eq!(
            make_raw_filename("2026-05-09", "obon-festival"),
            "2026-05-09-obon-festival"
        );
    }

    #[test]
    fn yaml_escape_handles_quotes_and_backslashes() {
        assert_eq!(escape_yaml_string(r#"hello "world""#), r#"hello \"world\""#);
        assert_eq!(escape_yaml_string(r"path\to\file"), r"path\\to\\file");
    }

    #[test]
    fn yaml_escape_strips_control_chars() {
        let s = "line one\nline two\tindented";
        let escaped = escape_yaml_string(s);
        assert!(!escaped.contains('\n'));
        assert!(!escaped.contains('\t'));
        assert!(escaped.contains("line one line two indented"));
    }

    #[test]
    fn truncate_for_summary_preserves_short_strings() {
        assert_eq!(truncate_for_summary("short", 50), "short");
    }

    #[test]
    fn truncate_for_summary_adds_ellipsis_when_long() {
        let s = "a".repeat(100);
        let t = truncate_for_summary(&s, 20);
        assert_eq!(t.chars().count(), 20);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_for_summary_handles_unicode() {
        let s = "ค้นหาข่าวล่าสุดเกี่ยวกับเทคโนโลยี";
        let t = truncate_for_summary(s, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn build_raw_note_includes_required_frontmatter_keys() {
        let body = build_raw_note_body("test query", "2026-05-09", "Some synthesized content");
        assert!(body.starts_with("---\n"));
        assert!(body.contains("title: \"Research: test query\""));
        assert!(body.contains("type: research"));
        assert!(body.contains("query: \"test query\""));
        assert!(body.contains("created: 2026-05-09"));
        assert!(body.contains("updated: 2026-05-09"));
        assert!(body.contains("Some synthesized content"));
    }

    /// M6.39.5: pin the post-fix body shape so the synthesized
    /// abstract becomes the first non-frontmatter line. Pre-fix the
    /// body led with `# Research: <query>` H1 + `**Query:**` /
    /// `**Date:**` lines that just restated frontmatter — kms's
    /// `first_meaningful_line` indexer would pull `Research: <query>`
    /// as the page summary, useless for the LLM picking pages to read.
    #[test]
    fn build_raw_note_body_starts_with_synthesized_content_after_frontmatter() {
        let synthesized = "OBON is a Japanese summer festival honoring \
                           ancestors [1][3].\n\n## History\n\nMore content here.";
        let body = build_raw_note_body("what is OBON", "2026-05-09", synthesized);
        // Body MUST NOT contain the old preamble — those were the
        // exact things crowding out the abstract.
        assert!(
            !body.contains("# Research: what is OBON"),
            "body should not lead with H1 — frontmatter title covers it"
        );
        assert!(
            !body.contains("**Query:**"),
            "body should not restate Query — frontmatter has `query:`"
        );
        assert!(
            !body.contains("**Date:**"),
            "body should not restate Date — frontmatter has `created:`/`updated:`"
        );
        // The first non-frontmatter line must be the abstract prose.
        let after_fm = body.splitn(2, "---\n\n").nth(1).unwrap();
        let first_line = after_fm.lines().next().unwrap();
        assert!(
            first_line.starts_with("OBON is a Japanese"),
            "first non-frontmatter line should be the abstract, got: {first_line}"
        );
    }

    /// Integration test: write to a temp KMS and verify both files land
    /// with the right shape. Uses the same `scoped_home` helper pattern
    /// as `kms::tests`.
    #[test]
    fn write_creates_raw_and_summary_in_fresh_kms() {
        let _g = scoped_home();
        let outcome = write(
            "research-test-kms",
            "what is OBON",
            "obon-festival",
            "2026-05-09",
            "Obon is a Japanese festival [1].\n\n## Sources\n[1] https://example.com\n",
        )
        .unwrap();
        assert_eq!(outcome.raw_note, "2026-05-09-obon-festival.md");
        assert_eq!(outcome.kms_name, "research-test-kms");
        assert!(outcome.raw_note_path.exists());
        let raw = std::fs::read_to_string(&outcome.raw_note_path).unwrap();
        assert!(raw.contains("type: research"));
        assert!(raw.contains("https://example.com"));

        // Summary also exists with the expected bullet entry.
        let kref = kms::resolve("research-test-kms").unwrap();
        let summary_path = kref.pages_dir().join("_summary.md");
        assert!(summary_path.exists(), "summary should be created");
        let summary = std::fs::read_to_string(&summary_path).unwrap();
        assert!(summary.contains("type: research-summary"));
        assert!(summary.contains("2026-05-09 — [what is OBON]"));
        assert!(summary.contains("(2026-05-09-obon-festival.md)"));
    }

    /// End-to-end: write a research note whose body leads with an
    /// abstract, then verify the KMS auto-index picks up that abstract
    /// as the page summary (not "Research: ..." or any meta line).
    /// This is the exact flow that fixes the "LLM ignores KMS"
    /// observation from /system inspection — an informative summary
    /// signals page relevance to the model deciding which page to
    /// KmsRead.
    #[test]
    fn write_index_summary_uses_abstract_not_title() {
        let _g = scoped_home();
        let synthesized = "OBON is a Japanese festival honoring ancestors \
                           observed 13–16 August [1][3].\n\n## History\nmore.";
        write(
            "research-test-index",
            "what is OBON",
            "what-is-obon",
            "2026-05-09",
            synthesized,
        )
        .unwrap();
        let kref = kms::resolve("research-test-index").unwrap();
        let index = std::fs::read_to_string(&kref.index_path()).unwrap();
        // The auto-index should have a bullet for the page using the
        // abstract's first line as the summary, NOT "Research: ..."
        // or "Query: ...".
        assert!(
            index.contains("OBON is a Japanese festival"),
            "index should contain the abstract as page summary, got:\n{index}"
        );
        assert!(
            !index.contains("Research: what is OBON"),
            "index should NOT use the H1 title as summary (that's the pre-fix bug):\n{index}"
        );
    }

    #[test]
    fn second_write_appends_to_existing_summary() {
        let _g = scoped_home();
        write(
            "research-multi",
            "first query",
            "first-query",
            "2026-05-09",
            "first answer",
        )
        .unwrap();
        write(
            "research-multi",
            "second query",
            "second-query",
            "2026-05-10",
            "second answer",
        )
        .unwrap();
        let kref = kms::resolve("research-multi").unwrap();
        let summary = std::fs::read_to_string(kref.pages_dir().join("_summary.md")).unwrap();
        assert!(summary.contains("first query"));
        assert!(summary.contains("second query"));
    }
}
