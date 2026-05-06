# Gateway Overlay (EE)

`gateway.rs` (`providers/gateway.rs`, 201 LOC) is **not a `Provider` impl**. It's a transparent overlay that runs INSIDE `build_provider` (`repl.rs:1204`) and rewrites which provider gets returned when an org policy declares `policies.gateway.enabled: true`.

When active, the gateway replaces every cloud provider with a single `OpenAIProvider` pointing at the org's gateway URL (LiteLLM, Portkey, Helicone, internal proxy). The user's per-provider API keys are ignored; the gateway's own credentials are used. Local providers (Ollama, OllamaAnthropic, LMStudio, AgentSdk) bypass when `read_only_local_models_allowed: true`.

This is part of the Enterprise Edition's defense layer: orgs that pay for centralized LLM cost control, audit logs, and request shaping route every cloud call through their gateway by policy fiat — and the agent layer above can't bypass it (gateway substitution happens at the provider construction site, not at the request site).

**Source:** `crates/core/src/providers/gateway.rs`
**Trigger:** `policies.gateway.enabled: true` in a verified [org policy file](permissions.md#8-ee-policy-layer)

**Cross-references:**
- [`providers.md`](providers.md) §4 — `build_provider` consults `gateway::should_route` first
- [`provider-openai.md`](provider-openai.md) — the gateway speaks OpenAI Chat Completions; this is the impl used for the substituted provider
- [`permissions.md`](permissions.md) §8 — policy file format, signature verification, `Policy::policies::gateway` schema

---

## 1. The substitution at `build_provider`

```rust
pub fn build_provider(config: &AppConfig) -> Result<Arc<dyn Provider>> {
    let kind = config.detect_provider_kind()?;

    // Stage A — gateway override runs FIRST
    if crate::providers::gateway::should_route(kind) {
        if let Some(url) = crate::providers::gateway::gateway_url() {
            let chat_url = if url.ends_with("/chat/completions") {
                url
            } else {
                format!("{}/chat/completions", url.trim_end_matches('/'))
            };
            let auth = crate::providers::gateway::resolve_auth_header().unwrap_or_default();
            return Ok(Arc::new(OpenAIProvider::new(auth).with_base_url(chat_url)));
        }
    }

    // ... rest of dispatch (Stages B + C)
}
```

When `should_route(kind) = true` AND `gateway_url() = Some(url)`:
1. Build the chat URL — append `/chat/completions` if not already present.
2. Resolve the auth header from the policy template (`{{env:NAME}}` / `{{sso_token}}` substitution).
3. Construct `OpenAIProvider::new(auth)` with that URL — **note `auth` is passed as the `api_key` constructor argument**, but it's not actually a Bearer token; see §3.
4. Return immediately. Stages B and C never run.

User's `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, etc. are completely bypassed. The user could have zero per-provider keys configured — gateway-only deployments work fine.

---

## 2. The 4 functions

### `is_active() -> bool`

```rust
pub fn is_active() -> bool {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .map(|g| g.enabled)
        .unwrap_or(false)
}
```

Cheap — doesn't allocate. Returns `false` when no policy is loaded OR when `policies.gateway` is absent OR when `enabled: false`. Checked by external code (e.g. status displays) that needs to know if the gateway is enforcing.

### `gateway_url() -> Option<String>`

```rust
pub fn gateway_url() -> Option<String> {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| g.enabled && !g.url.trim().is_empty())
        .map(|g| g.url.clone())
}
```

Returns the policy's `gateway.url` when active AND non-empty. The empty-string guard means a misconfigured `enabled: true, url: ""` returns `None` — which makes `build_provider` skip the substitution and fall through to normal dispatch. (`policy::validate_policies` ALSO catches this at startup with `InvalidConfig` before the binary opens, so reaching this guard at runtime should be impossible — defense in depth.)

### `should_route(kind) -> bool`

```rust
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

fn is_local_provider(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Ollama
            | ProviderKind::OllamaAnthropic
            | ProviderKind::LMStudio
            | ProviderKind::AgentSdk
    )
}
```

Three cases:

| Policy state | Provider kind | `should_route` |
|---|---|---|
| no policy / disabled | any | `false` |
| enabled, `read_only_local_models_allowed: false` | any | `true` |
| enabled, `read_only_local_models_allowed: true` | `Ollama`/`OllamaAnthropic`/`LMStudio`/`AgentSdk` | `false` (bypass) |
| enabled, `read_only_local_models_allowed: true` | other (cloud) | `true` |

The "local" classification covers exactly the providers that don't need to leave the user's machine:
- `Ollama` — local daemon
- `OllamaAnthropic` — local daemon (different shim)
- `LMStudio` — local runtime
- `AgentSdk` — subprocess, billing via Claude subscription

`OllamaCloud` is NOT local (it's hosted at `ollama.com`) — gateway always routes it through.

### `resolve_auth_header() -> Option<String>`

```rust
pub fn resolve_auth_header() -> Option<String> {
    let template = crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| g.enabled)
        .and_then(|g| g.auth_header_template.clone())?;
    Some(render_template(&template))
}

pub fn render_template(template: &str) -> String {
    // {{env:NAME}} → process env value (or empty)
    // {{sso_token}} → SSO session access_token (or empty)
}
```

Returns `None` when:
- No policy active
- Gateway disabled
- `auth_header_template` not set in the policy

Returns `Some(rendered)` otherwise. The render function applies two substitution patterns:

#### `{{env:NAME}}` substitution

```rust
while let Some(start) = out.find("{{env:") {
    let after = &out[start + 6..];
    let Some(end_offset) = after.find("}}") else { break; };
    let name = &after[..end_offset];
    let value = std::env::var(name).unwrap_or_default();
    let full_token_end = start + 6 + end_offset + 2;
    out.replace_range(start..full_token_end, &value);
}
```

Replaces `{{env:VAR_NAME}}` with the value of `$VAR_NAME` from the process environment. Unset env → empty string (deliberate: a missing token surfaces as a clean upstream 401 rather than a panic at startup). Loop handles multiple substitutions in one template.

Common templates:
- `Bearer {{env:GATEWAY_TOKEN}}` — env-deployed token
- `X-Org: acme; X-Token: {{env:GATEWAY_TOKEN}}` — multiple headers concatenated (gateway parses them out)

#### `{{sso_token}}` substitution

```rust
if out.contains("{{sso_token}}") {
    let token = crate::policy::active()
        .and_then(|a| a.policy.policies.sso.as_ref())
        .filter(|s| s.enabled)
        .and_then(crate::sso::current_access_token)
        .unwrap_or_default();
    out = out.replace("{{sso_token}}", &token);
}
```

Replaces `{{sso_token}}` with the OIDC access token from the active SSO session. Renders to empty string when no session — the gateway will surface a 401 and the user is prompted to run `/sso login`. Wired in EE Phase 4 (SSO).

The two substitutions can be combined in one template: `Bearer {{sso_token}}; X-Internal: {{env:INTERNAL_TOKEN}}`.

---

## 3. The "auth as api_key" trick

```rust
return Ok(Arc::new(OpenAIProvider::new(auth).with_base_url(chat_url)));
```

`OpenAIProvider::new(api_key)` expects an API key. The gateway passes the rendered `auth` string instead. Then `OpenAIProvider`'s auth path:

```rust
fn auth_header_value(&self) -> String {
    match &self.api_key_header {
        Some(_) => self.api_key.clone(),                  // raw value (no Bearer prefix)
        None => format!("Bearer {}", self.api_key),       // standard
    }
}
```

Since `api_key_header` is None (default), the value is rendered as `Authorization: Bearer <auth>` where `<auth>` is the rendered template. **So the template should produce JUST the token portion, NOT include `Bearer ` itself** — the OpenAI provider adds it.

But the docs above say `Bearer {{env:GATEWAY_TOKEN}}` is a common template. Reconciling: the template can also produce the entire header value if needed, but you'd need to combine with `with_api_key_header` to bypass the auto-Bearer prefix. The substitution today doesn't expose a header-name knob, so:

- Template `{{env:GATEWAY_TOKEN}}` → header `Authorization: Bearer <token>` (standard)
- Template `Bearer {{env:GATEWAY_TOKEN}}` → header `Authorization: Bearer Bearer <token>` (broken!)

In practice, gateway templates should NOT include `Bearer` — let the OpenAI provider add it. If your gateway expects a different scheme (e.g. `Token <value>` or `X-API-Key: <value>`), that needs a future enhancement to expose the header-name knob too.

---

## 4. `fail_closed` advisory

```rust
pub fn fail_closed() -> bool {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .map(|g| g.enabled && g.fail_closed)
        .unwrap_or(false)
}
```

Returns the policy's `fail_closed` setting. **Currently advisory only** — not enforced at the network layer. Per the module's doc comment:

> The current implementation prevents direct provider calls by replacing the provider entirely at `build_provider`-time — there's no path through the agent loop that can bypass it. A future hardening pass could add a `reqwest::Client` wrapper that rejects any HTTP request whose host doesn't match the gateway, as defense in depth. Tracked in the dev-plan as a Phase 3 follow-up.

So today: with the gateway active, every cloud HTTP call goes through it because `OpenAIProvider` is the only option and its `base_url` IS the gateway URL. There's no escape hatch.

The risk a future `reqwest::Client` interceptor would address: if some new provider impl is added that constructs its own `Client` and bypasses `build_provider`'s substitution (e.g. a side-channel for telemetry, an MCP server fetch), it could hit a non-gateway host. The current model relies on convention: no code outside `build_provider` instantiates an HTTP client for LLM traffic.

---

## 5. Practical deployment

Minimal policy file enabling gateway-only mode for a LiteLLM proxy:

```json
{
  "version": 1,
  "issuer": "acme",
  "issued_at": "2026-05-01T00:00:00Z",
  "policies": {
    "gateway": {
      "enabled": true,
      "url": "https://litellm.acme.internal/v1",
      "auth_header_template": "{{env:LITELLM_TOKEN}}",
      "fail_closed": true,
      "read_only_local_models_allowed": false
    }
  },
  "signature": "<base64 ed25519>"
}
```

Deployment steps (admin perspective):
1. Sign with the org's private key via `thclaws-policy-tool sign`.
2. Distribute the signed file to `/etc/thclaws/policy.json` on workstations (via MDM, Salt, Ansible, etc.).
3. Distribute the corresponding public key to `/etc/thclaws/policy.pub` (or compile-embed via `THCLAWS_EMBEDDED_POLICY_PUBKEY` build env).
4. Distribute `LITELLM_TOKEN` env var via login script / OS keychain integration.
5. Make sure `LITELLM_TOKEN` is set BEFORE the user launches thClaws (otherwise the gateway gets `Authorization: Bearer ` and 401s).

User experience after deployment:
- `/permissions` etc. work normally
- `/model` shows whatever models the gateway exposes (LiteLLM mirrors upstream catalogue)
- Per-provider API key fields in Settings are still settable but IGNORED for cloud providers
- Local models (if allowed) bypass the gateway

---

## 6. Tests

`gateway::tests` — 6 tests:

- `render_template_substitutes_env` — `{{env:VAR}}` → process env value
- `render_template_unset_env_produces_empty` — missing env → empty string (clean upstream 401)
- `render_template_sso_placeholder_phase3_is_empty` — `{{sso_token}}` renders empty when no SSO session
- `render_template_combines_multiple_substitutions` — multiple `{{env:...}}` in one template
- `render_template_literal_passthrough` — strings without `{{...}}` pass through unchanged
- `is_local_provider_classification` — Ollama/OllamaAnthropic/LMStudio/AgentSdk are local; Anthropic/OpenAI/Gemini/OllamaCloud are not

No end-to-end test of the substitution at `build_provider` — that path requires a verified policy at startup, which OnceLock-guarded `policy::active()` won't accept in a test environment. Manual repro: deploy a signed policy + run.

---

## 7. Notable behaviors / gotchas

- **Substitution happens at provider construction time, not at request time.** The auth header value is captured into `OpenAIProvider.api_key` at `build_provider`, then re-applied on every request. If `LITELLM_TOKEN` rotates mid-session, the agent keeps using the old value until `build_provider` re-runs (model swap, session reload). The `{{sso_token}}` path has the same issue — the access token resolved at construction time gets baked in.
- **No Anthropic-shape gateway support.** If your gateway speaks the Anthropic Messages API instead of OpenAI Chat Completions, the substitution is wrong — wire format mismatch. There's no `gateway.format` policy field today; the substitution is hard-coded to OpenAI Chat Completions because that's what every common gateway product uses.
- **Local provider classification is hard-coded.** Adding a new self-hosted provider (e.g. a future "TabbyAPI" variant) requires updating `is_local_provider`. Without that, the gateway will route the new provider through itself — likely breaking it.
- **`fail_closed` is documentation, not enforcement.** Advisory only today. Don't rely on it as a hard guarantee — verify by inspecting outbound network traffic.
- **`OnceLock` policy means no live reload.** Policy updates require restart. The gateway settings are evaluated against `policy::active()` which is set once at startup.
- **Gateway URL must end at `/v1` (not `/chat/completions`).** The substitution appends `/chat/completions` automatically. Putting the full path in `gateway.url` AND having the auto-append fire would produce `/chat/completions/chat/completions` — but the impl checks `url.ends_with("/chat/completions")` and skips the append in that case, so both work.
- **`/v1/models` requests still work** — `OpenAIProvider::list_models` derives `/v1/models` by replacing `/chat/completions` with `/models`, which the gateway should also serve.
- **No per-provider gateway routing.** Either ALL cloud providers route through the gateway OR none do. You can't say "Anthropic goes to gateway A, OpenAI to gateway B". Future feature.
- **The "auth as api_key" trick** (§3) means `Bearer ` should not be in the template. If your template is wrong, the upstream gets `Bearer Bearer <token>` and rejects with 401. Test by inspecting the rendered template in non-gateway mode first.

---

## 8. What's NOT supported

- **Network-layer enforcement of `fail_closed`** — declared but advisory.
- **Anthropic Messages format gateways** — substitution always produces an OpenAI Chat Completions client.
- **Per-request auth refresh** — substitution baked at construction.
- **Per-provider gateway selection** — single gateway for all cloud kinds.
- **Header-name override via template** — template produces only the value, header is always `Authorization`.
- **Gateway-side cost reporting** — usage info from the gateway flows back via `OpenAIProvider`'s usage parsing, but the gateway can't surface its own annotations (e.g. "this request was rate-limited at gateway tier 2").
- **Sticky-session routing** — every request is independent from the gateway's perspective; no state tracking.
- **Per-call gateway bypass** — once active, no per-call escape hatch (only the standing `read_only_local_models_allowed` policy).
