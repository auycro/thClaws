//! `PdfRead` — extract text from a PDF by shelling out to `pdftotext`
//! (poppler-utils). poppler does the heavy lifting (Thai shaping, ligature
//! decomposition, layout-aware extraction); we just wrap it with sandbox
//! checks, page-range parsing, and a clear missing-binary error.
//!
//! When the PDF is a scanned image (no embedded text layer), pdftotext
//! returns empty / mostly-empty output. In that case the multimodal
//! entry point falls through to `pdftoppm` which renders each requested
//! page as a PNG and returns them as image blocks so the model sees the
//! pages visually. Both binaries ship together in poppler-utils, so the
//! fallback adds no new install requirement.
//!
//! Why shell-out instead of a pure-Rust pdf crate: extraction quality
//! across real-world PDFs (tagged structure, form fields, embedded fonts
//! with non-standard cmaps) is dominated by poppler's twenty-plus years
//! of corner-case handling. The Rust crates that exist are good for
//! valid PDFs but break on the long tail.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use crate::types::{ImageSource, ToolResultBlock, ToolResultContent};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

const EXTRACT_TIMEOUT: Duration = Duration::from_secs(60);

/// Vision-OCR fallback constants. Tuned conservatively — the goal is
/// "make scanned PDFs work at all", not "perfectly handle 100-page
/// scans". Users with bigger documents can paginate via the `pages`
/// parameter.
mod fallback {
    use super::Duration;
    /// If pdftotext returns less than this many non-whitespace chars
    /// per requested page on average, treat the PDF as scanned and
    /// fall through to vision OCR. 50 chars ≈ a one-line title;
    /// anything thinner is almost certainly a scanned image.
    pub const MIN_CHARS_PER_PAGE: usize = 50;
    /// Hard cap on pages to render. Twenty 150-DPI A4 PNGs at typical
    /// content density land around 8-15 MB total before base64 — fits
    /// comfortably under most providers' per-request limits.
    pub const MAX_PAGES_TO_RENDER: u32 = 20;
    /// Render resolution. 150 DPI is the sweet spot for OCR quality
    /// vs file size; below that fine print drops out of the model's
    /// recognition; above that the size grows quadratically with
    /// minimal accuracy gain.
    pub const RENDER_DPI: u32 = 150;
    /// Per-page byte cap. Anthropic's documented per-image limit is
    /// 5 MB; matches the Read tool's MAX_IMAGE_BYTES.
    pub const MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
    /// Render-step timeout. pdftoppm at 150 DPI on a 20-page PDF is
    /// usually ~5s on a modern machine, but spinning rust + complex
    /// fonts can push past 30s.
    pub const RENDER_TIMEOUT: Duration = Duration::from_secs(120);
}

pub struct PdfReadTool;

#[async_trait]
impl Tool for PdfReadTool {
    fn name(&self) -> &'static str {
        "PdfRead"
    }

    fn description(&self) -> &'static str {
        "Extract text from a PDF file. Uses `pdftotext` from poppler-utils. \
         Optional `pages` parameter accepts \"all\" (default), \"3\" \
         (single page), or \"1-5\" (inclusive range). Returns extracted \
         text. **Scanned / image-based PDFs** (no embedded text layer) \
         fall through to a vision-OCR path that renders each requested \
         page as PNG via `pdftoppm` so the model sees the pages directly \
         — no separate OCR step needed. Requires poppler-utils installed \
         (`brew install poppler` on macOS, `apt install poppler-utils` \
         on Debian/Ubuntu)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path":  {"type": "string", "description": "PDF file path."},
                "pages": {"type": "string", "description": "Page range: \"all\", \"N\", or \"M-N\". Default: all."}
            },
            "required": ["path"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;
        extract_text(&validated, first, last).await
    }

    /// Multimodal entry: text-first, vision-OCR fallback for scanned PDFs.
    /// When the model invokes PdfRead via the agent loop (not a direct
    /// `call`), this path runs. If pdftotext returns enough text for
    /// the requested pages it returns a Text block as before. If the
    /// text is empty / sparse (typical scanned PDF) it renders each
    /// page to PNG and returns an Image block per page plus a summary
    /// Text block so the model has both the visual and a textual
    /// handle on what it saw.
    async fn call_multimodal(&self, input: Value) -> Result<ToolResultContent> {
        let validated = crate::sandbox::Sandbox::check(req_str(&input, "path")?)?;
        let pages_spec = input.get("pages").and_then(|v| v.as_str()).unwrap_or("all");
        let (first, last) = parse_page_range(pages_spec)?;

        let text = extract_text(&validated, first, last).await?;

        // Heuristic for "looks scanned": split on form-feed (the
        // page boundary marker pdftotext emits) and average chars per
        // page. The form-feed split also handles "all" pages
        // gracefully without needing a separate pdfinfo call.
        if !text_is_too_sparse(&text) {
            return Ok(ToolResultContent::Text(text));
        }

        // Fall through to vision-OCR.
        render_pages_as_image_blocks(&validated, first, last)
            .await
            .map(|blocks| ToolResultContent::Blocks(blocks))
    }
}

/// True when the extracted text is so thin it almost certainly came
/// from a PDF without an embedded text layer (scanned image). Splits
/// on form-feed (pdftotext's page boundary marker) and checks the
/// average non-whitespace char count per page against
/// `fallback::MIN_CHARS_PER_PAGE`. A single-page PDF with a one-line
/// title (~30 chars) trips the threshold; that's intentional — vision
/// OCR adds little overhead for a single page and meaningfully
/// improves quality on covers / title slides / posters.
fn text_is_too_sparse(text: &str) -> bool {
    let pages: Vec<&str> = text.split('\u{000C}').collect();
    let page_count = pages.len().max(1);
    let total_meaningful: usize = text.chars().filter(|c| !c.is_whitespace()).count();
    let avg = total_meaningful / page_count;
    avg < fallback::MIN_CHARS_PER_PAGE
}

/// Run pdftotext and return the extracted text. Shared between `call`
/// and the multimodal entry's text-first path.
async fn extract_text(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<String> {
    let mut cmd = Command::new("pdftotext");
    cmd.arg("-layout");
    if let Some(f) = first {
        cmd.arg("-f").arg(f.to_string());
    }
    if let Some(l) = last {
        cmd.arg("-l").arg(l.to_string());
    }
    cmd.arg(validated.as_os_str()).arg("-"); // stdout
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Tool(
                "pdftotext not found — install poppler-utils \
                 (`brew install poppler` on macOS, \
                 `apt install poppler-utils` on Debian/Ubuntu)"
                    .into(),
            )
        } else {
            Error::Tool(format!("spawn pdftotext: {e}"))
        }
    })?;

    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let mut out_buf = Vec::new();
    let mut err_buf = Vec::new();

    let run = async {
        let stdout_fut = stdout.read_to_end(&mut out_buf);
        let stderr_fut = stderr.read_to_end(&mut err_buf);
        let (a, b) = tokio::join!(stdout_fut, stderr_fut);
        a.map_err(|e| Error::Tool(format!("read stdout: {e}")))?;
        b.map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftotext: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(EXTRACT_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftotext timed out after {}s",
                EXTRACT_TIMEOUT.as_secs()
            )));
        }
    };

    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftotext failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    Ok(String::from_utf8_lossy(&out_buf).to_string())
}

/// Render the requested page range to PNG via `pdftoppm` and wrap each
/// page as a `ToolResultBlock::Image`. Caps at `MAX_PAGES_TO_RENDER`
/// pages — anything beyond that returns the rendered prefix plus a
/// trailing `Text` block telling the user how to fetch the rest via a
/// narrower `pages` argument.
async fn render_pages_as_image_blocks(
    validated: &std::path::Path,
    first: Option<u32>,
    last: Option<u32>,
) -> Result<Vec<ToolResultBlock>> {
    // pdftoppm needs a concrete page range. "all" → 1..=∞ is fine in
    // CLI semantics but we want to enforce our own MAX_PAGES_TO_RENDER
    // cap, so when last is None we substitute first + cap and tell
    // the user about the truncation in the trailing text block.
    let render_first = first.unwrap_or(1);
    let (render_last, truncated) = match last {
        Some(l) if l >= render_first => {
            let span = l - render_first + 1;
            if span > fallback::MAX_PAGES_TO_RENDER {
                (
                    render_first + fallback::MAX_PAGES_TO_RENDER - 1,
                    Some((span, fallback::MAX_PAGES_TO_RENDER)),
                )
            } else {
                (l, None)
            }
        }
        _ => {
            // first set, last unbounded ("3" already collapsed to
            // (3,3) by parse_page_range so this only triggers on the
            // "all" path where both are None — still substitute a
            // capped end and let pdftoppm clip naturally if the PDF
            // has fewer pages).
            (render_first + fallback::MAX_PAGES_TO_RENDER - 1, None)
        }
    };

    let tmp = tempfile::tempdir().map_err(|e| Error::Tool(format!("tempdir: {e}")))?;
    let prefix = tmp.path().join("page");
    let prefix_str = prefix.to_string_lossy().into_owned();

    let mut cmd = Command::new("pdftoppm");
    cmd.arg("-png")
        .arg("-r")
        .arg(fallback::RENDER_DPI.to_string())
        .arg("-f")
        .arg(render_first.to_string())
        .arg("-l")
        .arg(render_last.to_string())
        .arg(validated.as_os_str())
        .arg(&prefix_str);
    cmd.stdout(Stdio::null()).stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            Error::Tool(
                "pdftoppm not found — install poppler-utils for the vision-OCR \
                 fallback (`brew install poppler` on macOS, `apt install \
                 poppler-utils` on Debian/Ubuntu)"
                    .into(),
            )
        } else {
            Error::Tool(format!("spawn pdftoppm: {e}"))
        }
    })?;

    let mut stderr = child.stderr.take().unwrap();
    let mut err_buf = Vec::new();

    let run = async {
        stderr
            .read_to_end(&mut err_buf)
            .await
            .map_err(|e| Error::Tool(format!("read stderr: {e}")))?;
        let status = child
            .wait()
            .await
            .map_err(|e| Error::Tool(format!("wait pdftoppm: {e}")))?;
        Ok::<_, Error>(status)
    };

    let status = match timeout(fallback::RENDER_TIMEOUT, run).await {
        Ok(r) => r?,
        Err(_) => {
            return Err(Error::Tool(format!(
                "pdftoppm timed out after {}s",
                fallback::RENDER_TIMEOUT.as_secs()
            )));
        }
    };
    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&err_buf);
        return Err(Error::Tool(format!(
            "pdftoppm failed (exit {}): {}",
            status.code().unwrap_or(-1),
            stderr_str.trim()
        )));
    }

    // pdftoppm's filename pattern is `<prefix>-<N>.png` where N is
    // 1-indexed and zero-padded to the digits needed for the LAST
    // page. e.g. 100 pages → `page-001.png`; 9 pages → `page-1.png`.
    // Walking the directory and sorting by name gives us pages in
    // the right order regardless of the padding width.
    let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(tmp.path())
        .map_err(|e| Error::Tool(format!("read tmp dir: {e}")))?
        .filter_map(|r| r.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("png"))
        .collect();
    entries.sort();

    let mut blocks: Vec<ToolResultBlock> = Vec::with_capacity(entries.len() + 1);
    let mut total_bytes: usize = 0;
    for (idx, path) in entries.iter().enumerate() {
        let bytes = std::fs::read(path).map_err(|e| Error::Tool(format!("read png: {e}")))?;
        if bytes.len() > fallback::MAX_IMAGE_BYTES {
            blocks.push(ToolResultBlock::Text {
                text: format!(
                    "(page {} skipped — rendered PNG is {} bytes, over the {}-byte cap; \
                     try a narrower `pages` range or downscale via the source PDF.)",
                    render_first + idx as u32,
                    bytes.len(),
                    fallback::MAX_IMAGE_BYTES
                ),
            });
            continue;
        }
        total_bytes += bytes.len();
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        blocks.push(ToolResultBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".to_string(),
                data,
            },
        });
    }

    let mut summary = format!(
        "PDF appears to be scanned / image-based (no extractable text layer). \
         Rendered {} page(s) at {} DPI for vision OCR — total {} KB before \
         base64.",
        blocks
            .iter()
            .filter(|b| matches!(b, ToolResultBlock::Image { .. }))
            .count(),
        fallback::RENDER_DPI,
        (total_bytes + 512) / 1024
    );
    if let Some((requested, capped)) = truncated {
        summary.push_str(&format!(
            " Truncated: requested {} pages but only the first {} were rendered \
             (cap: MAX_PAGES_TO_RENDER). Re-invoke with a narrower `pages` range \
             to see the rest.",
            requested, capped
        ));
    }
    blocks.push(ToolResultBlock::Text { text: summary });

    Ok(blocks)
}

/// Parse a `pages` string into (first, last) page numbers (1-indexed,
/// inclusive). `None` for either side means "no bound". Examples:
/// - `"all"` → (None, None)
/// - `"3"` → (Some(3), Some(3))
/// - `"1-5"` → (Some(1), Some(5))
fn parse_page_range(spec: &str) -> Result<(Option<u32>, Option<u32>)> {
    let s = spec.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("all") {
        return Ok((None, None));
    }
    if let Some((a, b)) = s.split_once('-') {
        let first: u32 = a
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range start: {a:?}")))?;
        let last: u32 = b
            .trim()
            .parse()
            .map_err(|_| Error::Tool(format!("invalid page range end: {b:?}")))?;
        if first == 0 || last < first {
            return Err(Error::Tool(format!(
                "invalid page range: {first}-{last} (pages are 1-indexed; end must be >= start)"
            )));
        }
        return Ok((Some(first), Some(last)));
    }
    let n: u32 = s
        .parse()
        .map_err(|_| Error::Tool(format!("invalid page spec: {spec:?}")))?;
    if n == 0 {
        return Err(Error::Tool("page numbers are 1-indexed, got 0".into()));
    }
    Ok((Some(n), Some(n)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_page_range_all() {
        assert_eq!(parse_page_range("all").unwrap(), (None, None));
        assert_eq!(parse_page_range("ALL").unwrap(), (None, None));
        assert_eq!(parse_page_range("").unwrap(), (None, None));
    }

    #[test]
    fn parse_page_range_single() {
        assert_eq!(parse_page_range("3").unwrap(), (Some(3), Some(3)));
    }

    #[test]
    fn parse_page_range_span() {
        assert_eq!(parse_page_range("1-5").unwrap(), (Some(1), Some(5)));
        assert_eq!(parse_page_range(" 2 - 7 ").unwrap(), (Some(2), Some(7)));
    }

    #[test]
    fn parse_page_range_rejects_bad_input() {
        assert!(parse_page_range("0").is_err());
        assert!(parse_page_range("abc").is_err());
        assert!(parse_page_range("5-3").is_err());
        assert!(parse_page_range("1-").is_err());
    }

    /// The sparseness heuristic must:
    /// - Treat empty extracted text as scanned (no chars at all)
    /// - Treat a single-line title as scanned (way under the threshold)
    /// - Pass dense paragraphs through as text-PDF
    /// - Average across pages (form-feed separated)
    #[test]
    fn text_sparseness_matches_intent() {
        // Empty → scanned.
        assert!(text_is_too_sparse(""));
        assert!(text_is_too_sparse("   \n\n  "));

        // One-line title across one page → still under 50 chars
        // non-whitespace → treated as scanned. Conservative on
        // purpose; vision OCR adds little cost for a single page.
        assert!(text_is_too_sparse("Cover page"));

        // Dense single-page paragraph → not scanned.
        let dense = "A".repeat(500);
        assert!(!text_is_too_sparse(&dense));

        // Two pages, one dense + one empty → average dilutes but
        // overall still well above threshold.
        let mixed = format!("{}\u{000C}", "A".repeat(500));
        assert!(!text_is_too_sparse(&mixed));

        // Five pages of title-only content → very sparse, should
        // trip even though chars-per-page math hits exactly the
        // boundary. 5 pages × 10 chars = 50 chars total, divided
        // by 5 pages = 10 chars/page average → below the 50
        // threshold.
        let titles = (0..5)
            .map(|_| "Cover page")
            .collect::<Vec<_>>()
            .join("\u{000C}");
        assert!(text_is_too_sparse(&titles));
    }

    fn pdftotext_available() -> bool {
        std::process::Command::new("pdftotext")
            .arg("-v")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// End-to-end: PdfCreateTool writes a Thai+Latin PDF to a tempfile,
    /// PdfReadTool extracts it via pdftotext, and we assert that both
    /// scripts survive the round-trip. Skipped if poppler-utils isn't
    /// installed (CI macOS runners need `brew install poppler` in the
    /// workflow setup; ubuntu uses `apt install poppler-utils`).
    #[tokio::test]
    async fn round_trips_thai_latin_via_pdftotext() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("rt.pdf");
        let _ = PdfCreateTool
            .call(json!({
                "path": pdf.to_string_lossy(),
                "content": "# Hello สวัสดี\n\nMixed paragraph with English and ภาษาไทย together."
            }))
            .await
            .unwrap();

        let extracted = PdfReadTool
            .call(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();

        assert!(
            extracted.contains("Hello"),
            "Latin should survive round-trip, got: {extracted:?}"
        );
        assert!(
            extracted
                .chars()
                .any(|c| matches!(c, '\u{0E00}'..='\u{0E7F}')),
            "Thai should survive round-trip, got: {extracted:?}"
        );
    }

    /// Multimodal entry: a normal text PDF returns Text, not Blocks.
    /// Regression guard so the fallback doesn't accidentally fire on
    /// every invocation.
    #[tokio::test]
    async fn call_multimodal_returns_text_for_text_pdf() {
        if !pdftotext_available() {
            eprintln!("skipping: pdftotext not in PATH");
            return;
        }
        use crate::tools::PdfCreateTool;
        use serde_json::json;
        use tempfile::tempdir;

        let dir = tempdir().unwrap();
        let pdf = dir.path().join("text.pdf");
        // Long enough body that the per-page average comfortably
        // clears MIN_CHARS_PER_PAGE.
        let body = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
                    sed do eiusmod tempor incididunt ut labore et dolore magna \
                    aliqua. Ut enim ad minim veniam, quis nostrud exercitation \
                    ullamco laboris nisi ut aliquip ex ea commodo consequat.";
        PdfCreateTool
            .call(json!({"path": pdf.to_string_lossy(), "content": format!("# Doc\n\n{body}")}))
            .await
            .unwrap();

        let result = PdfReadTool
            .call_multimodal(json!({"path": pdf.to_string_lossy()}))
            .await
            .unwrap();
        match result {
            ToolResultContent::Text(t) => {
                assert!(t.contains("Lorem"), "expected text body, got: {t:?}");
            }
            ToolResultContent::Blocks(_) => {
                panic!("text PDF should not trigger image fallback")
            }
        }
    }
}
