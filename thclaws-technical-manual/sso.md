# SSO — OIDC sign-in (standard + EE)

`crate::sso` drives an OpenID Connect Authorization-Code + PKCE flow
against any standards-compliant IdP. As of v0.9.5 it ships in two
modes:

- **Standard** — built-in providers (Google now, Azure stubbed)
  resolved from env-supplied `*_CLIENT_ID` / `*_CLIENT_SECRET`. No
  policy file required. Surfaced via the navbar `LoginButton`.
- **Enterprise** — a signed `policy.policies.sso` block pins
  thClaws to an org-managed IdP and overrides the standard
  picker. Same OIDC code path; different policy origin.

This doc covers the wire flow, the cache shape, the secrets-backend
collision that bit v0.9.5 pre-fix, and the gateway-side
verification expected by plan-09.

---

## 1. Layout

```
crates/core/src/sso/
├── mod.rs        — login() / logout() / current_session() / status() / build_state_payload()
├── builtin.rs    — BuiltinProvider enum, .env-backed SsoPolicy construction
├── discovery.rs  — OIDC discovery doc fetcher (/.well-known/openid-configuration)
├── loopback.rs   — One-shot HTTP server bound to 127.0.0.1:<random>, accepts the OAuth callback
├── pkce.rs       — PKCE verifier + S256 challenge generator
└── storage.rs    — Session struct + keychain persistence (per-issuer)
```

`Session` carries `access_token`, `id_token`, `refresh_token`,
`expires_at` (unix-seconds), and the displayable claims (`email`,
`name`, `sub`). Stored as JSON under a sha256-of-issuer keychain
entry — `thclaws-sso-<sha256>`.

---

## 2. Two policy origins, one flow

```
                       ┌──────────────────────────────┐
                       │ build_state_payload()        │
                       │                              │
                       │ policy::active()             │
                       │   .policies.sso.enabled?     │
                       └──────────────┬───────────────┘
                                      │
                       ┌──────────────┴──────────────┐
                       │                             │
                  yes (EE override)              no (standard)
                       │                             │
                       ▼                             ▼
            ee_state_payload(policy)       builtin::current_session_any()
            { managed: true,                  iterates available() providers,
              issuer,                         returns first stored session
              ... }                           { managed: false,
                                                providers: [{id, label}, …],
                                                ... }
                       │                             │
                       └──────────────┬──────────────┘
                                      ▼
                              dispatched to frontend
                              as `{type: "sso_state", ...}`
```

Both branches produce SsoPolicy values that the existing
`sso::login(&policy)` consumes unchanged. **The OIDC code path
doesn't know or care where the policy came from** — that's the
deliberate seam.

---

## 3. `BuiltinProvider` (standard path)

```rust
pub enum BuiltinProvider {
    Google,
    Azure, // stubbed: env keys + issuer wired, awaiting OAuth registration
}

impl BuiltinProvider {
    fn env_keys(&self) -> (&'static str, &'static str) {
        match self {
            Self::Google => ("GOOGLE_CLIENT_ID", "GOOGLE_CLIENT_SECRET"),
            Self::Azure  => ("AZURE_CLIENT_ID",  "AZURE_CLIENT_SECRET"),
        }
    }

    pub fn issuer_url(&self) -> &'static str {
        match self {
            Self::Google => "https://accounts.google.com",
            Self::Azure  => "https://login.microsoftonline.com/common/v2.0",
        }
    }

    pub fn is_configured(&self) -> bool { /* env-var presence check */ }
    pub fn resolve(&self) -> Result<SsoPolicy> { /* build inline */ }
}

pub fn available() -> Vec<BuiltinProvider>;            // configured ones
pub fn current_session_any() -> Option<(BuiltinProvider, Session)>; // first stored
```

**Why env vars rather than baked constants:** Google + Azure
desktop client_secrets are non-confidential per their own docs
(see "Threat model" below), but distributing them through `.env`
lets each installation point at its own OAuth project. The
official thClaws dmg / msi bundles credentials via CI secret
injection — `build.rs` reads `BUNDLED_GOOGLE_CLIENT_ID` /
`BUNDLED_GOOGLE_CLIENT_SECRET` from CI env and exposes them via
`option_env!`. `resolve()` priority: runtime env (`.env` / shell
export) > bundled (`option_env!`) > error with configure hint.

---

## 4. Login flow (per `sso::login`)

```
1. fetch discovery doc from issuer_url + "/.well-known/openid-configuration"
2. generate PkcePair { verifier, challenge=S256(verifier) }
3. bind loopback.LoopbackServer (random localhost port)
4. construct authz URL with response_type=code, client_id, redirect_uri,
   scope="openid email profile", state=random, code_challenge=...
5. open_browser(authz_url) — falls back to printing URL on failure
6. server.accept_one(300s) — blocks for the callback redirect
7. verify state matches → defuses CSRF
8. exchange_code at token_endpoint with code + verifier + client_secret
   (when configured)
9. build_session(token_response, issuer, client_id) — decodes id_token
   claims (NO signature verification — that's the gateway's job)
10. storage::save(&session)  ◄── failure path
11. update_cache(Some(session))
12. return Ok(session)
```

The `storage::save` step at line 10 is what bit v0.9.5 pre-fix.
See `secrets.rs` collision below.

---

## 5. The `secrets` collision (v0.9.5 fix)

`crate::secrets::set/get` route through a "policy-respecting"
keychain wrapper that refuses to touch the OS keychain when the
user picked `Backend::Dotenv` on first launch:

```rust
pub fn set(provider: &str, key: &str) -> Result<()> {
    if resolved_backend() == Backend::Dotenv {
        return Err(Error::Config("keychain disabled by user preference".into()));
    }
    // ... write through the bundle
}
```

The intent is: API keys (Anthropic, OpenAI, …) honor the user's
chosen storage. Pre-v0.9.5 `sso::storage` also used these calls →
a Dotenv-preferring user's sign-in completed in the browser but
`storage::save` returned `Err("keychain disabled by user
preference")`, `login` propagated the error, and the failure
sat inside the closed navbar dropdown.

The SSO module is documented as *never* falling back to `.env` —
short-lived OAuth tokens must not land in plaintext. v0.9.5 added
three direct-keychain helpers that bypass the Backend preference:

```rust
pub fn keychain_set_raw(account: &str, value: &str) -> Result<()>;
pub fn keychain_get_raw(account: &str) -> Option<String>;
pub fn keychain_clear_raw(account: &str) -> Result<()>;
```

`sso/storage.rs` now routes through these. The user's "I prefer
dotenv for API keys" choice is preserved; SSO tokens go to
keychain regardless.

---

## 6. State payload + IPC

`build_state_payload()` returns one of three shapes:

```json
// Standard, logged out, env configured
{
  "type": "sso_state",
  "enabled": true, "managed": false,
  "logged_in": false,
  "providers": [{"id": "google", "label": "Google"}]
}

// Standard, logged in
{
  "type": "sso_state",
  "enabled": true, "managed": false,
  "logged_in": true,
  "provider": "google",
  "issuer": "https://accounts.google.com",
  "email": "jimmy@pinnshop.com",
  "name": "Jimmy", "sub": "...",
  "expires_in_secs": 3543,
  "providers": [{"id": "google", "label": "Google"}]
}

// EE override
{
  "type": "sso_state",
  "enabled": true, "managed": true,
  "logged_in": <bool>,
  "issuer": "https://acme.okta.com",
  ...
}
```

IPC handlers in `crate::ipc`:

| Inbound `type` | Behavior |
|---|---|
| `sso_status` | Dispatches `build_state_payload()` |
| `sso_login` | Spawns OAuth flow. Optional `provider: "google"\|"azure"` chooses a builtin (ignored under EE override). Re-dispatches state on completion (Ok or Err) |
| `sso_logout` | Clears EE policy session AND every builtin session for symmetry. Re-dispatches state |

The frontend `LoginButton` (navbar top-right) subscribes to
`sso_state` and refetches `sso_status` on window-focus + dropdown-open
events for self-healing — if the post-login dispatch fires while the
desktop webview is unfocused (user still on the browser OAuth tab),
the focus event when they return forces a state refresh.

---

## 7. Gateway-side verification (plan-09)

`sso::decode_id_token_claims(jwt)` parses claims **without verifying
the signature** — its doc-comment explicitly delegates verification
to the gateway. For plan-09 (thClaws Cloud Gateway), the Axum
`verify_id_token` middleware does the full check:

```rust
async fn verify_id_token(req: Request, next: Next) -> Response {
    let token = bearer(&req)?;
    let kid = decode_header(&token)?.kid?;
    let jwk = jwks_cache().get(&kid).await
        .or_else_refresh_jwks_then_retry()?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_audience(&[GOOGLE_CLIENT_ID]);   // ◄── critical
    validation.set_issuer(&["https://accounts.google.com",
                            "accounts.google.com"]);
    let claims = decode(&token, &DecodingKey::from_jwk(&jwk)?, &validation)?.claims;
    let account = db::upsert_account(&claims.sub, &claims.email).await?;
    req.extensions_mut().insert(account);
    next.run(req).await
}
```

The `aud` check at line `set_audience(...)` is the load-bearing
line that makes bundling `GOOGLE_CLIENT_SECRET` in the
distributed dmg/msi safe. Without it, an attacker could clone the
binary, swap in their own OAuth project, get a user to consent on
Google's screen (the consent screen would show THEIR app name,
not yours — already suspicious), and replay those tokens against
the gateway. With it, those forged tokens carry the attacker's
`aud` and get rejected before any billing logic runs.

Full threat model + rotation procedure live in
[`dev-plan/09-cloud-gateway.md`](../../dev-plan/09-cloud-gateway.md)
(workspace-only).

---

## 8. EE override semantics

When `policy::active()` returns Some with `.policies.sso.enabled =
true`, the EE branch wins:

- `build_state_payload` ignores `BuiltinProvider::available()` and
  returns `managed: true`.
- `sso_login`'s `provider` argument is ignored — the org's
  policy is the only IdP.
- The frontend `LoginButton` collapses to a single "Sign in"
  entry pointing at the issuer.

This is the "enterprise pins everyone to one IdP" property. The
standard Google/Azure path stays dormant under EE.

---

## 9. Refresh dance

`current_session(&policy)`:

```rust
{
    let guard = cache().lock()?;
    if let Some(s) = guard.clone() {
        if s.expires_within(REFRESH_WINDOW_SECS=60) && s.refresh_token.is_some() {
            spawn_background_refresh(policy.clone(), s.clone()); // fire-and-forget
        }
        return Some(s);
    }
}
let stored = storage::load(&policy.issuer_url)?;
update_cache(Some(stored.clone()));
Some(stored)
```

The 60s window means callers always get a "fresh enough" token —
by the time the *current* request hits Anthropic / OpenAI, the
background refresh is racing to update the cache for the *next*
request. If the refresh fails (network glitch, IdP rate limit),
the next call sees the still-valid token until it actually
expires.

---

## 10. Tests

35 tests in `crates/core/src/sso/{mod,storage,builtin}.rs`:

- JWT claim parsing — handles padded + unpadded base64, rejects
  malformed JWTs
- `cache_key` determinism — trailing slashes + case variation
  collapse to one entry
- `expires_within` / `is_expired` boundary behavior
- Session JSON round-trip with all-Optional fields absent
- `build_session` claim extraction from id_token
- `resolve_client_secret` env-var indirection (string secret →
  env-name secret → none)
- `Url`-encoding edge cases (safe chars, spaces, special chars)

`BuiltinProvider::resolve` is exercised by unit tests via env-var
overrides; `current_session_any` enumeration is left to integration
testing.

---

## 11. What this doc doesn't cover

- The k3s deployment for the gateway-side `verify_id_token`
  middleware — that's plan-09 territory.
- OAuth client registration on Google Cloud / Azure AD —
  operator work documented in plan-09's CI section.
- LINE OA's separate identity model (`LINE_USER_ID` keyed in
  Postgres) — see [`line-bridge.md`](line-bridge.md). LINE-OA
  identity does NOT flow through `crate::sso`.
