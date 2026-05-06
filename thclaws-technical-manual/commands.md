# Prompt commands

Markdown templates that fire on `/<name> [args]` and inject as a user message into the next turn. Inherited from Claude Code's slash-command system; thClaws supports the same on-disk shape so existing `.claude/commands/` directories work unchanged. Compared to skills, commands are intentionally minimal: no script bundle, no model-side tool to load, no trust gate. Just a `.md` file that becomes a prompt.

This doc covers: file format, discovery + precedence (project / user / plugin), resolution flow on both surfaces, the `$ARGUMENTS` substitution rule, skills-vs-commands decision criteria, code organization, and known limitations.

**Source modules:**
- `crates/core/src/commands.rs` — types, discovery, parse, render
- `crates/core/src/repl.rs` — CLI `/<name>` resolution arm (`repl.rs:2601-2632`)
- `crates/core/src/shared_session.rs` — GUI worker `/<name>` resolution arm (the `command_store` fallback in `handle_line`)
- `crates/core/src/gui.rs` — slash popup data feed (`gui.rs:1815-1862`)
- `crates/core/src/plugins.rs` — `plugin_command_dirs()` for plugin contributions
- `crates/core/src/memory.rs` — `parse_frontmatter` (shared YAML reader, also used by skills + memory)

---

## 1. What a command is

A command is a single `.md` file. The filename stem is the command name (`deploy.md` → `/deploy`). Optional YAML frontmatter carries metadata; the body is the prompt template.

```markdown
---
description: Deploy the current branch to staging
whenToUse: When the user asks to push to staging
---

Deploy the current branch to the staging environment. Run the
pre-deploy checks first, then `make deploy STAGE=staging`.

User-supplied detail: $ARGUMENTS
```

| Frontmatter key | Required | Notes |
|---|---|---|
| `name` | optional | Falls back to filename stem if absent |
| `description` | recommended | One-line summary; surfaced in the `/` popup |
| `whenToUse` *or* `when_to_use` | optional | Trigger criteria; not yet displayed by either surface but parsed and stored for future use |
| anything else | optional | Ignored; useful as inline doc for the next reader |

Body conventions:
- **`$ARGUMENTS`** is replaced with whatever the user typed after the command name (trimmed). If the body has no placeholder and the user did supply args, the args are appended after a blank line so the model still sees them — no input is silently dropped.
- Anything else in the body is sent verbatim. Markdown formatting passes through untouched (the model receives it as raw text in a user message).

---

## 2. Discovery: four standard dirs + plugin extras

`CommandStore::discover_with_extra(extra)` walks five categories of directory in this load order:

| # | Directory | Scope | Precedence |
|---|---|---|---|
| 1 | `.thclaws/commands/` | project | **highest** |
| 2 | `.claude/commands/` | project (Claude Code compat) | wins over user |
| 3 | `~/.config/thclaws/commands/` | user | thClaws-native |
| 4 | `~/.claude/commands/` | user (Claude Code compat) | fallback |
| 5 | plugin-contributed dirs (manifest `commands:` field, or `<plugin>/commands/` fallback) | per-plugin | last; **see "First-write wins" below** |

`load_dir` uses `entry(name).or_insert(cmd)` — **first write wins**, opposite of skills. So a project-local `deploy.md` correctly overrides a user-global `deploy.md`. Plugins are loaded LAST and lose every name collision because earlier dirs already populated those slots.

> **Difference from skills.** SkillStore uses `HashMap::insert` (last-write-wins) and reorders dirs so project beats plugin beats user. CommandStore uses `or_insert` (first-write-wins) and orders dirs project → user → plugin. Both end up with "project highest, plugin loses on collision," but via opposite mechanisms. If you're hacking on either store, don't copy code without checking which semantic you need.

`command_dirs()` (`commands.rs:79-88`) uses `insert(0, ...)` to push the project dirs to the front of the vector before user dirs are appended. Plugin dirs come last via the `extra` parameter.

```rust
fn command_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = crate::util::home_dir() {
        dirs.push(home.join(".config/thclaws/commands"));
        dirs.push(home.join(".claude/commands"));
    }
    dirs.insert(0, PathBuf::from(".claude/commands"));   // project Claude
    dirs.insert(0, PathBuf::from(".thclaws/commands"));  // project thClaws (highest)
    dirs
}
```

**Plugin contributions** (`plugins.rs:450-468`) work the same as skills: a plugin's manifest can declare an explicit `commands` field (list of paths relative to the plugin root), or omit it and rely on the conventional `<plugin>/commands/` subdir.

---

## 3. Resolution: `/<name> [args]` shortcut

Both CLI and GUI route any `/<word>` that `parse_slash` returns as `Unknown` through the same two-tier fallback:

```
USER: /<word> [args]
  │
  ▼
parse_slash (repl.rs:591)
  ├── built-in command (e.g. /help, /model, /skill marketplace)
  │     → dispatch_slash, done
  └── Unknown(...)
        │
        ▼
1. SKILL lookup
        skill_store.contains_key(word)?
        ├── yes → rewrite to "Call Skill(name: \"<word>\")…" prompt → agent.run_turn
        └── no  → fall through
        │
        ▼
2. COMMAND lookup
        CommandStore::discover_with_extra(&plugin_command_dirs()).get(word)?
        ├── Some(cmd) → cmd.render(args) → agent.run_turn
        └── None → dispatch_slash → "unknown command: <word>"
```

CLI implementation: `repl.rs:2601-2632`. GUI worker: `shared_session.rs::handle_line` (search for `command_store` near the slash-handling block). Both surfaces re-discover the store **per call** — cheap (just reads a handful of `.md` files), and lets freshly-installed plugin commands surface without a restart.

### `cmd.render(args)` rules (`commands.rs:38-47`)

```rust
pub fn render(&self, args: &str) -> String {
    let args = args.trim();
    if self.body.contains("$ARGUMENTS") {
        self.body.replace("$ARGUMENTS", args)             // (a) explicit substitution
    } else if args.is_empty() {
        self.body.clone()                                  // (b) no args, no placeholder
    } else {
        format!("{}\n\n{args}", self.body.trim_end())     // (c) implicit append
    }
}
```

- **(a) Body has `$ARGUMENTS`** → exact substitution. All occurrences replaced with the trimmed args (which may be empty, in which case the placeholder becomes empty string).
- **(b) No placeholder, no args** → body sent verbatim.
- **(c) No placeholder, has args** → args appended after a blank line. Defensive: a user typing `/deploy to prod` against a body that doesn't use `$ARGUMENTS` still gets their detail through to the model.

### Resolution feedback

Both surfaces echo a dim hint so the user sees what `/<word>` resolved to:

- **CLI:** `(/deploy → prompt from /Users/jimmy/.thclaws/commands/deploy.md)` — printed by `repl.rs:2625-2627`
- **GUI:** same shape via `emit_command_resolution_hint` (`shared_session.rs`); rendered as a dim system bubble

The path in the hint is the literal `source` field on `CommandDef` — useful when two `.md` files exist under different scopes and you want to know which one fired.

### Discovery in the GUI slash popup

`gui.rs:1815-1862` builds the popup payload by calling `CommandStore::discover_with_extra(&plugin_command_dirs())` (matching what the worker resolves against). Keep these two call sites in lock-step — if the popup uses a different discovery shape than the resolver, users see entries in the popup that fail when selected. The previous implementation had this drift (popup discovered without plugin extras while the CLI resolver included them), and was the reason commands appeared visually broken in GUI.

---

## 4. Skills vs commands

Both fire as `/<name>`. When should you ship which?

| | Command | Skill |
|---|---|---|
| **Ships as** | One `.md` file | A directory with `SKILL.md` + optional `scripts/`, `requirements.txt`, etc. |
| **Loaded by** | Inline expansion into next user message | `Skill` tool — the model calls `Skill(name: "<n>")` and gets the body |
| **Has scripts?** | No | Yes (`scripts/foo.py`, auto-listed with interpreter hint) |
| **Has Python deps?** | No | Yes (`requirements.txt` auto-surfaced + bash auto-venv) |
| **Trust gate?** | No | No (skills don't need trust; only MCP-Apps widgets do) |
| **System-prompt cost** | Zero | Per-skill (configurable strategy) |
| **Lazy-loaded body?** | No (always read on resolve) | Yes (frontmatter at boot, body on first call) |
| **Best for** | One-shot prompt injections, project conventions, "always start the deploy task this way" | Workflows that need pre-built scripts, expert instructions the model should consult mid-task, anything bigger than a single prompt |
| **Token cost** | One user message worth | One tool result worth (skill body + auto-listed scripts) |

Rough heuristic: if you can express what you want in a single paragraph the model reads as a user message, ship a command. If you need scripts the model should `Bash` into existence, or instructions that span multiple turns, ship a skill.

When both a skill and a command exist with the same name, **skill wins** — the resolution order in §3 checks skills first.

---

## 5. Differences from Claude Code

The on-disk format is intentionally compatible — point thClaws at an existing `.claude/commands/` and it works unchanged. Two extensions:

1. **Native dirs.** `.thclaws/commands/` (project) and `~/.config/thclaws/commands/` (user) are checked alongside the Claude Code paths. Project-thClaws beats project-Claude beats user-thClaws beats user-Claude.
2. **Plugin contributions.** Claude Code commands ship as either user-global or project-local files; thClaws additionally accepts plugin-bundled commands via `plugins.rs::plugin_command_dirs()`.

What thClaws does NOT (yet) extend:
- `whenToUse` is parsed but not used by either surface (no auto-suggest based on context). Reserved for future model-side hinting.
- No tool-use API for commands — the model can't enumerate or call them. (Skills get `SkillList` / `SkillSearch` / `Skill`; commands are user-driven only.)
- No live-refresh on `/plugin install` or `/skill install` — discovery happens per `/`-resolution call, so newly-added commands surface on the next user invocation without a restart.

---

## 6. Code organization

```
crates/core/src/
├── commands.rs                  ── ~205 LOC, the whole subsystem
│   ├── CommandDef               (name, description, when_to_use, body, source)
│   ├── CommandDef::render       ($ARGUMENTS substitution + implicit-append)
│   ├── CommandStore             (HashMap<name, CommandDef>)
│   ├── CommandStore::discover                  (no extras)
│   ├── CommandStore::discover_with_extra       (with plugin dirs)
│   ├── CommandStore::command_dirs              (4 standard paths, project-first)
│   ├── CommandStore::load_dir / parse          (first-write-wins, frontmatter parse)
│   └── tests                                   (parse, render, collision precedence)
│
├── repl.rs
│   ├── line 1804: command_store discovery at run_repl startup
│   └── line 2601-2632: `/<word>` resolution arm (skill check → command check → fall through)
│
├── shared_session.rs
│   └── handle_line: same `/<word>` resolution arm — re-discovers per call so
│                    new plugin commands surface without restart
│
├── gui.rs
│   └── line 1815-1862: slash popup data feed (built-in + custom + skills);
│                        custom commands use discover_with_extra to match resolver
│
└── plugins.rs
    └── plugin_command_dirs (manifest `commands:` field + `<plugin>/commands/` fallback)
```

---

## 7. Testing

`commands::tests` covers the parse + render + precedence surface. ~5 tests:

- **`parse_file_extracts_frontmatter_and_body`** — `description` from YAML, body trimmed of leading whitespace
- **`render_substitutes_arguments`** — `$ARGUMENTS` exact substitution; whitespace trimming on the args input
- **`render_without_placeholder_appends_args`** — no placeholder + no args → verbatim body; with args → appended after blank line
- **`earlier_dir_wins_on_name_collision`** — pins the first-write-wins semantic so a future refactor to `insert` last-wins would break this test loudly

Resolution arms (CLI + GUI) are exercised by manual verification — they're inlined into the slash-handling blocks in `repl.rs` and `shared_session.rs` rather than extracted to a callable function, so a unit test would need significant refactoring. If you touch either resolver, add a regression test for the specific behavior you changed.

---

## 8. Known limitations

- **No `whenToUse` surfacing** — parsed but unused. The model can't be told "when the user mentions X, suggest `/foo`" via this field today; a future revision could add a system-prompt section listing commands + triggers (similar to `skills_listing_strategy: "full"`).
- **No model-side enumeration** — there's no `CommandList` / `CommandSearch` tool. Commands are user-driven only; the model can't say "the user wants X — let me invoke `/foo`."
- **Plugin precedence loses on collision with user commands** — by design, but worth noting: if you install a plugin that ships `deploy.md` and you already have `~/.claude/commands/deploy.md`, the user file wins. If you actually want plugin override, drop the user file or rename the plugin's command.
- **No live broadcast on install** — discovery is per `/`-resolution and per popup-render call. There's no IPC event saying "command set changed"; the popup re-fetches when the frontend opens it.

### Sprint chronology

| Sprint | Dev-log | What shipped |
|---|---|---|
| Initial | (early Phase) | CommandStore + parse + CLI resolution arm |
| Plugin support | (later) | `plugin_command_dirs()` + `discover_with_extra` |
| GUI parity fix | `~134` (this sprint) | GUI worker now resolves `command_store` after the skill check; popup includes plugin extras. Pre-fix the GUI popup showed commands but selecting them produced "unknown command", and plugin-contributed commands were absent from the popup entirely. |
