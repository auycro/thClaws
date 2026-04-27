//! Token storage for the SSO session.
//!
//! Tokens (access, refresh, id) are sensitive — leaking them gives an
//! attacker the user's identity for the gateway's audit log + any
//! upstream API behind it. They live in the OS keychain via the
//! existing `secrets` module: macOS Keychain, Windows Credential
//! Manager, or Linux Secret Service. The `.env` fallback used by
//! provider keys is **deliberately not** used here — SSO tokens are
//! short-lived and never want to land on disk in plaintext.
//!
//! Cache key shape: `thclaws-sso-<sha256-of-issuer>`. Different
//! issuers (Okta vs Azure vs Keycloak) get separate entries; an
//! enterprise admin who flips the policy from one IdP to another
//! doesn't pollute the new session with stale claims from the old.
//!
//! Cache value shape: JSON `Session` struct. Compact, single keychain
//! entry. Atomic: an interrupted refresh either lands the new session
//! whole or leaves the prior session intact.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// One authenticated SSO session — what login produces, what the
/// gateway integration consumes, what `/sso status` displays.
///
/// `expires_at` is unix-seconds. We refresh ~60s before expiry so a
/// clock-skew of a few seconds doesn't surface as a 401.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    /// Issuer URL the policy directed us at — used to scope cache key.
    pub issuer: String,
    /// OAuth client_id — recorded so a future "rotate client_id" admin
    /// move surfaces here.
    pub client_id: String,
    /// Access token sent to the gateway.
    pub access_token: String,
    /// ID token (JWT) — read for displayable claims (email, name, sub).
    /// Some IdPs issue access_token as opaque; id_token is the reliable
    /// source for "who is this user".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id_token: Option<String>,
    /// Refresh token, when the IdP issued one. May be absent (some IdPs
    /// don't issue refresh tokens for public clients without `offline_access`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix-seconds when `access_token` expires.
    pub expires_at: u64,
    /// Email claim from id_token, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Display name from id_token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Stable subject id from id_token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sub: Option<String>,
}

impl Session {
    /// `true` when the access token will expire within `seconds` from now.
    /// Used to decide whether to kick off a refresh before the next call.
    pub fn expires_within(&self, seconds: u64) -> bool {
        let now = now_secs();
        self.expires_at <= now + seconds
    }

    /// `true` when the access token is already expired.
    pub fn is_expired(&self) -> bool {
        now_secs() >= self.expires_at
    }
}

/// Persist a session to the keychain. Overwrites any existing entry
/// for the same issuer.
pub fn save(session: &Session) -> crate::error::Result<()> {
    let key = cache_key(&session.issuer);
    let body = serde_json::to_string(session)
        .map_err(|e| crate::error::Error::Tool(format!("serialize SSO session: {e}")))?;
    crate::secrets::set(&key, &body)
        .map_err(|e| crate::error::Error::Tool(format!("save SSO session: {e}")))
}

/// Read the cached session for `issuer`. Returns `None` when the
/// keychain has no entry, or when the stored JSON failed to parse
/// (treat as "no session" — the user needs to re-login).
pub fn load(issuer: &str) -> Option<Session> {
    let key = cache_key(issuer);
    let raw = crate::secrets::get(&key)?;
    serde_json::from_str(&raw).ok()
}

/// Delete the cached session for `issuer`. Returns `Ok(())` even when
/// no entry existed (logout is idempotent — a user clicking it twice
/// shouldn't error).
pub fn clear(issuer: &str) -> crate::error::Result<()> {
    let key = cache_key(issuer);
    // The secrets module's set("") effectively blanks an entry; some
    // keychain backends don't expose delete cleanly. Set an empty
    // string and rely on `load()` to treat parse-fail as "no session".
    let _ = crate::secrets::set(&key, "");
    Ok(())
}

/// Compute the keychain entry name for an issuer. Hashing keeps the
/// entry name stable when the issuer URL has trailing slash variation
/// or query strings, and also avoids embedding the full issuer URL
/// (which may be sensitive in some deployments) in the keychain UI.
pub fn cache_key(issuer: &str) -> String {
    let mut h = Sha256::new();
    h.update(issuer.trim_end_matches('/').to_ascii_lowercase().as_bytes());
    let hash = h.finalize();
    format!("thclaws-sso-{:x}", hash)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_session(expires_at: u64) -> Session {
        Session {
            issuer: "https://acme.okta.com".into(),
            client_id: "thclaws-internal".into(),
            access_token: "at_abc".into(),
            id_token: Some("id_abc".into()),
            refresh_token: Some("rt_abc".into()),
            expires_at,
            email: Some("alice@acme.example".into()),
            name: Some("Alice Sanders".into()),
            sub: Some("00u1abc".into()),
        }
    }

    #[test]
    fn cache_key_is_deterministic() {
        assert_eq!(
            cache_key("https://acme.okta.com"),
            cache_key("https://acme.okta.com/")
        );
    }

    #[test]
    fn cache_key_normalizes_case() {
        assert_eq!(
            cache_key("https://ACME.OKTA.com"),
            cache_key("https://acme.okta.com")
        );
    }

    #[test]
    fn cache_key_distinguishes_issuers() {
        assert_ne!(
            cache_key("https://acme.okta.com"),
            cache_key("https://other.auth0.com")
        );
    }

    #[test]
    fn expires_within_detects_imminent_expiry() {
        let s = fixture_session(now_secs() + 30);
        assert!(s.expires_within(60));
        assert!(!s.expires_within(10));
    }

    #[test]
    fn is_expired_is_inclusive_at_boundary() {
        let s = fixture_session(now_secs().saturating_sub(1));
        assert!(s.is_expired());
        let s2 = fixture_session(now_secs() + 3600);
        assert!(!s2.is_expired());
    }

    #[test]
    fn session_round_trips_through_json() {
        let s = fixture_session(1_700_000_000);
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert_eq!(back.access_token, "at_abc");
        assert_eq!(back.email.as_deref(), Some("alice@acme.example"));
    }

    #[test]
    fn session_round_trips_with_optional_fields_absent() {
        let s = Session {
            issuer: "https://example.com".into(),
            client_id: "c".into(),
            access_token: "at".into(),
            id_token: None,
            refresh_token: None,
            expires_at: 1_700_000_000,
            email: None,
            name: None,
            sub: None,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: Session = serde_json::from_str(&json).unwrap();
        assert!(back.id_token.is_none());
        assert!(back.refresh_token.is_none());
    }
}
