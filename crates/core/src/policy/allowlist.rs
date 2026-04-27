//! Host allow-list matching for the `policies.plugins` enforcement.
//!
//! The matching rule is intentionally simple: each pattern is a glob
//! over the URL **host + path** segments (after stripping scheme +
//! query/fragment). `*` matches a single path segment; everything else
//! is literal. Patterns without `/` match the host only.
//!
//! Examples:
//!
//! | Pattern | Matches |
//! |---|---|
//! | `github.com` | any path under `github.com` |
//! | `github.com/acmecorp/*` | any single repo under that org |
//! | `github.com/acmecorp/*/*` | any subpath two levels deep |
//! | `internal.acme.example` | any URL on that host |
//! | `*.acme.example` | any subdomain of `acme.example` (host-glob; the leading `*.` is treated as a host-prefix wildcard) |
//!
//! Open questions (deferred):
//! - Port-aware matching (`github.com:443/...`). Skipped — patterns
//!   should not encode ports; the URL parse strips them before match.
//! - SSH scheme handling (`git@github.com:foo/bar.git`). Out of scope
//!   for v0.5.x — the `/plugin install` and `/skill install` flows
//!   already require https / .zip URLs.

use crate::policy::Policy;

/// Result of an allow-list check.
#[derive(Debug, PartialEq, Eq)]
pub enum AllowDecision {
    /// No policy active or `policies.plugins.enabled: false` — anything
    /// goes (today's open-core behavior).
    NoPolicy,
    /// Policy active and the URL matched at least one allowed pattern.
    Allowed,
    /// Policy active and the URL matched no patterns. Caller refuses
    /// the install / load and prints the included reason.
    Denied { reason: String },
}

impl AllowDecision {
    pub fn is_allowed(&self) -> bool {
        matches!(self, AllowDecision::NoPolicy | AllowDecision::Allowed)
    }
}

/// Check whether `url` is acceptable under the active policy. Returns
/// `NoPolicy` when the policy module says no policy is loaded, or when
/// `policies.plugins.enabled: false`. Otherwise returns `Allowed` /
/// `Denied` based on the host+path glob match.
pub fn check_url(url: &str) -> AllowDecision {
    let active = match crate::policy::active() {
        Some(a) => a,
        None => return AllowDecision::NoPolicy,
    };
    check_url_with(url, &active.policy)
}

/// Like `check_url` but takes the `Policy` directly — useful for tests
/// and for the MCP load path that may want to defer the global lookup.
pub fn check_url_with(url: &str, policy: &Policy) -> AllowDecision {
    let plugins = match &policy.policies.plugins {
        Some(p) if p.enabled => p,
        _ => return AllowDecision::NoPolicy,
    };
    let normalized = normalize_url_for_match(url);
    if plugins.allowed_hosts.is_empty() {
        return AllowDecision::Denied {
            reason: format!(
                "{url}: org policy has plugins.enabled but no allowed_hosts configured (deny-all)"
            ),
        };
    }
    for pattern in &plugins.allowed_hosts {
        if matches_pattern(pattern, &normalized) {
            return AllowDecision::Allowed;
        }
    }
    AllowDecision::Denied {
        reason: format!(
            "{url}: host doesn't match any of {} allowed pattern(s) — see your org's thClaws policy",
            plugins.allowed_hosts.len()
        ),
    }
}

/// Strip scheme, query, fragment, and trailing `.git` suffix; lowercase
/// host. Leaves `host[/path/...]` for matching.
pub fn normalize_url_for_match(url: &str) -> String {
    let s = url.trim();
    // Strip scheme.
    let after_scheme = s.split_once("://").map(|(_, rest)| rest).unwrap_or(s);
    // Strip user@ prefix (e.g. git ssh URLs — though we don't really
    // support them yet, defensive).
    let after_user = after_scheme
        .split_once('@')
        .map(|(_, rest)| rest)
        .unwrap_or(after_scheme);
    // Strip query / fragment.
    let body = after_user.split(['?', '#']).next().unwrap_or(after_user);
    // Strip trailing `.git`.
    let body = body.strip_suffix(".git").unwrap_or(body);
    // Drop port from host segment (only the first path segment is host).
    let (host, rest) = match body.split_once('/') {
        Some((h, r)) => (h, Some(r)),
        None => (body, None),
    };
    let host_no_port = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
    match rest {
        Some(r) if !r.is_empty() => format!("{host_no_port}/{r}"),
        _ => host_no_port,
    }
}

/// Match a normalized URL against a single allow-list pattern. `*`
/// matches one path segment OR (when leading the host as `*.`) any
/// host prefix.
pub fn matches_pattern(pattern: &str, normalized: &str) -> bool {
    let p = pattern.trim().trim_end_matches('/').to_ascii_lowercase();
    let n = normalized.trim_end_matches('/');

    // Leading `*.` is a host-glob: `*.acme.example` matches
    // `internal.acme.example` and `acme.example`.
    if let Some(suffix) = p.strip_prefix("*.") {
        let n_host = n.split('/').next().unwrap_or(n);
        return n_host == suffix || n_host.ends_with(&format!(".{suffix}"));
    }

    // Otherwise split on `/` and walk segments; `*` matches one segment.
    let p_parts: Vec<&str> = p.split('/').collect();
    let n_parts: Vec<&str> = n.split('/').collect();
    // Pattern host-only matches any path on that host.
    if p_parts.len() == 1 {
        return n_parts.first().copied().unwrap_or("") == p_parts[0];
    }
    // Pattern with path: pattern-segment count must be ≤ url-segment
    // count (pattern can be a prefix). Each pattern segment matches the
    // same-index URL segment, with `*` as a single-segment wildcard.
    if p_parts.len() > n_parts.len() {
        return false;
    }
    for (i, pat) in p_parts.iter().enumerate() {
        let target = n_parts[i];
        if *pat == "*" {
            continue;
        }
        if pat.contains('*') {
            // `repo-*` style mid-segment wildcard. Simple impl:
            // split on `*`, ensure each chunk appears in order.
            if !glob_segment(pat, target) {
                return false;
            }
            continue;
        }
        if *pat != target {
            return false;
        }
    }
    true
}

fn glob_segment(pattern: &str, target: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.is_empty() {
        return pattern == target;
    }
    let mut cursor = 0;
    for (i, chunk) in parts.iter().enumerate() {
        if chunk.is_empty() {
            continue;
        }
        if i == 0 {
            if !target[cursor..].starts_with(chunk) {
                return false;
            }
            cursor += chunk.len();
        } else if i == parts.len() - 1 {
            if !target[cursor..].ends_with(chunk) {
                return false;
            }
            cursor = target.len() - chunk.len();
        } else {
            match target[cursor..].find(chunk) {
                Some(idx) => cursor += idx + chunk.len(),
                None => return false,
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{PluginsPolicy, Policies, Policy};

    fn policy_with_plugins(p: PluginsPolicy) -> Policy {
        Policy {
            version: 1,
            issuer: "test".into(),
            issued_at: String::new(),
            expires_at: None,
            binding: None,
            policies: Policies {
                plugins: Some(p),
                ..Default::default()
            },
            signature: None,
        }
    }

    #[test]
    fn no_policy_means_no_policy() {
        // Calling check_url_with using a policy that has plugins
        // disabled → NoPolicy decision (caller treats as allowed).
        let p = policy_with_plugins(PluginsPolicy {
            enabled: false,
            allowed_hosts: vec!["github.com/acme/*".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://github.com/random/repo.git", &p),
            AllowDecision::NoPolicy
        );
    }

    #[test]
    fn empty_allowed_hosts_with_enabled_is_deny_all() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec![],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        let decision = check_url_with("https://github.com/random/repo", &p);
        assert!(matches!(decision, AllowDecision::Denied { .. }));
    }

    #[test]
    fn host_only_pattern_matches_any_path() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["github.com".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://github.com/anyone/anything.git", &p),
            AllowDecision::Allowed
        );
        assert!(matches!(
            check_url_with("https://gitlab.com/anyone/anything.git", &p),
            AllowDecision::Denied { .. }
        ));
    }

    #[test]
    fn segment_wildcard_matches_one_segment() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["github.com/acmecorp/*".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://github.com/acmecorp/internal-skills.git", &p),
            AllowDecision::Allowed
        );
        assert!(matches!(
            check_url_with("https://github.com/randomuser/skills.git", &p),
            AllowDecision::Denied { .. }
        ));
    }

    #[test]
    fn host_glob_matches_subdomains() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["*.acme.example".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://internal.acme.example/anything", &p),
            AllowDecision::Allowed
        );
        assert_eq!(
            check_url_with("https://acme.example/foo", &p),
            AllowDecision::Allowed
        );
        assert!(matches!(
            check_url_with("https://attacker.example/foo", &p),
            AllowDecision::Denied { .. }
        ));
    }

    #[test]
    fn url_with_query_and_token_normalizes() {
        // `?token=abc` shouldn't poison the match — strip queries first.
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["github.com/acme/*".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://github.com/acme/repo.zip?token=GH-abc&ref=main", &p),
            AllowDecision::Allowed
        );
    }

    #[test]
    fn case_insensitive_host_match() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["GitHub.com/Acme/*".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://GITHUB.COM/acme/repo", &p),
            AllowDecision::Allowed
        );
    }

    #[test]
    fn mid_segment_glob_works() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["github.com/acme/skill-*".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://github.com/acme/skill-deploy", &p),
            AllowDecision::Allowed
        );
        assert_eq!(
            check_url_with("https://github.com/acme/skill-anything-here", &p),
            AllowDecision::Allowed
        );
        assert!(matches!(
            check_url_with("https://github.com/acme/plugin-other", &p),
            AllowDecision::Denied { .. }
        ));
    }

    #[test]
    fn port_in_url_does_not_break_match() {
        let p = policy_with_plugins(PluginsPolicy {
            enabled: true,
            allowed_hosts: vec!["mcp.acme.example".into()],
            allow_external_scripts: true,
            allow_external_mcp: true,
        });
        assert_eq!(
            check_url_with("https://mcp.acme.example:8443/v1", &p),
            AllowDecision::Allowed
        );
    }

    #[test]
    fn normalize_strips_dot_git_suffix() {
        assert_eq!(
            normalize_url_for_match("https://github.com/acme/repo.git"),
            "github.com/acme/repo"
        );
    }
}
