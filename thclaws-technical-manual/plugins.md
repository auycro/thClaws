# Plugins

Bundles of skills + commands + agents + MCP servers managed as one unit. A plugin ships as a directory (cloned from git or extracted from a zip) with a `plugin.json` manifest declaring which subdirs hold which contributions. Inherited shape from Claude Code (`.claude-plugin/plugin.json` is read as fallback) so anthropics-style plugins install in thClaws unchanged.

This doc covers: on-disk layout, manifest schema, discovery + scope precedence, the install / enable / disable / remove / gc lifecycle (with the explicit "what's refreshed live vs what needs a restart" matrix), security model (org-policy URL allowlist + manifest-name sanitization), trust propagation to MCP servers, code organization, testing, and sprint chronology.

**Source modules:**
- `crates/core/src/plugins.rs` — manifest types, install dispatch, registry, scope-resolved discovery (`plugin_skill_dirs`, `plugin_command_dirs`, `plugin_agent_dirs`, `plugin_mcp_servers`), `gc()`
- `crates/core/src/repl.rs` — CLI slash-command parser + handlers; `plugin_mcp_server_names()` helper for restart-hint messaging
- `crates/core/src/shell_dispatch.rs` — GUI slash-command dispatch; `refresh_after_plugin_change()` + `mcp_server_names()` helpers
- `crates/core/src/skills.rs` — `parse_git_subpath` (shared with plugins for `#branch:subpath` URLs)
- `crates/core/src/policy/mod.rs` + `allowlist.rs` — `check_url` org-policy gate
- `crates/core/src/marketplace.rs` — plugin-name → install_url resolution (`/plugin install <name>` lookup)

---

## 1. What a plugin is

A plugin is a directory containing a manifest at one of two well-known locations:

```
<plugin-root>/
├── .thclaws-plugin/
│   └── plugin.json          ← preferred (thClaws-native)
└── .claude-plugin/
    └── plugin.json          ← fallback (Claude Code compat)
```

When both exist, the thClaws-native path wins (`plugins.rs::read_manifest`). Letting Claude-Code-style plugins install unchanged is intentional — `anthropics/skills`-derived plugins typically only ship `.claude-plugin/plugin.json`.

The manifest declares which subdirectories of the plugin root contain skill / command / agent / MCP contributions. Each contribution type is loaded by the corresponding subsystem at startup (or per slash-resolution call for commands).

---

## 2. Manifest schema

```json
{
  "name": "code-review",
  "version": "1.2.0",
  "description": "Pre-commit code review with team conventions",
  "author": "Acme Eng",
  "skills": ["skills"],
  "commands": ["commands"],
  "agents": ["agents"],
  "mcpServers": {
    "deploy-hub": {
      "transport": "http",
      "url": "https://deploy.acme.example/mcp",
      "headers": {"X-Team": "platform"}
    },
    "linter-mcp": {
      "transport": "stdio",
      "command": "linter-mcp",
      "args": ["--strict"],
      "env": {"NODE_ENV": "production"}
    }
  }
}
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `name` | string | **yes** | Validated `^[A-Za-z0-9._-]+$`, never `.`/`..` (M6.16 BUG M1) — no path traversal on install |
| `version` | string | optional | Free-form (semver convention but not enforced) |
| `description` | string | optional | Surfaced in `/plugin show`, `/plugin marketplace` |
| `author` | string \| object | optional | Accepts both `"author": "Jane Doe"` AND `"author": {"name": "Jane Doe", "email": "j@x.io"}` — the object form is the Claude Code spec convention |
| `skills` | string[] | optional | Subdirs (relative to plugin root) whose children are skill dirs (each with `SKILL.md`) |
| `commands` | string[] | optional | Subdirs whose children are `.md` prompt-command templates |
| `agents` | string[] | optional | Subdirs whose children are agent-definition `.md` files |
| `mcpServers` | object | optional | Map of server-name → `McpServerEntry` |

Unknown fields are tolerated (`#[serde(default)]` on every field, no `deny_unknown_fields`) so a forward-compatible manifest doesn't break older clients.

### `McpServerEntry` shape

Same as `mcp.json`:

```json
{
  "transport": "stdio" | "http",   // default: "stdio"
  "command": "...",                 // for stdio
  "args": ["..."],
  "env": {"K": "V"},
  "url": "...",                     // for http
  "headers": {"K": "V"}
}
```

Plugin-installed MCP servers are auto-marked `trusted: true` via `McpServerEntry::to_config` (`plugins.rs:130`). The reasoning: they came in through the install flow which the user explicitly ran, and the marketplace is the curation layer for that flow. Hand-added entries in `.mcp.json` go through `config.rs::parse_mcp_json` where `trusted` must be set explicitly. See [`mcp.md`](mcp.md) §7.1 for what trust gates (HTML widget rendering, NOT unattended tool execution).

### Convention-over-configuration fallback

If the manifest doesn't declare a `skills` / `commands` / (TODO `agents`) field, the loader falls back to a conventional subdir at the plugin root:

| Field | Fallback dir |
|---|---|
| `skills` | `<plugin-root>/skills/` if it exists |
| `commands` | `<plugin-root>/commands/` if it exists |
| `agents` | (no fallback — must declare) |
| `mcpServers` | (no fallback — must declare in manifest map) |

This mirrors Claude Code's auto-discovery for plugins that rely on the convention rather than declaring it explicitly.

---

## 3. Discovery + scope precedence

Plugins live at one of two scopes:

| Scope | Plugin install dir | Registry file |
|---|---|---|
| **Project** | `<cwd>/.thclaws/plugins/<name>/` | `<cwd>/.thclaws/plugins.json` |
| **User** | `~/.config/thclaws/plugins/<name>/` | `~/.config/thclaws/plugins.json` |

### Cross-scope resolution

`installed_plugins_all_scopes()` (`plugins.rs:430`) loads project first, then user, dedup'd by name. Project wins on collision. Disabled plugins are filtered out.

`all_plugins_all_scopes()` is the same but keeps disabled entries — used by `/plugins` so the user can see what's installed but inactive.

`find_installed(name)` searches project then user, returns the first match. `find_installed_with_scope(name)` returns `(plugin, is_user_bool)` — added in M6.16.1 for `/plugin show` to print the scope.

### How plugin contributions reach each subsystem

| Contribution | Discovery function | Where it's consumed |
|---|---|---|
| Skills | `plugin_skill_dirs()` | `SkillStore::discover()` (`skills.rs:271`) — appended after user dirs but before project dirs (M6.14 priority order) |
| Commands | `plugin_command_dirs()` | `CommandStore::discover_with_extra(extra)` — both popup feed (`gui.rs:1838`) and worker resolver (`shared_session.rs::handle_line`) re-discover per call |
| Agents | `plugin_agent_dirs()` | `AgentDefsConfig::load_with_extra(extra)` — additive merge, never shadows project/user defs with the same name (`subagent.rs:52`, `team.rs:1120`) |
| MCP servers | `plugin_mcp_servers()` | Spawned at worker startup alongside `config.mcp_servers`. Project-level servers in `mcp.json` win on name clash. |

### Precedence model end-to-end

For skills: `~/.claude/skills` < `~/.config/thclaws/skills` < **plugin-contributed dirs** < `.claude/skills` < `.thclaws/skills` (project-thClaws highest). M6.14 reordered this so project always beats plugins — pre-fix plugins came LAST and shadowed project skills, contradicting the documented priority.

For commands: project paths first via `insert(0, …)`, plugin extras at the end, with `or_insert` (first-write-wins) inside `load_dir`. Net: same outcome as skills (project highest, plugin loses on collision) but via opposite mechanism (`or_insert` vs last-wins `insert`). See [`commands.md`](commands.md) §2 for the full precedence ordering.

---

## 4. Lifecycle: install / enable / disable / remove / gc

```
USER COMMAND      LIVE EFFECT (in current session)               NEEDS RESTART
──────────────────────────────────────────────────────────────────────────────
/plugin install   Skills refreshed via SkillStore::discover     MCP servers
                  Commands re-discover per call                 (subprocesses)
                  Agent rebuilt with new tool registry
                  System prompt rebuilt

/plugin enable    Same as install (refresh + rebuild)            MCP servers

/plugin disable   Skills dropped from cached store               MCP subprocess
                  Commands stop appearing in popup               keeps running +
                  Agent rebuilt without their entries            tools stay in
                                                                 registry until
                                                                 restart

/plugin remove    Same as disable, plus files deleted from       Same as disable
                  disk and registry entry dropped                (subprocess)

/plugin gc        Removes registry entries with missing path     n/a
                  or unreadable manifest. Refreshes skills
                  if any zombie was contributing.

/plugin show      Read-only — no state change                    n/a
/plugins          Read-only — list across both scopes            n/a
```

The "needs restart" column reflects what the M6.16.1 messaging makes loud and explicit. Every plugin-mutation handler that touches an MCP-contributing plugin appends:

```
⚠  restart thClaws (/quit then relaunch) to spawn 2 MCP server(s): linter-mcp, deploy-hub
```

Names always listed so the user knows exactly what's coming.

### Why MCP stays restart-bound

Tearing down a per-plugin MCP subprocess + de-registering its tools would need:
- Track which `McpClient` came from which plugin (today: flat `Vec<Arc<McpClient>>`)
- Drop the matching client (kills subprocess via `kill_on_drop`)
- Walk `tool_registry`, remove tools whose name starts with the dropped server's qualified prefix
- Rebuild agent so it stops sending those tools to the LLM

Symmetric work for spawn-on-enable. Estimated ~50 LOC each side, plus integration tests against a fake MCP server. Tracked as "MCP plugin lifecycle" sprint (BUG L1, deferred from M6.16). See [`mcp.md`](mcp.md) §7 for how MCP clients connect.

### Refresh helpers

GUI: `shell_dispatch.rs::refresh_after_plugin_change(state, events_tx)` — calls `SkillStore::discover()` + `state.rebuild_system_prompt()` + `state.rebuild_agent(true)`. Wired into `PluginInstall` (since M6.15 era), `PluginEnable`, `PluginDisable`, `PluginRemove`, `PluginGc` (since M6.16+).

CLI: same shape, inlined in each handler since the CLI repl loop holds `skill_names: HashSet<String>` and `skill_store_handle: Option<Arc<Mutex<SkillStore>>>` directly rather than a `WorkerState` struct.

### `/plugin gc`

Walks both registries, removes entries whose `path` doesn't exist OR whose `read_manifest()` returns Err. Reports the names grouped by scope. Calls `refresh_after_plugin_change` afterward in case any zombie was contributing skills cached in the worker. Use case: the user `rm -rf`'d a plugin dir manually instead of going through `/plugin remove`.

```
> /plugin gc
removed zombie entries:
  - old-plugin (project)
  - vanished (user)
```

`gc` returns `(removed_project, removed_user)` — separated so the message can attribute each entry to its scope.

---

## 5. Slash-command surface

| Command | Notes |
|---|---|
| `/plugin install <name-or-url>` | Marketplace name OR git/zip URL. `--user` for user scope (default: project). |
| `/plugin remove [--user] <name>` | Deletes files + drops registry entry |
| `/plugin enable [--user] <name>` | Flip enabled flag back on |
| `/plugin disable [--user] <name>` | Flip enabled flag off (skills/commands stop, MCP subprocess keeps running until restart) |
| `/plugin show <name>` | Single-plugin detail with scope, status, version, path, source, manifest summary |
| `/plugin gc` | Remove zombie registry entries |
| `/plugin marketplace [--refresh]` | Browse the catalogue |
| `/plugin search <query>` | Search marketplace |
| `/plugin info <name>` | Marketplace entry detail |
| `/plugins` (or `/plugin list` / `/plugin ls`) | List installed across both scopes |

The `--user` vs `--project` flag is significant — every plugin-mutation command operates on a single scope. `/plugin show` (read-only) crosses both scopes, project first.

### URL forms accepted by `/plugin install`

```
https://github.com/acme/my-plugin.git           # plain git
https://github.com/acme/my-plugin.git#main      # branch
https://github.com/acme/my-plugin.git#main:plugins/code-review
                                                 # subpath (monorepo install)
https://example.com/plugin.zip                  # zip
https://example.com/plugin.zip?token=abc        # zip with query
my-plugin                                       # marketplace name → install_url lookup
```

The `#branch:subpath` extension is shared with skills (`crate::skills::parse_git_subpath`) so a multi-plugin monorepo can be installed plugin-by-plugin without pulling the whole tree.

### CLI dispatch hint format (since M6.16.1)

```
> /plugin install code-review
plugin 'code-review' installed (project, skills+commands+1 MCP) → ~/proj/.thclaws/plugins/code-review
⚠  restart thClaws to spawn 1 new MCP server(s): linter-mcp
   skills + commands already callable in this session.
```

When the plugin contributes no MCP servers, the warning line is omitted entirely:

```
> /plugin install pure-skill-pack
plugin 'pure-skill-pack' installed (project, 3 skills) → ~/proj/.thclaws/plugins/pure-skill-pack
skills + commands callable in this session — no restart needed
```

---

## 6. Install dispatch internals

```
USER:  /plugin install <url-or-name>   [--user]
  │
  ▼
resolve_plugin_install_target(arg) → (effective_url, abort_msg)
  │  marketplace lookup if not URL-shaped
  │
  ▼
plugins::install(url, user)
  │
  ├── policy::check_url(url) → Allowed | Denied | NoPolicy   (§7)
  │
  ├── plugins_dir(user) = ~/.config/thclaws/plugins/  OR  cwd/.thclaws/plugins/
  ├── staging = plugins_dir/.install-<uuid>           (same volume as final → cheap rename)
  │
  ├── fetch_into(url, staging)
  │   ├── is_zip_url(url) → download_zip(url) [30s timeout, 64 MiB cap, zip-slip protection]
  │   │                     extract_zip(bytes, staging)
  │   └── git_clone(url, staging) — tokio::process, 60s timeout, kill_on_drop, depth-1
  │       └── if subpath in URL: clone to .clone-<uuid> sibling, then move subpath into dest
  │
  ├── plugin_root = locate_plugin_root(staging)
  │   ├── if manifest at staging root → return staging
  │   └── else if exactly one wrapper subdir with manifest → return wrapper
  │       (handles `pack-v1/...` zip wrapper convention)
  │
  ├── manifest = read_manifest(plugin_root)
  ├── validate manifest.name is non-empty AND matches [A-Za-z0-9._-]+
  │   AND not "." / ".."                              (M6.16 BUG M1: path-traversal block)
  │
  ├── final_dir = plugins_dir.join(manifest.name)
  ├── reject if final_dir.exists()                    (refuse to overwrite)
  │
  ├── std::fs::rename(plugin_root, final_dir)
  │
  ├── registry = PluginRegistry::load(user)?          (load: missing file → empty)
  ├── registry.upsert(plugin)
  ├── registry.save(user)?  ← tmp + rename (atomic, M6.16 BUG M2)
  │   on ANY failure: roll back the rename and surface a clear error  (M6.16.1 BUG L4)
  │
  └── return Plugin { name, source, path: final_dir, version, enabled: true }
```

Failure paths consistently clean up:
- Fetch error → drop staging dir
- Manifest read error → drop staging dir
- Name validation error → drop staging dir
- Final-dir collision → drop staging dir
- Registry save failure → roll back rename (best-effort) → surface either "rolled back cleanly" OR "files orphaned at <path>, run `rm -rf` to clean up"

---

## 7. Trust + policy

### Org-policy URL gate (EE)

`crate::policy::check_url(url)` runs at the top of `install`. Returns `AllowDecision::Denied { reason }` when the URL doesn't match the active policy's `allowed_hosts` list. Open-core builds with no policy active fall through to `AllowDecision::NoPolicy`. Same gate skills + MCP installs use; see [`marketplace.md`](marketplace.md) §6 for the policy model and [`mcp.md`](mcp.md) §7 for what trust gates downstream.

### Plugin-installed MCP servers are auto-trusted

`McpServerEntry::to_config` sets `trusted: true` because:
- The user explicitly ran `/plugin install` (or accepted via the marketplace flow)
- The marketplace is the curation layer for that channel
- Hand-edited `.mcp.json` entries default to `trusted: false` → no widget rendering

The trust flag gates **MCP-Apps widget rendering** (HTML iframes inside chat) — it does NOT mean tool calls run without approval. Widget tool-calls go through the same approval gate as agent-initiated tool-calls (M6.15 BUG 2 fix; see [`mcp.md`](mcp.md) §7.7).

### Manifest-name sanitization

`is_valid_plugin_name(name)` (`plugins.rs:230`) — required: `^[A-Za-z0-9._-]+$`, non-empty, NOT `.` or `..`. Rejects path separators, control characters, and the bare-dot path-resolution aliases. Pre-M6.16 a malicious `"name": "../../etc/cron.d/x"` resolved to a path that escaped `~/.config/thclaws/plugins/` on `Path::join` (which doesn't normalize `..`); bounded by FS perms in practice but no reason to leave the trapdoor open.

### What the gates do NOT block

- Plugin contents themselves — once installed, skills run with whatever scripts they ship; commands run as raw user-message text. A malicious skill can still write `Bash(...)` calls that the model executes (subject to approval mode).
- MCP server commands — a stdio MCP server's `command` field is gated separately by the user-approval allowlist at first-spawn time (`mcp.rs:139`); plugin-contributed servers ARE pre-allowlisted via the trusted flag (no first-spawn prompt). This is intentional but means the user is delegating "I trust this plugin to ship arbitrary stdio commands" to the install moment.

---

## 8. Code organization

```
crates/core/src/
├── plugins.rs                     ── ~970 LOC, the whole subsystem
│   ├── PluginManifest             (name, version, description, author, skills, commands, agents, mcpServers)
│   ├── deserialize_author_flexible (string OR {name,email} OR null OR anything)
│   ├── McpServerEntry             (mirror of McpServerConfig — to_config sets trusted: true)
│   ├── Plugin                     (registry entry: name, source URL, install path, version, enabled)
│   ├── PluginRegistry             (Vec<Plugin>; load/save/find/upsert/remove)
│   ├── PluginRegistry::save       (atomic tmp + rename, M6.16 BUG M2)
│   ├── registry_path / plugins_dir (scope-resolved)
│   ├── read_manifest              (.thclaws-plugin > .claude-plugin)
│   ├── install                    (full lifecycle: policy → fetch → locate → validate → rename → registry, with rollback)
│   ├── is_valid_plugin_name       (M6.16 BUG M1 — path-traversal block)
│   ├── set_enabled / find_installed / find_installed_with_scope / remove
│   ├── installed_plugins_all_scopes / all_plugins_all_scopes
│   ├── plugin_skill_dirs / plugin_command_dirs / plugin_agent_dirs / plugin_mcp_servers
│   ├── gc                         (M6.16.1 BUG L2 — zombie cleanup)
│   ├── fetch_into / download_zip (30s timeout, M6.16) / extract_zip (zip-slip safe)
│   ├── git_clone                  (tokio::process, 60s timeout, kill_on_drop, M6.16)
│   ├── locate_plugin_root         (root or single-wrapper-subdir descent)
│   └── tests                      (8 unit tests covering name validation, atomic save, gc, name collision precedence, manifest parsing, etc.)
│
├── repl.rs
│   ├── parse_plugin_subcommand    (CLI slash parser: install / remove / enable / disable / show / gc / list / marketplace / search / info)
│   ├── plugin_mcp_server_names    (M6.16.1 — replaces plugin_has_mcp_servers; returns sorted Vec<String>)
│   ├── PluginInstall handler      (CLI: skill-store refresh + emphasized MCP-restart hint)
│   ├── PluginEnable / PluginDisable / PluginRemove handlers (live refresh + MCP names list)
│   ├── PluginShow handler         (uses find_installed_with_scope to print scope, M6.16.1 BUG L3)
│   └── PluginGc handler           (M6.16.1 BUG L2)
│
├── shell_dispatch.rs
│   ├── refresh_after_plugin_change   (worker-state refresh helper)
│   ├── mcp_server_names              (Option<Vec<String>> per plugin)
│   ├── PluginInstall / PluginRemove / PluginEnable / PluginDisable / PluginShow / PluginGc handlers
│   └── (mirrors CLI behavior with the GUI WorkerState shape)
│
├── skills.rs
│   └── parse_git_subpath          (shared helper for #branch:subpath URLs)
│
├── policy/mod.rs + allowlist.rs
│   ├── check_url                  (host-pattern allowlist with `*.host` and segment wildcards)
│   └── normalize_url_for_match    (strip scheme + user@ + query + fragment + .git + port)
│
└── marketplace.rs
    └── find_plugin                (name → MarketplacePlugin entry → install_url)
```

---

## 9. Testing

`plugins::tests` ships 8 unit tests:

**Manifest + parsing:**
- `reads_native_manifest_then_falls_back_to_claude` — `.thclaws-plugin/plugin.json` wins over `.claude-plugin/plugin.json` when both present
- `locate_plugin_root_descends_single_wrapper` — single-subdir wrapper (zip `pack-v1/...` convention) resolved correctly

**Registry:**
- `registry_roundtrip_upsert_remove` — upsert replaces by name, remove by name
- `registry_toggle_enabled_persists` — flag round-trips through upsert
- `registry_save_atomic_uses_tmp_then_rename` (M6.16 M2) — saves leave no `.tmp` lingering, real file round-trips through load

**URL handling:**
- `is_zip_url_handles_query_and_fragment` — `.zip?token=...` / `.zip#frag` / `.ZIP` all detected; `.git` rejected

**Validation (M6.16 BUG M1):**
- `rejects_unsafe_plugin_names` — `""`, `"."`, `".."`, `"../foo"`, `"foo/bar"`, `"foo\bar"`, `"foo\0null"`, `"space inside"`, `"emoji-🦀"` all rejected
- `accepts_typical_plugin_names` — `"foo"`, `"foo-bar"`, `"foo_bar"`, `"foo.bar"`, `"Foo123"`, `".hidden"`, `"a"` all accepted

**Garbage collection (M6.16.1 BUG L2):**
- `gc_removes_entries_with_missing_dir` — zombie entry (path doesn't exist) gets removed; valid entry stays

CWD-mutating tests use a `scoped_user_home()` helper (HOME env-var swap) backed by `kms::test_env_lock()` — process-global mutex shared with kms / oauth tests so parallel sibling tests don't race.

H1 (refresh on mutation), L3 (scope in `/plugin show`), L4 (rollback on save failure) — GUI-flow / CLI-flow shape, covered by manual verification. Adding integration tests would need a fully-wired `WorkerState` fixture plus a fake-server harness; deferred until the test infrastructure exists for it.

M3 (git_clone timeout) and M4 (download_zip timeout) — hard to test without a stall-server fixture. The 60 s / 30 s caps are validated by the `Duration::from_secs(...)` literals; a real network-stall test would need a fake HTTP/git endpoint and a 60 s test runtime, neither worth it.

---

## 10. Migration notes / known limitations

### M6.16 fixes (`dev-log/134`)

| # | Severity | What | Where |
|---|---|---|---|
| H1 | HIGH | `/plugin remove` / `/plugin enable` / `/plugin disable` didn't refresh the in-process skill store; removed skills could still be invoked and lazy-read empty bodies. Now refreshed via `refresh_after_plugin_change`. | `shell_dispatch.rs` + `repl.rs` |
| M1 | MED | Manifest `name` was joined onto plugins dir without sanitization; path-traversal possible (`../../...`). | `plugins.rs::install` + `is_valid_plugin_name` |
| M2 | MED | `PluginRegistry::save` not atomic — crash mid-write corrupted `plugins.json`. | `plugins.rs:202` |
| M3 | MED | `git_clone` had no timeout; hung git server hung `/plugin install` indefinitely. | `plugins.rs::git_clone` (tokio::process + 60s) |
| M4 | MED | `download_zip` had no timeout; same shape as M6.14's skills fix that never propagated. | `plugins.rs::download_zip` (30s) |

### M6.16.1 follow-up

| # | Severity | What | Where |
|---|---|---|---|
| L2 | LOW | Zombie registry entries when user manually `rm -rf`'d a plugin dir; `/plugin gc` cleans them up. | `plugins.rs::gc` + dispatch |
| L3 | LOW | `/plugin show` didn't print scope; user couldn't tell which `--user` flag to pass to follow-up commands. | `find_installed_with_scope` + dispatch |
| L4 | LOW | Half-installed state if `registry.save` failed after rename; install now rolls back the rename or surfaces a clear orphan-cleanup error. | `plugins.rs::install` |
| (cleanup) | LOW | MCP-needs-restart messages were vague + the install message wrongly mentioned commands. Replaced `has_running_mcp_contributions(name) -> bool` with `mcp_server_names(name) -> Option<Vec<String>>` so every message lists exact server names. | All four handlers (CLI + GUI) |

### Deferred (still)

- **BUG L1 — MCP plugin lifecycle.** Tearing down per-plugin MCP subprocesses on disable + spawning them on enable. Needs a real design sprint that handles BOTH sides; a one-sided fix would be misleading. Tracked as the "MCP plugin lifecycle" project in the dev-plan backlog.

### Sprint chronology

| Sprint | Dev-log | What shipped |
|---|---|---|
| Initial plugin support | (early Phase) | PluginManifest + install (zip + git) + registry + plugin_skill_dirs / plugin_command_dirs / plugin_mcp_servers |
| Marketplace integration | `~127` | `/plugin install <name>` resolves marketplace entry → install_url |
| Live skill refresh on install | `~131` | Install handler refreshes skill store + rebuilds agent without restart |
| M6.16 audit | `134` | H1 + M1 + M2 + M3 + M4 (refresh-on-mutation, name sanitization, atomic save, both timeouts) |
| M6.16.1 follow-up | `134` | L2 + L3 + L4 + MCP-message cleanup |
