//! Gateway enforcement (Enterprise Edition Phase 3).
//!
//! When `policies.gateway.enabled: true`, every cloud-provider HTTP
//! call is rerouted to the org's private gateway (LiteLLM, Portkey,
//! Helicone, internal proxy, etc.). The gateway is assumed to speak
//! the OpenAI Chat Completions wire format and route to upstream
//! providers based on the `model` field — that's what every common
//! LLM gateway product does today.
//!
//! ## What this means at the provider layer
//!
//! When the gate is active:
//!
//! - `build_provider` returns a single `OpenAIProvider` pointing at the
//!   gateway URL, regardless of which `ProviderKind` the user picked.
//! - The user's per-provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
//!   etc.) are **ignored**. The gateway has its own credentials.
//! - The auth header is rendered from the policy's
//!   `auth_header_template` (`{{env:NAME}}` substitutions for tokens
//!   stored in env vars; `{{sso_token}}` substitution post-Phase 4).
//! - The model id is passed to the gateway unchanged. The gateway is
//!   responsible for upstream provider selection.
//!
//! ## Local models — the escape valve
//!
//! `policies.gateway.read_only_local_models_allowed: true` lets local
//! providers (Ollama, LMStudio, AgentSdk) bypass the gateway and run
//! directly. The use case: an org wants gateway-mediated access to
//! cloud LLMs but still allows engineers to use their own local
//! models for offline / private work. Off by default — strict enterprise
//! deployments expect everything to route through the gateway.
//!
//! ## What this module does *not* yet enforce
//!
//! `policies.gateway.fail_closed: true` is *declared* but not yet
//! enforced at the network layer. The current implementation prevents
//! direct provider calls by replacing the provider entirely at
//! `build_provider`-time — there's no path through the agent loop
//! that can bypass it. A future hardening pass could add a
//! `reqwest::Client` wrapper that rejects any HTTP request whose host
//! doesn't match the gateway, as defense in depth. Tracked in the
//! dev-plan as a Phase 3 follow-up.

use crate::providers::ProviderKind;

/// `true` when a verified org policy is active and `policies.gateway`
/// is `enabled`. Cheap — doesn't allocate.
pub fn is_active() -> bool {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .map(|g| g.enabled)
        .unwrap_or(false)
}

/// The gateway URL when active. Returns `None` when the gate is off
/// or no policy is loaded.
pub fn gateway_url() -> Option<String> {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| g.enabled && !g.url.trim().is_empty())
        .map(|g| g.url.clone())
}

/// Whether this `ProviderKind` should be routed through the gateway.
/// Returns `true` when the gate is active AND this is a cloud provider
/// (or `read_only_local_models_allowed` is false). Local providers
/// (Ollama, LMStudio, AgentSdk) bypass when `read_only_local_models_allowed`
/// is set.
pub fn should_route(kind: ProviderKind) -> bool {
    let g = match crate::policy::active().and_then(|a| a.policy.policies.gateway.as_ref()) {
        Some(g) if g.enabled => g,
        _ => return false,
    };
    if g.read_only_local_models_allowed && is_local_provider(kind) {
        return false;
    }
    true
}

/// Local-execution providers — the ones the `read_only_local_models_allowed`
/// escape valve unblocks. Cloud providers always route through the gateway
/// when the gate is active.
fn is_local_provider(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Ollama
            | ProviderKind::OllamaAnthropic
            | ProviderKind::LMStudio
            | ProviderKind::AgentSdk
    )
}

/// Resolve the auth header value to send to the gateway. Substitutes:
///
/// - `{{env:NAME}}` → contents of the `NAME` env var (empty string if
///   unset; deliberate so a missing token surfaces as a clean upstream
///   401 rather than a panic at startup).
/// - `{{sso_token}}` → empty string in Phase 3; Phase 4 will populate
///   from the active OIDC session.
///
/// Returns `None` when the gate is off or the template is empty.
/// Returns `Some("")` when the template renders to an empty string —
/// the OpenAI client always sends *some* Authorization header, so an
/// empty Bearer is fine for testing / unauthenticated gateway proxies.
pub fn resolve_auth_header() -> Option<String> {
    let template = crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| g.enabled)
        .and_then(|g| g.auth_header_template.clone())?;
    Some(render_template(&template))
}

/// Apply the documented substitutions. Public for testing — most
/// callers should use `resolve_auth_header()`.
pub fn render_template(template: &str) -> String {
    let mut out = template.to_string();
    // {{env:NAME}} → process env value (or empty).
    while let Some(start) = out.find("{{env:") {
        let after = &out[start + 6..];
        let Some(end_offset) = after.find("}}") else {
            break;
        };
        let name = &after[..end_offset];
        let value = std::env::var(name).unwrap_or_default();
        let full_token_end = start + 6 + end_offset + 2;
        out.replace_range(start..full_token_end, &value);
    }
    // {{sso_token}} → empty in Phase 3 (Phase 4 will populate).
    out = out.replace("{{sso_token}}", "");
    out
}

/// `true` when the `fail_closed` sub-policy is set. Currently advisory —
/// see module-level docs for the security stance.
pub fn fail_closed() -> bool {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .map(|g| g.enabled && g.fail_closed)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_template_substitutes_env() {
        std::env::set_var("THCLAWS_TEST_GATEWAY_TOKEN", "abc-secret-123");
        let rendered = render_template("Bearer {{env:THCLAWS_TEST_GATEWAY_TOKEN}}");
        assert_eq!(rendered, "Bearer abc-secret-123");
        std::env::remove_var("THCLAWS_TEST_GATEWAY_TOKEN");
    }

    #[test]
    fn render_template_unset_env_produces_empty() {
        let rendered = render_template("Bearer {{env:THCLAWS_NONEXISTENT_VAR_XYZ}}");
        assert_eq!(rendered, "Bearer ");
    }

    #[test]
    fn render_template_sso_placeholder_phase3_is_empty() {
        let rendered = render_template("Bearer {{sso_token}}");
        assert_eq!(rendered, "Bearer ");
    }

    #[test]
    fn render_template_combines_multiple_substitutions() {
        std::env::set_var("THCLAWS_TEST_KEY", "k1");
        let rendered = render_template("X-Org: acme; X-Token: {{env:THCLAWS_TEST_KEY}}");
        assert_eq!(rendered, "X-Org: acme; X-Token: k1");
        std::env::remove_var("THCLAWS_TEST_KEY");
    }

    #[test]
    fn render_template_literal_passthrough() {
        let rendered = render_template("Bearer static-token-12345");
        assert_eq!(rendered, "Bearer static-token-12345");
    }

    #[test]
    fn is_local_provider_classification() {
        assert!(is_local_provider(ProviderKind::Ollama));
        assert!(is_local_provider(ProviderKind::OllamaAnthropic));
        assert!(is_local_provider(ProviderKind::LMStudio));
        assert!(is_local_provider(ProviderKind::AgentSdk));
        assert!(!is_local_provider(ProviderKind::Anthropic));
        assert!(!is_local_provider(ProviderKind::OpenAI));
        assert!(!is_local_provider(ProviderKind::Gemini));
        assert!(!is_local_provider(ProviderKind::OllamaCloud));
    }
}
