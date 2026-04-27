//! PKCE (RFC 7636) — Proof Key for Code Exchange.
//!
//! Public OIDC clients (desktop apps without a confidential secret)
//! use PKCE to prevent authorization-code interception. The client
//! generates a random `code_verifier`, derives a `code_challenge` from
//! it (SHA-256 + base64url), and ties the two together across the
//! authorize → exchange flow:
//!
//!   1. Authorize request includes `code_challenge` + method `S256`
//!   2. IdP returns an authorization code
//!   3. Token exchange request includes the original `code_verifier`
//!   4. IdP rejects the exchange unless SHA-256(verifier) == challenge
//!
//! This proves the same client that started the flow is finishing it,
//! even though the authcode passed through the user's browser.

use base64::Engine;
use sha2::{Digest, Sha256};

/// A freshly-generated PKCE pair. The verifier stays in memory; the
/// challenge gets sent to the IdP in the authorize request.
#[derive(Debug, Clone)]
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

impl PkcePair {
    /// Generate a fresh pair using OS RNG. The verifier is 43 chars
    /// (32 random bytes → base64url-encoded), well within the
    /// RFC 7636 range of 43–128 chars.
    pub fn generate() -> Self {
        let mut buf = [0u8; 32];
        getrandom::getrandom(&mut buf).expect("OS RNG failed during PKCE generation");
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf);
        let challenge = derive_challenge(&verifier);
        PkcePair {
            verifier,
            challenge,
        }
    }
}

/// Compute the S256 challenge for a given verifier — exposed for tests
/// and for any custom flow that wants to verify the derivation.
pub fn derive_challenge(verifier: &str) -> String {
    let mut h = Sha256::new();
    h.update(verifier.as_bytes());
    let hash = h.finalize();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_pair_is_self_consistent() {
        let p = PkcePair::generate();
        assert_eq!(derive_challenge(&p.verifier), p.challenge);
    }

    #[test]
    fn verifier_length_is_within_rfc_range() {
        let p = PkcePair::generate();
        // 43–128 per RFC 7636 §4.1.
        assert!(p.verifier.len() >= 43 && p.verifier.len() <= 128);
    }

    #[test]
    fn challenge_is_base64url_no_padding() {
        let p = PkcePair::generate();
        assert!(!p.challenge.contains('='));
        assert!(!p.challenge.contains('+'));
        assert!(!p.challenge.contains('/'));
    }

    #[test]
    fn derive_challenge_matches_rfc_test_vector() {
        // RFC 7636 Appendix B test vector.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = derive_challenge(verifier);
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn fresh_pairs_are_unique() {
        let a = PkcePair::generate();
        let b = PkcePair::generate();
        assert_ne!(a.verifier, b.verifier);
        assert_ne!(a.challenge, b.challenge);
    }
}
