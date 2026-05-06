# Marketplace

Curated registry of skills, MCP servers, and plugins that thClaws clients fetch from `https://thclaws.ai/api/marketplace.json`. This doc covers the architecture, wire format, lifecycle, trust model, deployment workflow, EE policy override, and runbook for operators.

**Source modules:**
- `crates/core/src/marketplace.rs` — types, cache, refresh, generic search, policy tag
- `crates/core/src/skills.rs` — skill install dispatch (`install_from_url`)
- `crates/core/src/plugins.rs` — plugin install dispatch
- `crates/core/src/mcp.rs` — MCP server registration
- `crates/core/src/shell_dispatch.rs` — GUI slash-command rendering
- `crates/core/src/repl.rs` — CLI slash-command rendering
- `crates/core/resources/marketplace.json` — embedded baseline (workspace = source of truth)

**Public-facing repos:**
- `marketplace/` — public mirror with skill/mcp/plugin source directories
- `thclaws-web/` — static site that serves `api/marketplace.json` from the rsync'd workspace file

---

## 1. What the marketplace serves

Three entry types share one catalogue file:

| Type | Used by | Install via |
|---|---|---|
| **Skill** (`SKILL.md` packages) | `SkillTool` registry, system prompt | `/skill install <name>` → `skills::install_from_url` |
| **MCP server** (stdio or sse/http) | `mcp::register_server` | `/mcp install <name>` |
| **Plugin** (slash commands, agent defs, prompts) | Plugin loader at boot | `/plugin install <name>` |

Each entry record has a stable `name`, a `license_tier` (`open` / `linked-only`), an `install_url` (or transport-specific equivalent for MCP), and human-facing `description` / `homepage` / `category`. Search is substring across `name`, `description`, `category` with name-match ranked highest.

---

## 2. Three-layer architecture

Mirrors `model_catalogue`'s pattern. Same trade-off: latency-free at startup, refreshable on demand, fail-silent so offline use stays productive.

```
┌─────────────────────────────────────────────────────────────────────┐
│ 1. Embedded baseline                                                │
│    crates/core/resources/marketplace.json (include_str! at build)   │
│    Ships with every binary. First-launch fallback. ALWAYS available.│
└─────────────────────────────────────────────────────────────────────┘
                                  ↓ (on first cache miss or load)
┌─────────────────────────────────────────────────────────────────────┐
│ 2. User cache                                                       │
│    ~/.config/thclaws/marketplace.json (XDG_CONFIG_HOME aware)       │
│    Atomic write via .tmp + rename. Read on every load() call.       │
└─────────────────────────────────────────────────────────────────────┘
                                  ↓ (on /skill marketplace --refresh
                                     or daily auto-refresh @ 24h)
┌─────────────────────────────────────────────────────────────────────┐
│ 3. Remote endpoint                                                  │
│    https://thclaws.ai/api/marketplace.json (compile-time const)     │
│    Operator-deployed via thclaws-web/Makefile's `api:` target.      │
└─────────────────────────────────────────────────────────────────────┘
```

### Load path (`marketplace::load`, `marketplace.rs:478`)

```rust
pub fn load() -> Marketplace {
    if let Some(cache) = load_cache() {
        return cache;
    }
    Marketplace::from_json_str(BASELINE_JSON)
        .expect("embedded marketplace baseline must parse")
}
```

Cache → baseline. Remote fetch is **explicit only**, never on the load path — keeps REPL launch snappy even on flaky networks.

### Refresh path (`marketplace::refresh_from_remote`)

```rust
pub async fn refresh_from_remote() -> Result<RefreshOutcome, RefreshError> {
    let client = http_client().ok_or(...)?;     // static OnceLock<reqwest::Client>
    let resp = client.get(REMOTE_URL).send().await?;
    if !resp.status().is_success() { return Err(...); }
    let body = resp.text().await?;
    let parsed = Marketplace::parse_with_error(&body)?;  // validate before write
    write_cache(&body)?;                          // atomic .tmp + rename
    Ok(RefreshOutcome { skill_count, source })
}
```

Validate-before-write is the contract: if the remote response is malformed or the schema bumped past what this binary supports, the cache is **not** overwritten. Existing cache (or baseline) keeps serving.

### Daily auto-refresh (`marketplace::spawn_daily_auto_refresh`, M6.11)

Wired at both worker-boot points (`shared_session.rs` for GUI, `repl.rs::run_repl` for CLI):

```rust
pub fn spawn_daily_auto_refresh() {
    let needs_refresh = match cache_age_secs() {
        Some(secs) => secs >= AUTO_REFRESH_AFTER_SECS,  // 24 * 60 * 60
        None => true, // no cache → fetch
    };
    if !needs_refresh { return; }
    tokio::spawn(async {
        let _ = refresh_from_remote().await;  // fail-silent
    });
}
```

Cheap when fresh (one `fs::metadata` call, no network). Spawns a fail-silent fetch when stale or missing.

---

## 3. Wire format (schema v1)

Single JSON document with three array fields:

```jsonc
{
  "schema": 1,
  "source": "baseline 2026-04-29",
  "fetched_at": "2026-04-29T00:00:00Z",
  "skills": [
    {
      "name": "skill-creator",
      "short_description": "Scaffold a new SKILL.md package",
      "description": "Create properly-shaped skills with SKILL.md, scripts/, references/...",
      "category": "development",
      "license": "Apache-2.0",
      "license_tier": "open",
      "source_repo": "thClaws/marketplace",
      "source_path": "skills/skill-creator",
      "install_url": "https://github.com/thClaws/marketplace.git#main:skills/skill-creator",
      "homepage": "https://github.com/thClaws/marketplace/tree/main/skills/skill-creator"
    }
  ],
  "mcp_servers": [
    {
      "name": "weather-mcp",
      "description": "...",
      "license": "MIT",
      "license_tier": "open",
      "transport": "stdio",
      "command": "uvx",
      "args": ["weather-mcp"],
      "install_url": null,
      "post_install_message": "pip install weather-mcp",
      "url": "",
      "homepage": "..."
    }
  ],
  "plugins": [
    {
      "name": "productivity",
      "description": "...",
      "license": "Apache-2.0",
      "license_tier": "open",
      "install_url": "https://github.com/thClaws/marketplace.git#main:plugins/productivity",
      "homepage": "..."
    }
  ]
}
```

### Schema versioning

`CURRENT_SCHEMA = 1` is a `pub const u32` in `marketplace.rs:88`. The parser sniffs the schema field via a minimal probe struct **before** the full deserialize:

```rust
pub fn parse_with_error(body: &str) -> Result<Self, ParseError> {
    #[derive(Deserialize)]
    struct SchemaProbe { #[serde(default)] schema: u32 }
    let probe: SchemaProbe = serde_json::from_str(body).map_err(...)?;
    if probe.schema != CURRENT_SCHEMA {
        return Err(ParseError::SchemaMismatch { got: probe.schema, expected: CURRENT_SCHEMA });
    }
    serde_json::from_str(body).map_err(...)  // full deserialize
}
```

A remote that bumps to `schema: 2` reports:

> `remote schema=2, this binary supports schema=1 — upgrade thclaws to refresh from a newer endpoint`

…instead of a confusing field-by-field deserialize error. (M6.11 fix M1 — see `dev-log/127`.)

**Bumping the schema** requires:
1. Decide what's incompatible (new required field, removed field, type change)
2. Bump `CURRENT_SCHEMA` in `marketplace.rs`
3. Add migration handling if v1 caches should be readable as v2 (currently they're not — old caches are silently rejected and the loader falls back to baseline)
4. Update the embedded baseline + remote endpoint together
5. Cut a release; users on the old binary get the upgrade prompt next time they `--refresh`

### License tiers

| Tier | Install behavior | Use case |
|---|---|---|
| `open` | `/skill install <name>` proceeds normally | Apache-2.0 / MIT / fully redistributable |
| `linked-only` | Listed in catalogue but install command refuses; prints `homepage` URL instead | Anthropic source-available skills (docx/pdf/pptx/xlsx) — we can show them but can't redistribute |

Enforced at install time in `skills::install_from_url`. Listing is unaffected — `linked-only` entries appear with a `[linked-only]` tag in the catalog (see §6 below).

### MCP transports

```rust
#[serde(default = "default_mcp_transport")]
pub transport: String,        // "stdio" or "sse"
```

- **stdio**: `command` + `args` required. `install_url` (optional) is a git URL that `/mcp install` clones into `~/.config/thclaws/mcp/<name>/` before first run. `post_install_message` is shown verbatim post-install (typically a `pip install` or `npm install` command).
- **sse / http**: `url` required (the connection target). `command` / `args` unused.

The `MarketplaceEntry::policy_check_url` impl prefers `install_url` when both are set (stdio with a clone source), falls back to `url` (sse/http target) otherwise.

---

## 4. Storage paths

### Embedded baseline

`include_str!("../resources/marketplace.json")` at build time. Bundled into both `thclaws` and `thclaws-cli` binaries. Never written to disk; serves only as the parse target inside `BASELINE_JSON`.

**To update the baseline**: edit `crates/core/resources/marketplace.json` in the workspace. The `thclaws-web/Makefile`'s `api:` target copies that same file to `thclaws-web/api/marketplace.json` at deploy time, so binary baseline and over-the-wire catalog stay in lock-step.

### User cache

```rust
pub fn cache_path() -> Option<PathBuf> {
    let base = if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg)
    } else {
        crate::util::home_dir()?.join(".config")
    };
    Some(base.join("thclaws").join("marketplace.json"))
}
```

- macOS / Linux default: `~/.config/thclaws/marketplace.json`
- Honors `XDG_CONFIG_HOME` for users with non-standard config layouts
- Returns `None` when the home directory can't be determined (rare; some CI environments)

### Atomic writes

```rust
fn write_cache(body: &str) -> Result<(), RefreshError> {
    let path = cache_path().ok_or(RefreshError::NoHome)?;
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;  // POSIX atomic rename
    Ok(())
}
```

Write-to-tmp-then-rename is atomic on POSIX filesystems. Guards against partial writes if the process is killed mid-fetch.

---

## 5. Code organization

### Types

```rust
pub struct Marketplace {
    pub schema: u32,
    pub source: String,
    pub fetched_at: String,
    pub skills: Vec<MarketplaceSkill>,
    pub mcp_servers: Vec<MarketplaceMcpServer>,
    pub plugins: Vec<MarketplacePlugin>,
}

pub struct MarketplaceSkill { name, description, category, license, license_tier, install_url, ... }
pub struct MarketplaceMcpServer { name, description, transport, command, args, install_url, url, ... }
pub struct MarketplacePlugin { name, description, license_tier, install_url, ... }
```

### `MarketplaceEntry` trait (M6.12)

```rust
pub trait MarketplaceEntry {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn category(&self) -> &str;
    fn license_tier(&self) -> &str;
    fn policy_check_url(&self) -> Option<&str>;
}
```

Implemented for all three types. Enables generic search/render helpers that don't duplicate per-type code. To add a fourth entry type (e.g. `MarketplaceAgentDef`), `impl MarketplaceEntry for MarketplaceAgentDef` and you immediately get `find_entry`, `search_entries`, `entry_tags` for free.

### Generic helpers

```rust
pub fn find_entry<T: MarketplaceEntry>(items: &[T], name: &str) -> Option<&T>;
pub fn search_entries<T: MarketplaceEntry>(items: &[T], query: &str) -> Vec<&T>;
pub fn entry_tags<T: MarketplaceEntry>(entry: &T) -> String;
```

`find_entry` is exact-match. `search_entries` is case-insensitive substring across name/description/category, ranked by where the match lands (name match beats description match beats category match). `entry_tags` returns a leading-space-prefixed bracketed string for catalog rendering — `"  [linked-only]"`, `" [blocked by policy]"`, or both, or empty.

### Inherent wrappers (preserved for caller convenience)

```rust
impl Marketplace {
    pub fn find(&self, name: &str) -> Option<&MarketplaceSkill> { find_entry(&self.skills, name) }
    pub fn search(&self, query: &str) -> Vec<&MarketplaceSkill> { search_entries(&self.skills, query) }
    pub fn find_mcp(...) -> ... { find_entry(&self.mcp_servers, ...) }
    pub fn search_mcp(...) -> ... { search_entries(&self.mcp_servers, ...) }
    pub fn find_plugin(...) -> ... { find_entry(&self.plugins, ...) }
    pub fn search_plugin(...) -> ... { search_entries(&self.plugins, ...) }
}
```

One-liner wrappers. Existing callers don't need to change after the M6.12 generic refactor.

### Cache freshness helpers (M6.11)

```rust
pub const AUTO_REFRESH_AFTER_SECS: u64 = 24 * 60 * 60;
pub const STALE_AFTER_SECS: u64 = 7 * 24 * 60 * 60;

pub fn cache_age_secs() -> Option<u64>;     // file mtime → age in seconds
pub fn cache_age_label() -> Option<String>; // formatted "3 hours ago" / stale hint
pub fn spawn_daily_auto_refresh();           // fire-and-forget tokio task
```

`cache_age_label` returns one of:
- `Some("3 hours ago")` — fresh
- `Some("12 days ago (stale — refresh with /skill marketplace --refresh)")` — past 7d threshold
- `None` — no cache (baseline-only usage)

Used to suffix the catalog header line in all six listing renderers.

### HTTP client (M6.11 fix L1)

```rust
fn http_client() -> Option<&'static reqwest::Client> {
    static CLIENT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    CLIENT.get_or_init(|| reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .ok()
    ).as_ref()
}
```

`OnceLock<reqwest::Client>` shared across all refresh calls so the connection pool is reused. 10-second timeout. Returns `None` only on TLS init failure (rare; prevents a panic if the OpenSSL/rustls stack is broken).

---

## 6. Slash-command surface

Six commands per entry type — three discovery + one install — symmetric across CLI and GUI.

| Command | Implementation |
|---|---|
| `/skill marketplace [--refresh]` | `shell_dispatch::SkillMarketplace` / `repl.rs::SkillMarketplace` |
| `/skill search <query>` | `shell_dispatch::SkillSearch` / `repl.rs::SkillSearch` |
| `/skill info <name>` | `shell_dispatch::SkillInfo` / `repl.rs::SkillInfo` |
| `/skill install <name|url>` | `skills::install_from_url` (after marketplace name resolution) |
| `/mcp marketplace [--refresh]` | symmetric |
| `/mcp search <query>` | symmetric |
| `/mcp info <name>` | symmetric |
| `/mcp install <name|url>` | `mcp::install_from_url` |
| `/plugin marketplace [--refresh]` | symmetric |
| `/plugin search <query>` | symmetric |
| `/plugin info <name>` | symmetric |
| `/plugin install <name|url>` | `plugins::install_from_url` |

### Catalog header

```
marketplace (baseline 2026-04-29, 1 skill(s), 3 hours ago)
── development ──
  skill-creator             — Scaffold a new SKILL.md package
install with: /skill install <name>   |   detail: /skill info <name>
```

The `, 3 hours ago` suffix comes from `cache_age_label()`. After 7 days it becomes `, 12 days ago (stale — refresh with /skill marketplace --refresh)`.

### Entry tags

| Scenario | Rendered tag |
|---|---|
| Open tier, no policy / policy allows | (none) |
| Open tier, policy denies the install_url | ` [blocked by policy]` |
| `linked-only` (no install_url) | ` [linked-only]` |
| `linked-only` with denied install_url | ` [linked-only] [blocked by policy]` |

For MCP: also `[hosted]` for `transport == "sse"` (rendered separately, not via `entry_tags` — sits between the name and the policy/license tags).

---

## 7. Install dispatch

### Skills (`skills::install_from_url`)

```rust
pub async fn install_from_url(
    url: &str,
    override_name: Option<&str>,
    project_scope: bool,
) -> Result<Vec<String>> {
    // 1. Org-policy gate
    if let AllowDecision::Denied { reason } = policy::check_url(url) {
        return Err(Error::Tool(format!("skill install blocked by org policy: {reason}")));
    }
    // 2. Dispatch on URL shape
    if is_zip_url(url) {
        install_from_zip(url, override_name, project_scope).await
    } else {
        install_from_git(url, override_name, project_scope)
    }
}
```

`is_zip_url` strips query/fragment before the `.zip` extension check, so `?token=...#frag` doesn't mask the suffix.

The git path supports an extended URL syntax for installing a single skill from a multi-skill repo:

```
https://github.com/anthropics/skills.git#main:skills/skill-creator
                                       ^----- branch
                                            ^---- subpath
```

`skills::install_from_git` parses this `#<branch>:<subpath>` extension out of the URL.

### MCP (`mcp::install_from_url` and friends)

For stdio: clone the install_url into `~/.config/thclaws/mcp/<name>/`, write the mcp.json entry with `command + args`, run `post_install_message` if present.

For sse/http: write the mcp.json entry with `url`, no clone step.

### Plugins (`plugins::install_from_url`)

Similar to skills but lands in `.thclaws/plugins/<name>/` (project) or `~/.config/thclaws/plugins/<name>/` (user). After install the plugin loader picks up the new slash commands, agent defs, and prompts on the next session.

### Live refresh after install

Both GUI and CLI install paths trigger:

```rust
let refreshed = crate::skills::SkillStore::discover();
if let Ok(mut store) = state.skill_store.lock() { *store = refreshed; }
state.rebuild_system_prompt();
state.rebuild_agent(true)?;  // rebuild the LLM agent so the new skill is in the prompt
```

User can `/skill install foo` and immediately use it without restart.

---

## 8. Trust + deployment model

The marketplace's value proposition rests on the curation guarantee. Here's how the curation is enforced.

### Workspace = source of truth

The canonical marketplace contents live in:

```
crates/core/resources/marketplace.json   ← edit here
```

This file is:
- `include_str!`'d at compile time as the embedded baseline
- `cp`'d to `thclaws-web/api/marketplace.json` at deploy time

The `thclaws-web/Makefile` `api:` target:

```makefile
api:
	@mkdir -p api
	@cp ../thclaws/crates/core/resources/marketplace.json api/marketplace.json
	@echo "✓ api/marketplace.json refreshed from workspace"
```

So the binary baseline and the live API endpoint stay in lock-step — there's no way to deploy a marketplace version that isn't also in the source-controlled workspace file.

### Public-mirror PR path

The public repo at `github.com/thClaws/thClaws` mirrors the workspace. A community contributor can submit a PR that edits `crates/core/resources/marketplace.json`. The PR is gated by `.github/CODEOWNERS` so the operator (workspace owner) must explicitly approve.

### Live-API push path

Pushing the actual `https://thclaws.ai/api/marketplace.json` requires:
1. SSH credentials to the deploy host
2. Running `make api:` from the operator's workspace clone (which uses `cp` from the local workspace file)
3. A subsequent rsync to the static site host

There is **no CI auto-deploy.** Even if a PR merges to public, the live API doesn't update until the operator runs `make api:` + rsync from their workspace.

### What a forker can do

| Vector | Outcome |
|---|---|
| Fork edits local `BASELINE_JSON` | Only changes their fork's offline-fallback list |
| Fork redirects `REMOTE_URL` | Compile-time const; only affects users running the fork's binary |
| Fork submits PR to public repo | Gated by CODEOWNERS approval |
| Fork pushes to `thclaws.ai/api/...` | No SSH credentials → impossible |
| Fork bundles their fork-binary on a download site | User would have to download the fork-binary specifically; official-binary users still hit thclaws.ai |

The trust boundary is "the binary the user installed." If they installed an official build, they get the official catalog. If they installed a fork, they get whatever the fork's `REMOTE_URL` const points at.

### Doc cited from `marketplace.rs:23-49`

The module docstring captures this verbatim. When changing the trust model, update both the docstring AND this section so they stay in sync.

---

## 9. Policy integration (org allow-lists)

`crates/core/src/policy/allowlist.rs` provides:

```rust
pub enum AllowDecision {
    NoPolicy,                          // open-core; everything goes
    Allowed,                           // policy active and URL matched
    Denied { reason: String },         // policy active and URL didn't match
}

pub fn check_url(url: &str) -> AllowDecision;
```

### Listing-time tagging (M6.12 fix M3)

```rust
pub fn entry_tags<T: MarketplaceEntry>(entry: &T) -> String {
    let mut out = String::new();
    if entry.license_tier() == "linked-only" {
        out.push_str(" [linked-only]");
    }
    if let Some(url) = entry.policy_check_url() {
        if let AllowDecision::Denied { .. } = crate::policy::check_url(url) {
            out.push_str(" [blocked by policy]");
        }
    }
    out
}
```

So `/skill marketplace` shows `[blocked by policy]` next to entries the user couldn't install anyway — saves a discovery step.

### Install-time enforcement

The `install_from_url` paths for skills, MCP, and plugins all gate on `check_url` BEFORE the actual clone/zip/spawn. A `Denied` decision returns an error early; the source is never touched.

### Policy-less open-core

In open-core builds (no policy file loaded), `check_url` returns `NoPolicy`. `entry_tags` skips the `Denied` branch and produces no `[blocked by policy]` tag. Install proceeds normally.

---

## 10. EE Phase 6 — private marketplace override (planned)

Tracked in `dev-plan/01-enterprise-edition.md`.

Under a signed org policy with a `marketplace` sub-policy:

```rust
// (planned, not yet implemented)
let effective_remote_url = match policy::active().and_then(|p| p.policy.marketplace.url.as_ref()) {
    Some(url) => url.as_str(),
    None => REMOTE_URL,  // open-core default
};
```

`REMOTE_URL` is overridden at runtime to the org's private marketplace endpoint (typically an internal mirror that lists only security-team-vetted skills).

Tamper-resistance same as the rest of the policy stack:
- Signature failure refuses startup
- URL field can't be set via `settings.json` — only via signed policy
- Embedded baseline is treated as **untrusted** under EE: when a marketplace policy is active and the remote fetch fails, the client shows an empty catalog rather than falling back to the public-baseline list

Open-core and policy-less EE builds keep current behavior: `REMOTE_URL` points at thclaws.ai, baseline is the trusted offline fallback.

---

## 11. Adding a new entry to the catalog

### As a community contributor (PR path)

1. Fork `github.com/thClaws/thClaws`
2. Add your entry to `crates/core/resources/marketplace.json` (matching schema v1)
3. If contributing source (skill, MCP server, plugin), also PR to `github.com/thClaws/marketplace`'s mirror with the source files (`SKILL.md` + `LICENSE.txt` + `NOTICE.md` + `scripts/` for skills, etc.)
4. Open the PR — CODEOWNERS will tag the operator
5. Operator reviews license, vetting, security; merges
6. Operator runs `make api:` + rsync to push to live endpoint
7. Within 24h all online users auto-refresh and see your entry

### As the operator

1. Edit `crates/core/resources/marketplace.json` in the workspace
2. Update `source` (e.g. `"baseline 2026-05-15"`) and `fetched_at`
3. Run tests: `cargo test --features gui --lib marketplace::`
4. Commit + tag a release if also bumping the binary baseline
5. Run `cd thclaws-web && make api:` to copy the workspace file to the static site
6. rsync the site to the deploy host
7. Verify: `curl https://thclaws.ai/api/marketplace.json | jq .source`

---

## 12. Adding a new entry type

After M6.12 the trait makes this small:

```rust
// 1. Define the type
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceAgentDef { name, description, category, license_tier, install_url, ... }

// 2. Implement the trait
impl MarketplaceEntry for MarketplaceAgentDef {
    fn name(&self) -> &str { &self.name }
    fn description(&self) -> &str { &self.description }
    fn category(&self) -> &str { &self.category }
    fn license_tier(&self) -> &str { &self.license_tier }
    fn policy_check_url(&self) -> Option<&str> { Some(&self.install_url) }
}

// 3. Add to the Marketplace struct
pub struct Marketplace {
    // ... existing ...
    #[serde(default)]
    pub agent_defs: Vec<MarketplaceAgentDef>,
}

// 4. Optional: add inherent wrappers for caller convenience
impl Marketplace {
    pub fn find_agent_def(&self, name: &str) -> Option<&MarketplaceAgentDef> {
        find_entry(&self.agent_defs, name)
    }
    pub fn search_agent_def(&self, query: &str) -> Vec<&MarketplaceAgentDef> {
        search_entries(&self.agent_defs, query)
    }
}

// 5. Wire into slash dispatch (shell_dispatch.rs + repl.rs):
//    /agent marketplace [--refresh], /agent search <q>, /agent info <name>
//    Each renderer uses entry_tags(entry) for the [linked-only] and
//    [blocked by policy] tags automatically.

// 6. Bump CURRENT_SCHEMA if the new field changes wire compatibility
//    (additive fields with #[serde(default)] don't require a bump).
```

The trait + generic helpers carry most of the boilerplate. The remaining work is wire-format extension and slash-command dispatch.

---

## 13. Testing

### Test fixtures

`marketplace::tests::fixture_marketplace()` returns a synthetic 3-skill catalog:

```rust
fn fixture_marketplace() -> Marketplace {
    Marketplace::from_json_str(r#"{ ... 3 skills ... }"#).expect("fixture must parse")
}
```

Used by the find/search tests so they don't depend on whatever the live baseline currently contains (the baseline can legitimately be empty during a content-review period).

### Mandatory tests for changes

| Change | Required test |
|---|---|
| New schema field (additive) | Confirm `baseline_parses` still passes |
| New schema field (required) | Bump `CURRENT_SCHEMA`, add migration test |
| New entry type | Trait impl smoke test (`marketplace_entry_trait_implemented_for_all_three_types` style) |
| New listing renderer | Confirm `entry_tags` is wired (no inline `tier_tag` blocks) |
| New install path | Org-policy gate test (verify `Denied` returns error before clone) |

### `baseline_parses` invariant

```rust
#[test]
fn baseline_parses() {
    let m = Marketplace::from_json_str(BASELINE_JSON).expect("baseline must parse");
    assert_eq!(m.schema, CURRENT_SCHEMA);
    for s in &m.skills {
        assert_eq!(s.license_tier, "open", "baseline shouldn't carry linked-only");
        assert!(s.install_url.is_some(), "open-tier entries must have install_url");
    }
}
```

`baseline_short_descriptions_are_concise` similarly catches authoring drift — short_descriptions over 70 chars fail the test.

---

## 14. Operator runbook

### Daily operation

Nothing to do. Users auto-refresh every 24h. The live API serves whatever was last `cp`'d via `make api:`.

### Adding a new entry

See §11 above. Process from edit to live: ~15 minutes if you have your workspace + deploy creds ready.

### Schema bump

1. Update `CURRENT_SCHEMA` in `marketplace.rs`
2. Add the new field(s) to all relevant types
3. Update both the embedded baseline (`crates/core/resources/marketplace.json`) and the rendered output expectations
4. Run `cargo test --features gui --lib marketplace::`
5. Cut a release; old-binary users get the schema-mismatch error on next refresh and know to upgrade

### Rollback

The cache is a single file. To roll back the live API:

```sh
# On deploy host
cd /path/to/thclaws-web
git log api/marketplace.json     # find the prior commit
git checkout <prior-sha> -- api/marketplace.json
# rsync to live
```

User caches will re-overwrite themselves on the next 24h auto-refresh.

To roll back the embedded baseline you'd need to cut a new release of thclaws — the baseline is compile-time embedded.

### "The remote returned 5xx"

Cache holds. Users on existing cache or baseline keep working. `RefreshError::Http` surfaces only when the user explicitly `--refresh`'s. Investigate the static site host.

### "I see `[blocked by policy]` tags I don't expect"

The user's binary has a policy file loaded that's denying the install URLs. Check `~/.config/thclaws/policy.json` (or the EE policy install location). For open-core builds with no policy, the tags should never appear.

### "Cache freshness label is wrong"

`cache_age_secs` reads file mtime. If the user's filesystem clock is broken or the file was touched manually (e.g. `find -exec touch`), the label drifts. Refreshing via `--refresh` resets the mtime to "now."

---

## 15. References

- **Source:** `crates/core/src/marketplace.rs` (~1130 lines incl. tests)
- **Public mirror:** `marketplace/` (workspace) → `github.com/thClaws/marketplace` (public)
- **Live API:** `https://thclaws.ai/api/marketplace.json`
- **Sprint records:**
  - `dev-log/127` — M6.11 (auto-refresh + cache age + schema-mismatch + static client)
  - `dev-log/128` — M6.12 (generic search trait + policy-blocked tag)
- **Design docs:**
  - `dev-plan/01-enterprise-edition.md` — EE Phase 6 private-marketplace override (planned)

---

*Last updated: 2026-05-02 (M6.12 ship). Update this doc when changing trust model, schema version, or listing renderers.*
