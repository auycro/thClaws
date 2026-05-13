//! Format thClaws output for LINE.
//!
//! LINE rendering constraints (plan-07):
//! - Single text message capped at 5_000 characters
//! - No formatting (markdown, code fences, headings render as
//!   literal text — fine, but extra-noisy compared to the GUI)
//! - One reply per webhook; we send the **final** assistant text
//!   only, hiding intermediate `assistant` chunks between tool
//!   calls and any `thinking` blocks
//!
//! The `OutputFilter` rule is simple in Phase 1.1 because the
//! caller (`LineSession`) already concatenates the final
//! assistant text. This module exposes one helper —
//! `filter_for_line` — that trims + truncates with a "switch to
//! thClaws" notice when the body would exceed LINE's cap.

/// Hard ceiling — LINE rejects single text messages above this.
pub const LINE_MAX_CHARS: usize = 5_000;

/// We truncate well below the ceiling so the appended notice
/// always fits. `4_500 + notice` lands comfortably under 5_000.
pub const TRUNCATE_AT: usize = 4_500;

const NOTICE: &str = "\n\n…[response truncated — open thClaws to read in full]";

/// Trim leading/trailing whitespace + truncate at `TRUNCATE_AT`
/// characters with a notice. UTF-8 safe (uses `chars().take(N)`
/// not byte slicing, so Thai/CJK don't get cut mid-codepoint).
pub fn filter_for_line(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= TRUNCATE_AT {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(TRUNCATE_AT).collect();
    format!("{head}{NOTICE}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_when_short() {
        assert_eq!(filter_for_line("hello"), "hello");
    }

    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(filter_for_line("\n  hello\n\n"), "hello");
    }

    #[test]
    fn truncates_long_text_with_notice() {
        let body = "x".repeat(6_000);
        let out = filter_for_line(&body);
        assert!(out.ends_with("open thClaws to read in full]"));
        assert!(out.chars().count() < LINE_MAX_CHARS);
        assert!(out.chars().count() > TRUNCATE_AT);
    }

    #[test]
    fn truncation_is_char_boundary_safe_for_thai() {
        // Thai chars are 3 bytes UTF-8; byte-truncating would
        // either panic on `.to_string()` of an invalid slice or
        // produce mojibake. `chars().take` makes the result valid.
        let body = "ก".repeat(5_000);
        let out = filter_for_line(&body);
        assert!(out.ends_with("open thClaws to read in full]"));
        // Round-trip through `is_char_boundary` would be tautological
        // since `String` always ends on one — instead check we
        // didn't lose all the Thai chars.
        let thai_count = out.chars().filter(|c| *c == 'ก').count();
        assert_eq!(thai_count, TRUNCATE_AT);
    }
}
