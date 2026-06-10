# thClaws.cloud client

`crates/core/src/cloud/` — the engine-side surface for talking to a
thClaws.cloud catalog server. This file documents what ships in the
public crate and how it composes with `config.rs`, `repl.rs`, and the
GUI Settings panel. The catalog backend itself (FastAPI + Next.js +
Caddy + the Rust gateway proxy) lives in the workspace-private
`thclaws-cloud/` tree and is documented in
[`dev-plan/34`](../dev-plan/34-thclaws-cloud-control-plane.md) +
[`dev-plan/38`](../dev-plan/38-cloud-gateway-credits.md).

## What the catalog is

thClaws.cloud takes the **folder-is-an-agent** model (one folder =
`AGENTS.md` + `manifest.json` + `./.thclaws/`) and adds three
operations on top:

1. **Publish** — tar the folder, upload to the catalog as a versioned
   row.
2. **Get** — download a row, extract into a target folder, refuse to
   overwrite a non-empty folder unless `agent.uuid` matches.
3. **Rent** — server side: spawn a container, install the agent into
   it, serve at `/u/<handle>/<slug>/`. The client doesn't drive this
   directly — users provision via the catalog web UI.

The client module is the four CLI verbs + the slash dispatcher and
the `settings.json` schema needed to support them.

## Module layout

```
crates/core/src/cloud/
  mod.rs       — CloudConfig (settings.json::cloud.url), URL/token resolution,
                 ENV_TOKEN, KEYCHAIN_KEY, DEFAULT_CLOUD_URL constants
  client.rs    — HTTP client. Methods: me(), list(), list_mine(), publish(),
                 download_latest(). Bearer auth via the resolved token.
  manifest.rs  — On-disk `manifest.json` shape (Manifest + sub-structs
                 Pricing/Requires/Permissions/Preview), plus AgentConfig
                 fuse-for-publish (folds settings.json::agent into the
                 uploaded manifest, see "Agent identity" below).
  pack.rs      — Tarball encode/decode. STRIP_PREFIXES + STRIP_SUFFIXES rules,
                 sha256, peek_manifest_uuid (for the get-safety check),
                 verify_sha256, unpack with --force gate.
  cmd.rs       — Verb handlers (login/logout/publish/get/list/status/
                 unbind). Returns Vec<String> of lines for the REPL +
                 CLI binary to render uniformly.
```

`bin/app.rs` exposes the same verbs as `thclaws cloud {login|logout|
publish|get|list|status|unbind}` subcommands (`Command::Cloud` arm).
`repl.rs` parses the GUI/REPL form (`/cloud {list|get|status}`) — the
mutating verbs (`login`/`logout`/`publish`/`unbind`) intentionally stay
CLI-only because they touch the secrets backend and we don't want
slash-command typos rotating tokens.

## Settings.json shape

Two top-level blocks land in the project's `.thclaws/settings.json`
(or `~/.config/thclaws/settings.json` for user-scope overrides):

```json
{
  "cloud": {
    "url": "https://thclaws.cloud"
  },
  "agent": {
    "id": "my-research-bot",
    "name": "My Research Bot",
    "description": "Saturday-morning newsletter writer",
    "uuid": "1f9c1d70-3a26-43c4-9c40-1b1b6e3e3a01"
  }
}
```

- `cloud.url` is optional; missing → `DEFAULT_CLOUD_URL`
  (`https://thclaws.cloud`). The `THCLAWS_CLOUD_URL` env wins over
  both. The `--cloud-url` CLI flag wins over env. (See
  `cloud::resolve_cloud_url` in `mod.rs:58`.)
- `agent` is the folder's catalog identity. `id`/`name`/`description`
  mirror `manifest.json`'s same-named fields; `uuid` is stamped by the
  catalog on first publish and written back by `cloud::cmd::publish`.

The fuse step lives in `AgentConfig::fuse_for_publish` (`manifest.rs:134`)
— at publish time the on-disk `manifest.json` is merged with
`settings.json::agent` so:

- If the user updated `agent.name` via Settings but didn't touch
  `manifest.json`, the upload still carries the new name.
- The catalog's UUID never leaks into the on-disk `manifest.json`
  (so the file stays portable when shared via git, etc.) — it lives
  only in `settings.json::agent.uuid`, which is gitignored by the
  default `.thclaws/` carve-out.

## Token storage + precedence

```
THCLAWS_CLOUD_TOKEN env  →  secrets backend (keychain or .env)
                         →  legacy ~/.config/thclaws/cloud-token
```

`cmd::login` writes through `crate::secrets::set_token` with
`KEYCHAIN_KEY = "cloud-token"`, so the token rides the same backend
(`Keychain` / `Dotenv` / `None`) the user picked at install time. The
legacy file fallback only reads — new tokens never land there.

The GUI Settings → **thClaws.cloud** panel goes through the same
secrets bundle as provider API keys (an IPC `cloud_config_set`
followed by a `set_token` call). The frontend never sees the
plaintext after save.

## Tarball pack/unpack rules

`pack::pack` (`pack.rs:61`) takes a folder and a JSON manifest
override, produces a gzipped tarball:

```rust
pub struct PackResult {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub included: Vec<String>,
    pub stripped: Vec<String>,
}
```

Stripping is unconditional — the user can't disable it. Anything
matching `STRIP_PREFIXES` (sessions, KMS data, `.git/`, build outputs)
or `STRIP_SUFFIXES` (`.env`, `.key`, `.pyc`, `.log`) drops out. The
extra "`_secret`" substring rule catches files like
`my_secret_config.json` that don't match the suffix list.

`pack::unpack` (`pack.rs:209`) reverses this, with a `force` flag that
governs the empty-folder check. Empty target → fresh install. Target
with `AGENTS.md` or `manifest.json` + matching `agent.uuid` → safe
update (the caller in `cmd::get` does the UUID compare via
`pack::peek_manifest_uuid`). Mismatched UUID → abort. Other content
in target → abort unless `force=true`.

The sha256 of the packed bytes is what the catalog stores per
version. `verify_sha256` is called by `download_latest` so a corrupted
download fails fast rather than getting written to disk.

## Slash dispatch

`repl.rs:1729` parses `/cloud <sub>`:

| Subcommand | SlashCommand variant |
|---|---|
| `/cloud status` (or empty) | `Cloud(CloudSlash::Status)` |
| `/cloud list [--mine]` | `Cloud(CloudSlash::List { mine })` |
| `/cloud get <slug>` | `Cloud(CloudSlash::Get { slug })` |
| anything else | `Unknown(...)` with usage hint |

The dispatcher at `repl.rs:9368` resolves the URL + token (via
`cloud::resolve_cloud_url` + secrets backend), calls the matching
`cmd::*_lines` helper, prints the resulting `Vec<String>`. All three
GUI surfaces (Chat / Terminal / CLI REPL) hit the same dispatch arm.

## CLI verbs (full surface)

`bin/app.rs::CloudCmd` (~line 317) defines the full set:

| Variant | Path |
|---|---|
| `Login { token }` | `cloud_cmd::login(url_override, token, …)` |
| `Logout` | `cloud_cmd::logout()` — clears secrets-backend entry |
| `Publish { path, dry_run }` | `cloud_cmd::publish(path, url_override, dry_run, …)` |
| `Get { slug, target, version, force }` | `cloud_cmd::get(slug, target, version, force, …)` |
| `List { mine }` | `cloud_cmd::list(mine, …)` |
| `Status` | `cloud_cmd::status(…)` |
| `Unbind` | `cloud_cmd::unbind(…)` — clears `settings.json::agent.uuid` so the next publish creates a new catalog row (fork workflow) |

Every verb takes a `--cloud-url URL` flag on the parent `Cloud`
command so the user can target a self-hosted catalog without editing
`settings.json`.

## Compose with the rest of the engine

- `config.rs` exposes `AppConfig.cloud: Option<CloudConfig>` and
  `AppConfig.agent: Option<AgentConfig>`, both pulled from the layered
  settings load (project → user → defaults). `merge_agent`
  (`config.rs:886`) handles the publish-time write-back so a `publish`
  call patches the on-disk `settings.json` without touching other
  fields.
- `ipc.rs` adds `cloud_config_get/set` and `agent_config_get/set` IPC
  handlers (~`ipc.rs:170`) feeding the GUI Settings panels. The
  WebView's `useIPC.ts` exposes the matching channels.
- `shell_dispatch.rs` registers the cloud slash command so the GUI's
  Chat tab sees the same commands the REPL does.
- The frontend `SettingsModal.tsx` grows a **thClaws.cloud** section
  (URL + token paste) and an **Agent identity** section (id read-only,
  name + description editable) wired through those IPC channels.

## Gateway as a deployment target — pointer

The optional **cloud gateway** (dev-plan/38) is a separate concern:
it lets users top up credit on the catalog and call any provider via
`gateway.thclaws.cloud/<provider>/...` with a `gw_v1_…` token. On the
desktop, that's just a provider URL change — set
`ANTHROPIC_BASE_URL=https://thclaws.cloud/gateway/anthropic` and the
existing Anthropic provider (`crates/core/src/providers/anthropic.rs`)
calls through it transparently. No code in `crates/core/src/cloud/`
needs to know about the gateway — token format and URL substitution
are the only surfaces.

For hosted workspaces, the catalog server's DockerProvisioner injects
those env vars itself when the user picks "Gateway" at workspace
create. See `dev-plan/38` Tier 3 in the workspace-private
`thclaws-cloud/` tree.

## What's intentionally NOT in this module

- **Catalog server code.** FastAPI router, alembic migrations,
  pricing, gateway-key minting — all workspace-private.
- **Gateway proxy code.** Rust crate at `thclaws-cloud/gateway/` that
  forwards `gw_v1_*`-authenticated calls to upstream providers.
- **Hosted-workspace provisioner.** Container lifecycle is the
  catalog server's responsibility.
- **Stripe / billing.** Top-up flow runs in the catalog web UI.
- **Session sync.** Sessions stay on the machine that ran the agent;
  `STRIP_PREFIXES` actively drops `.thclaws/sessions/` from
  publishes.

If you're hacking on any of those, the source tree is at
`/Volumes/Data01/agentic-workspace/thclaws-cloud/`.
