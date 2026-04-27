//! Org policy file — the foundation every Enterprise Edition control
//! sits on. See `dev-plan/01-enterprise-edition.md` for the full design.
//!
//! ## Architectural principle
//!
//! The policy file is a *gate*, not a feature. Without one, thClaws
//! behaves exactly as today (open-core UX, no enforcement). Present a
//! verified policy file and the `policies.*` blocks selectively turn
//! enforcement on. Present an *un*verified one and the binary refuses
//! to start — silent fallback would defeat the point.
//!
//! ## Resolution flow
//!
//! 1. `KeySource::resolve()` — find a verification key (compile-time
//!    embed wins, falls back to env var, ultimately `None`).
//! 2. `Policy::find_file()` — search `THCLAWS_POLICY_FILE`, then
//!    `/etc/thclaws/policy.json`, then `~/.config/thclaws/policy.json`.
//! 3. If a file exists: parse → verify signature → check binding →
//!    check expiry → return `ActivePolicy`.
//! 4. If anything fails → `PolicyError`. The startup wrapper prints
//!    `refuse_message()` and exits non-zero.
//! 5. If no file exists: return `Ok(None)`. Today's behavior.
//!
//! ## What lives here vs elsewhere
//!
//! This module owns: file format, signature verification, expiry/binding
//! checks, the `ActivePolicy` accessor that other features read at
//! decision points. It does NOT own: enforcement of any specific policy
//! (branding, allow-list, gateway, SSO) — those live in their respective
//! modules and *consult* `policy::active()` to see whether to apply.

pub mod allowlist;
pub mod error;
pub mod verify;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::OnceLock;

pub use allowlist::{check_url, AllowDecision};
pub use error::PolicyError;
pub use verify::{KeySource, EMBEDDED_PUBKEY_BASE64};

/// Policy schema version this build understands. Forward-compat guard:
/// a policy declaring a higher version refuses to load rather than
/// silently skipping unknown blocks.
pub const SUPPORTED_VERSION: u32 = 1;

/// Top-level policy document, parsed from JSON. The `signature` field
/// is checked separately by `verify::verify_policy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    pub version: u32,
    #[serde(default)]
    pub issuer: String,
    #[serde(default)]
    pub issued_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<Binding>,
    #[serde(default)]
    pub policies: Policies,
    /// Base64-encoded Ed25519 signature over the canonical-JSON form
    /// of this document with the `signature` field removed. Required
    /// for verification — `MissingSignature` if absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Binding {
    /// Human-readable org id. Logged at startup so misdeployments are
    /// visible in support diagnostics.
    #[serde(default)]
    pub org_id: String,
    /// Optional binary fingerprint (e.g. `sha256:...`). When set, must
    /// match the running binary's fingerprint or the policy refuses
    /// to apply. Prevents lifting a customer policy onto a non-customer
    /// build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_fingerprint: Option<String>,
}

/// Per-feature policy blocks. Each has `enabled: bool` so the policy
/// file can selectively activate features without forcing all of them.
/// Disabled or omitted blocks fall back to open-core default behavior.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Policies {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branding: Option<BrandingPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugins: Option<PluginsPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gateway: Option<GatewayPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sso: Option<SsoPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BrandingPolicy {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub logo_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub support_email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub banner_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub about_text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PluginsPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// Wildcard host patterns. Empty list with `enabled: true` means
    /// "no external sources allowed at all" — useful for paranoid
    /// air-gapped deployments.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Reject skills that ship executable `scripts/` dirs.
    /// `false` (default) → declarative-only skills only.
    #[serde(default = "default_true")]
    pub allow_external_scripts: bool,
    /// Reject MCP servers whose endpoint isn't in `allowed_hosts`.
    #[serde(default = "default_true")]
    pub allow_external_mcp: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// Replacement base URL. All provider HTTP calls route here.
    #[serde(default)]
    pub url: String,
    /// Header template. `{{sso_token}}` substituted from the active
    /// SSO session (Phase 4); literal text otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_header_template: Option<String>,
    /// When true, any HTTP request that doesn't match the gateway
    /// host is blocked. When false, allows direct provider access
    /// (gateway becomes "preferred" not "required").
    #[serde(default = "default_true")]
    pub fail_closed: bool,
    /// Escape valve: when gateway is unreachable, allow local Ollama
    /// (read-only model) so users aren't completely blocked.
    #[serde(default)]
    pub read_only_local_models_allowed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SsoPolicy {
    #[serde(default)]
    pub enabled: bool,
    /// `oidc` (only supported value in v1).
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub issuer_url: String,
    #[serde(default)]
    pub client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audience: Option<String>,
}

fn default_true() -> bool {
    true
}

/// The result of loading a verified policy. Held in a `OnceLock` set
/// at startup so feature modules can read it cheaply (`policy::active()`).
#[derive(Debug, Clone)]
pub struct ActivePolicy {
    pub source_path: PathBuf,
    pub policy: Policy,
    /// Human-readable description of which key verified the active
    /// policy ("embedded", "env", or "file (path)"). For diagnostics —
    /// not load-bearing.
    pub key_source_label: String,
}

static ACTIVE: OnceLock<Option<ActivePolicy>> = OnceLock::new();

/// Read the active policy. Returns `None` if no policy file was loaded
/// at startup (today's open-core behavior). Cheap — no IO.
pub fn active() -> Option<&'static ActivePolicy> {
    ACTIVE.get().and_then(|opt| opt.as_ref())
}

/// Convenience: which `KeySource` label the active policy was verified
/// against. `"none"` when no policy is active.
pub fn key_source_label() -> String {
    active()
        .map(|a| a.key_source_label.clone())
        .unwrap_or_else(|| "none".to_string())
}

/// `true` when a policy is active AND `policies.plugins.enabled: true`
/// AND `allow_external_scripts: false`. Callers (skill installer + load
/// path) consult this to decide whether to reject script-bearing skills.
pub fn external_scripts_disallowed() -> bool {
    active()
        .and_then(|a| a.policy.policies.plugins.as_ref())
        .map(|p| p.enabled && !p.allow_external_scripts)
        .unwrap_or(false)
}

/// `true` when a policy is active AND `policies.plugins.enabled: true`
/// AND `allow_external_mcp: false`. Callers (MCP loader) consult this
/// to decide whether to apply the host allow-list to HTTP MCP servers.
pub fn external_mcp_disallowed() -> bool {
    active()
        .and_then(|a| a.policy.policies.plugins.as_ref())
        .map(|p| p.enabled && !p.allow_external_mcp)
        .unwrap_or(false)
}

/// Startup entry point. Call once, before `AppConfig::load()`. On
/// success, populates `ACTIVE` and returns whether a policy was loaded.
/// On failure, returns the error — caller prints `refuse_message()`
/// and exits non-zero.
pub fn load_or_refuse() -> Result<bool, PolicyError> {
    let key_source = KeySource::resolve()?;
    let path = match find_file() {
        Some(p) => p,
        None => {
            // No policy file found. Cache `None` so `active()` returns
            // it without re-doing the search.
            let _ = ACTIVE.set(None);
            return Ok(false);
        }
    };
    let body = std::fs::read_to_string(&path).map_err(|e| PolicyError::Io {
        path: path.clone(),
        source: e,
    })?;
    let raw: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| PolicyError::InvalidJson {
            path: path.clone(),
            source: e,
        })?;
    let _ = verify::verify_policy(&raw, &key_source, &path)?;
    let policy: Policy = serde_json::from_value(raw).map_err(|e| PolicyError::InvalidJson {
        path: path.clone(),
        source: e,
    })?;
    if policy.version > SUPPORTED_VERSION {
        return Err(PolicyError::UnsupportedVersion {
            path,
            got: policy.version,
            supported: SUPPORTED_VERSION,
        });
    }
    if let Some(exp) = &policy.expires_at {
        if is_expired(exp) {
            return Err(PolicyError::Expired {
                path,
                expires_at: exp.clone(),
            });
        }
    }
    if let Some(binding) = &policy.binding {
        if let Some(expected_fp) = &binding.binary_fingerprint {
            let actual = binary_fingerprint();
            if !fingerprint_matches(expected_fp, &actual) {
                return Err(PolicyError::BindingMismatch {
                    path,
                    expected: expected_fp.clone(),
                });
            }
        }
    }
    validate_policies(&policy, &path)?;
    let active = ActivePolicy {
        source_path: path,
        policy,
        key_source_label: key_source.label(),
    };
    let _ = ACTIVE.set(Some(active));
    Ok(true)
}

/// Cross-check enabled sub-policies against their required fields.
/// Catches misconfigurations that would silently fail open at runtime —
/// e.g. `gateway.enabled: true` with `gateway.url` empty. Returns
/// `Err(PolicyError::InvalidConfig)` so the binary refuses to start
/// with a clear message naming the bad field.
fn validate_policies(policy: &Policy, path: &PathBuf) -> Result<(), PolicyError> {
    if let Some(g) = &policy.policies.gateway {
        if g.enabled && g.url.trim().is_empty() {
            return Err(PolicyError::InvalidConfig {
                path: path.clone(),
                message: "gateway.enabled but gateway.url is empty — would fail open at provider construction".into(),
            });
        }
    }
    if let Some(s) = &policy.policies.sso {
        if s.enabled {
            if s.issuer_url.trim().is_empty() {
                return Err(PolicyError::InvalidConfig {
                    path: path.clone(),
                    message: "sso.enabled but sso.issuer_url is empty — OIDC discovery requires it"
                        .into(),
                });
            }
            if s.client_id.trim().is_empty() {
                return Err(PolicyError::InvalidConfig {
                    path: path.clone(),
                    message: "sso.enabled but sso.client_id is empty — OIDC requires it".into(),
                });
            }
        }
    }
    Ok(())
}

/// Walk the documented search path and return the first existing file.
/// Documented in the module header so testing can predict where a file
/// will be picked up from.
pub fn find_file() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("THCLAWS_POLICY_FILE") {
        let path = PathBuf::from(p);
        if path.exists() {
            return Some(path);
        }
    }
    let etc = PathBuf::from("/etc/thclaws/policy.json");
    if etc.exists() {
        return Some(etc);
    }
    if let Some(home) = crate::util::home_dir() {
        let user = home.join(".config/thclaws/policy.json");
        if user.exists() {
            return Some(user);
        }
    }
    None
}

/// Compare an `expires_at` ISO-8601 string against the current host
/// time. Returns `true` if the policy has expired. Tolerates the
/// common subset of ISO-8601 the rest of the codebase emits: `YYYY-
/// MM-DDTHH:MM:SSZ` and `YYYY-MM-DD`. Anything we can't parse is
/// treated as not-yet-expired (we'd rather accept a slightly weird
/// timestamp than lock everyone out).
fn is_expired(iso: &str) -> bool {
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let expiry_secs = match parse_iso8601(iso) {
        Some(s) => s,
        None => return false,
    };
    now_secs > expiry_secs
}

fn parse_iso8601(s: &str) -> Option<u64> {
    // Accept "YYYY-MM-DD" or "YYYY-MM-DDTHH:MM:SS[Z|+HH:MM]".
    // Discard timezone offset for now (treat as UTC).
    let s = s.trim();
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut date_parts = date.split('-');
    let y: i64 = date_parts.next()?.parse().ok()?;
    let m: u32 = date_parts.next()?.parse().ok()?;
    let d: u32 = date_parts.next()?.parse().ok()?;
    let (h, mi, se) = match time {
        Some(t) => {
            let t = t.trim_end_matches('Z');
            // Strip optional offset.
            let t = t.split(['+', '-']).next().unwrap_or(t);
            let mut p = t.split(':');
            let h: u64 = p.next().unwrap_or("0").parse().ok()?;
            let mi: u64 = p.next().unwrap_or("0").parse().ok()?;
            let se: u64 = p.next().unwrap_or("0").parse().ok()?;
            (h, mi, se)
        }
        None => (0, 0, 0),
    };
    let days = days_from_civil(y, m, d);
    Some((days as u64) * 86_400 + h * 3600 + mi * 60 + se)
}

/// Civil date → days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as u64 + 2) / 5 + d as u64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i64 - 719_468
}

/// Fingerprint of the running binary. Used for `binding.binary_fingerprint`
/// matching. We compute SHA-256 of the executable on first use and cache
/// the result. The fingerprint format is `sha256:<hex>` or `sha256:<hex>`
/// matched against a prefix to allow partial-fingerprint policies.
pub fn binary_fingerprint() -> String {
    static FP: OnceLock<String> = OnceLock::new();
    FP.get_or_init(|| {
        use sha2::{Digest, Sha256};
        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(_) => return String::from("sha256:unknown"),
        };
        match std::fs::read(&exe) {
            Ok(bytes) => {
                let mut h = Sha256::new();
                h.update(&bytes);
                format!("sha256:{:x}", h.finalize())
            }
            Err(_) => String::from("sha256:unknown"),
        }
    })
    .clone()
}

/// Compare an expected fingerprint against the actual one. Accepts
/// prefix matches: `sha256:abcd` matches any binary whose full
/// fingerprint starts with `abcd` after the algorithm prefix. Lets
/// admins ship policies that don't have to be reissued for every
/// rebuild that doesn't change the load-bearing code.
fn fingerprint_matches(expected: &str, actual: &str) -> bool {
    if expected == actual {
        return true;
    }
    let exp_inner = expected.strip_prefix("sha256:").unwrap_or(expected);
    let act_inner = actual.strip_prefix("sha256:").unwrap_or(actual);
    !exp_inner.is_empty() && act_inner.starts_with(exp_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_parses_date_only() {
        let secs = parse_iso8601("2026-04-27").unwrap();
        // 2026-04-27 00:00:00 UTC — sanity-check by feeding it back.
        assert!(secs > 1_700_000_000); // far enough into the future
    }

    #[test]
    fn iso_parses_full_timestamp() {
        let a = parse_iso8601("2026-04-27T00:00:00Z").unwrap();
        let b = parse_iso8601("2026-04-27").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn iso_handles_offset_by_treating_as_utc() {
        // We strip the offset rather than adjust — fine for expiry
        // semantics (off by hours, never by days).
        let a = parse_iso8601("2026-04-27T00:00:00+07:00").unwrap();
        let b = parse_iso8601("2026-04-27T00:00:00Z").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn iso_garbage_is_treated_as_unparseable() {
        assert!(parse_iso8601("hello world").is_none());
    }

    #[test]
    fn unparseable_expiry_is_not_treated_as_expired() {
        // We'd rather accept a weird timestamp than lock everyone out
        // because of a typo.
        assert!(!is_expired("not-a-date"));
    }

    #[test]
    fn past_expiry_is_expired() {
        assert!(is_expired("2020-01-01"));
    }

    #[test]
    fn future_expiry_is_not_expired() {
        // 50 years in the future, well past any test run.
        assert!(!is_expired("2076-01-01"));
    }

    #[test]
    fn fingerprint_exact_match() {
        assert!(fingerprint_matches("sha256:abcd1234", "sha256:abcd1234"));
    }

    #[test]
    fn fingerprint_prefix_match() {
        assert!(fingerprint_matches("sha256:abcd", "sha256:abcd1234ef"));
    }

    #[test]
    fn fingerprint_mismatch() {
        assert!(!fingerprint_matches("sha256:abcd", "sha256:beef1234"));
    }

    #[test]
    fn fingerprint_empty_expected_does_not_match() {
        // Defensive: an empty expected fingerprint would otherwise
        // satisfy `starts_with("")` for everything. Reject.
        assert!(!fingerprint_matches("sha256:", "sha256:abcd"));
    }

    #[test]
    fn validate_rejects_gateway_enabled_with_empty_url() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                gateway: Some(GatewayPolicy {
                    enabled: true,
                    url: String::new(),
                    auth_header_template: None,
                    fail_closed: true,
                    read_only_local_models_allowed: false,
                }),
                ..Default::default()
            },
            signature: None,
        };
        let result = validate_policies(&p, &PathBuf::from("/tmp/x.json"));
        assert!(matches!(result, Err(PolicyError::InvalidConfig { .. })));
    }

    #[test]
    fn validate_accepts_gateway_disabled_with_empty_url() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                gateway: Some(GatewayPolicy {
                    enabled: false,
                    url: String::new(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            signature: None,
        };
        assert!(validate_policies(&p, &PathBuf::from("/tmp/x.json")).is_ok());
    }

    #[test]
    fn validate_rejects_sso_enabled_with_empty_issuer() {
        let p = Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                sso: Some(SsoPolicy {
                    enabled: true,
                    provider: "oidc".into(),
                    issuer_url: String::new(),
                    client_id: "client".into(),
                    audience: None,
                }),
                ..Default::default()
            },
            signature: None,
        };
        assert!(matches!(
            validate_policies(&p, &PathBuf::from("/tmp/x.json")),
            Err(PolicyError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn policy_round_trips_through_json() {
        let policy = Policy {
            version: 1,
            issuer: "ACME".into(),
            issued_at: "2026-04-27T00:00:00Z".into(),
            expires_at: Some("2027-04-27T00:00:00Z".into()),
            binding: Some(Binding {
                org_id: "acme".into(),
                binary_fingerprint: Some("sha256:abcd".into()),
            }),
            policies: Policies {
                branding: Some(BrandingPolicy {
                    enabled: true,
                    name: Some("ACME Agent".into()),
                    ..Default::default()
                }),
                plugins: Some(PluginsPolicy {
                    enabled: true,
                    allowed_hosts: vec!["github.com/acme/*".into()],
                    allow_external_scripts: false,
                    allow_external_mcp: false,
                }),
                gateway: None,
                sso: None,
            },
            signature: Some("sig".into()),
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: Policy = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.issuer, "ACME");
        assert_eq!(
            parsed.policies.plugins.as_ref().unwrap().allowed_hosts,
            vec!["github.com/acme/*"]
        );
    }
}
