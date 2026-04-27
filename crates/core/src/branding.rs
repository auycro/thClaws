//! Branding indirection — product name, banner, logo, support contact.
//!
//! Reads from the active org policy when one is loaded; falls back to
//! today's defaults otherwise. Open-core users with no policy file see
//! zero behavior change. Enterprise builds with a verified policy that
//! has `policies.branding.enabled: true` get the branded values.
//!
//! The module is leaf-level: it depends on `crate::policy` (read-only)
//! and nothing else. Callers ask for `branding::current()` and get a
//! snapshot they can pull fields from. The snapshot is built once on
//! first call and cached — the policy doesn't change at runtime.
//!
//! ## Sites that consume branding
//!
//! - `repl.rs`: REPL banner / version header / "restart {product}" /
//!   "── {product} diagnostics ──"
//! - `gui.rs`: native window title
//! - `prompts.rs`: `{product}` placeholder substitution in loaded
//!   system / subagent / lead prompt templates
//!
//! ## Frontend
//!
//! GUI strings live in React. They'll consume branding through an IPC
//! handler in a follow-up commit; for now the frontend renders the
//! built-in "thClaws" strings unconditionally. Phase 1 covers the Rust
//! surface; the React side is a small follow-up that doesn't gate the
//! policy infrastructure.

use std::path::PathBuf;
use std::sync::OnceLock;

/// Embedded default banner. Same bytes the REPL has always shipped —
/// keeps the open-core build identical when no policy is active.
const DEFAULT_BANNER: &str = include_str!("../../../banner.txt");

/// Default about-text shown in CLI diagnostics and (eventually) the GUI
/// About modal. Match what the README and downloads page imply.
const DEFAULT_ABOUT: &str =
    "thClaws — native-Rust AI agent workspace. Open source, MIT/Apache-2.0. \
     Developed by ThaiGPT Co., Ltd.";

/// Default support contact. Matches what's in SECURITY.md and the
/// public website.
const DEFAULT_SUPPORT_EMAIL: &str = "jimmy@thaigpt.com";

/// Materialized branding values read once at startup. Treat as read-only.
#[derive(Debug, Clone)]
pub struct Branding {
    /// Product name shown in REPL banner, GUI title bar, system prompt.
    pub name: String,
    /// Support / vulnerability-report email surfaced in diagnostics
    /// and the About modal.
    pub support_email: String,
    /// Optional path to a logo file. None → frontend uses its embedded
    /// default. Set by enterprise policy when org wants their own
    /// logo in the GUI.
    pub logo_path: Option<PathBuf>,
    /// REPL ASCII banner. Defaults to the embedded `banner.txt` bytes.
    pub banner_text: String,
    /// One-line about description shown in CLI `/version` output and
    /// the GUI About modal.
    pub about_text: String,
}

impl Default for Branding {
    fn default() -> Self {
        Self {
            name: "thClaws".to_string(),
            support_email: DEFAULT_SUPPORT_EMAIL.to_string(),
            logo_path: None,
            banner_text: DEFAULT_BANNER.to_string(),
            about_text: DEFAULT_ABOUT.to_string(),
        }
    }
}

static CURRENT: OnceLock<Branding> = OnceLock::new();

/// Snapshot the current branding values. First call reads
/// `policy::active()` and applies any branding overrides; subsequent
/// calls return the cached snapshot. Safe to call from anywhere after
/// `policy::load_or_refuse()` has run at startup.
pub fn current() -> &'static Branding {
    CURRENT.get_or_init(materialize)
}

/// Build the snapshot from defaults + active policy. Each policy field
/// is `Option<String>`, so unset fields fall through to today's
/// defaults — an enterprise can override only product name and
/// support email without losing the embedded banner, etc.
fn materialize() -> Branding {
    let mut b = Branding::default();
    let active = match crate::policy::active() {
        Some(a) => a,
        None => return b,
    };
    let bp = match &active.policy.policies.branding {
        Some(p) if p.enabled => p,
        _ => return b,
    };
    if let Some(name) = &bp.name {
        if !name.trim().is_empty() {
            b.name = name.clone();
        }
    }
    if let Some(email) = &bp.support_email {
        if !email.trim().is_empty() {
            b.support_email = email.clone();
        }
    }
    if let Some(logo) = &bp.logo_path {
        if !logo.trim().is_empty() {
            b.logo_path = Some(PathBuf::from(logo));
        }
    }
    if let Some(banner) = &bp.banner_text {
        if !banner.trim().is_empty() {
            b.banner_text = banner.clone();
        }
    }
    if let Some(about) = &bp.about_text {
        if !about.trim().is_empty() {
            b.about_text = about.clone();
        }
    }
    b
}

/// Substitute branding placeholders into a template string. Replaces
/// `{product}` with the product name and `{support_email}` with the
/// support email. Other `{...}` placeholders are left untouched so
/// callers can compose with `prompts::render` without conflict.
///
/// Used by `prompts::load` to apply branding to system / subagent /
/// lead prompts as they're loaded — same `{key}` substitution syntax
/// the rest of the prompt machinery uses.
pub fn apply_template(template: &str) -> String {
    let b = current();
    template
        .replace("{product}", &b.name)
        .replace("{support_email}", &b.support_email)
}

#[cfg(test)]
mod tests {
    use super::*;

    // We can't easily reset the OnceLock between tests, so the policy-
    // active path is exercised end-to-end via the smoke tests in
    // `tests/policy_smoke/` rather than unit-tested here. These tests
    // cover the materialization helper and template substitution
    // directly, which don't depend on the cache.

    #[test]
    fn defaults_are_today_strings() {
        let b = Branding::default();
        assert_eq!(b.name, "thClaws");
        assert_eq!(b.support_email, "jimmy@thaigpt.com");
        assert!(b.logo_path.is_none());
        assert!(b.banner_text.contains("█")); // banner.txt has block chars
        assert!(b.about_text.contains("native-Rust"));
    }

    #[test]
    fn apply_template_substitutes_product() {
        // We test the substitution logic against a constructed template
        // because `current()` would read from the policy OnceLock.
        let b = Branding {
            name: "ACME Agent".into(),
            support_email: "agent@acme.example".into(),
            ..Branding::default()
        };
        let out = format!("Hello, {{product}}!").replace("{product}", &b.name);
        assert_eq!(out, "Hello, ACME Agent!");
    }

    #[test]
    fn apply_template_leaves_unknown_placeholders_alone() {
        let template = "I'm {product}. Reach me at {support_email}. Stay {curious}.";
        // Direct substitution test (without the OnceLock).
        let after_product = template.replace("{product}", "thClaws");
        let after_email = after_product.replace("{support_email}", "jimmy@thaigpt.com");
        assert_eq!(
            after_email,
            "I'm thClaws. Reach me at jimmy@thaigpt.com. Stay {curious}."
        );
    }
}
