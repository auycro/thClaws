//! Built-in SSO providers (standard feature, not enterprise-gated).
//!
//! The enterprise SSO path (`policy::active().policies.sso`) requires a
//! signed policy file pinning the org's IdP — issuer URL, client_id,
//! optional client_secret. That stays the override for enterprises that
//! need to enforce a single sign-in surface.
//!
//! Standard users (no policy file loaded) sign in with one of the
//! built-in providers below. Each provider's issuer + scopes are
//! hardcoded; the client_id / client_secret come from environment
//! variables (loaded from `.env` at startup via `crate::dotenv`). When
//! a provider's env vars are absent the provider drops out of
//! `available()` so the UI doesn't dangle a button that errors on
//! click.
//!
//! Why env vars rather than baking the client_id into the binary:
//! Google + Azure desktop client_ids aren't confidential, but
//! distributing them through `.env` lets each installation point at
//! its own OAuth project (avoids one rate-limited shared app across
//! the entire thClaws user base, and avoids forcing every fork to
//! re-register).

use crate::error::{Error, Result};
use crate::policy::SsoPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinProvider {
    Google,
    /// Stubbed — env keys + issuer wired but disabled until the
    /// Azure-side OAuth client is registered. Kept in the enum so
    /// `from_id` / `available` round-trip cleanly when the user
    /// flips it on later.
    Azure,
}

impl BuiltinProvider {
    /// Short id used over IPC and in the chosen-provider persistence.
    pub fn id(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "google",
            BuiltinProvider::Azure => "azure",
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "Google",
            BuiltinProvider::Azure => "Microsoft",
        }
    }

    pub fn issuer_url(&self) -> &'static str {
        match self {
            BuiltinProvider::Google => "https://accounts.google.com",
            // Azure's common endpoint accepts any tenant (personal +
            // work/school). For a tenant-restricted app, swap `common`
            // for the tenant id at registration time.
            BuiltinProvider::Azure => "https://login.microsoftonline.com/common/v2.0",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s {
            "google" => Some(Self::Google),
            "azure" => Some(Self::Azure),
            _ => None,
        }
    }

    /// Env var names this provider reads its client credentials from.
    /// Both are loaded out of `.env` at startup; `_SECRET` is optional
    /// (Azure desktop apps run PKCE-only without a secret), `_ID` is
    /// required.
    fn env_keys(&self) -> (&'static str, &'static str) {
        match self {
            BuiltinProvider::Google => ("GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"),
            BuiltinProvider::Azure => ("AZURE_CLIENT_ID", "AZURE_CLIENT_SECRET"),
        }
    }

    /// `true` when the provider's client_id env var is set — i.e. the
    /// "Sign in with …" button can actually launch a flow. UI consults
    /// this to decide which buttons to render.
    pub fn is_configured(&self) -> bool {
        let (id_env, _) = self.env_keys();
        std::env::var(id_env)
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
    }

    /// Resolve into the `SsoPolicy` shape the existing
    /// `crate::sso::login` / `current_session` / `logout` API consumes.
    /// The shape is the same one EE policies use — that's deliberate
    /// so the auth flow code doesn't need to branch on "where did this
    /// policy come from".
    pub fn resolve(&self) -> Result<SsoPolicy> {
        let (id_env, secret_env) = self.env_keys();
        let client_id = std::env::var(id_env)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                Error::Config(format!(
                    "{id_env} is not set — add it to .env or your environment"
                ))
            })?;
        let client_secret = std::env::var(secret_env)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(SsoPolicy {
            enabled: true,
            provider: "oidc".into(),
            issuer_url: self.issuer_url().into(),
            client_id,
            audience: None,
            client_secret,
            // Secret already resolved above — no env-name indirection.
            client_secret_env: None,
        })
    }
}

/// All providers whose client_id env var is set right now. Empty when
/// the user hasn't configured any OAuth app — the navbar Login button
/// stays present but its dropdown collapses to a single "configure"
/// hint.
pub fn available() -> Vec<BuiltinProvider> {
    [BuiltinProvider::Google, BuiltinProvider::Azure]
        .into_iter()
        .filter(|p| p.is_configured())
        .collect()
}

/// Find the first builtin (in `available()` order) that has a stored
/// session in the keychain. Used by the state payload to decide
/// "logged in as X" without requiring a separate chosen-provider
/// persistence file.
pub fn current_session_any() -> Option<(BuiltinProvider, super::Session)> {
    for p in available() {
        let Ok(policy) = p.resolve() else { continue };
        if let Some(s) = super::storage::load(&policy.issuer_url) {
            return Some((p, s));
        }
    }
    None
}
