//! Agent definitions — named agent configs for sub-agents and team members.
//!
//! Load order (later overrides earlier):
//! 1. `~/.config/thclaws/agents.json` — user global (legacy JSON format)
//! 2. `~/.claude/agents/*.md` — user Claude Code
//! 3. `~/.config/thclaws/agents/*.md` — user thClaws
//! 4. `.claude/agents/*.md` — project Claude Code
//! 5. `.thclaws/agents/*.md` — project thClaws (highest priority)
//!
//! Plus any plugin-contributed agent dirs (see [`crate::plugins`]), which
//! are merged additively and never shadow the sources above.
//!
//! Markdown format (YAML frontmatter + body as instructions):
//! ```markdown
//! ---
//! name: researcher
//! description: Researches topics thoroughly
//! model: claude-sonnet-4-5
//! tools: Read, Grep, Glob, WebSearch
//! maxTurns: 20
//! ---
//! You are a research agent. Search the codebase and web...
//! ```
//!
//! Used by both the Task tool (sub-agents) and Agent Teams (teammates).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentDef {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub instructions: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default = "default_max_iterations")]
    pub max_iterations: usize,
    /// Tool names this agent can use. Empty = all built-in tools.
    #[serde(default)]
    pub tools: Vec<String>,
    /// Tools to exclude.
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Agent terminal color.
    #[serde(default)]
    pub color: Option<String>,
    /// Isolation mode: "worktree" creates a git worktree for the agent.
    #[serde(default)]
    pub isolation: Option<String>,
    /// Permission mode override.
    #[serde(default)]
    pub permission_mode: Option<String>,
}

fn default_max_iterations() -> usize {
    200
}

impl Default for AgentDef {
    fn default() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            instructions: String::new(),
            model: None,
            max_iterations: default_max_iterations(),
            tools: vec![],
            disallowed_tools: vec![],
            color: None,
            isolation: None,
            permission_mode: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentDefsConfig {
    #[serde(default)]
    pub agents: Vec<AgentDef>,
}

impl AgentDefsConfig {
    /// Load agent definitions from all sources (JSON + markdown directories).
    pub fn load() -> Self {
        Self::load_with_extra(&[])
    }

    /// Load, additionally walking each directory in `extra` after the
    /// standard dirs. Used by the plugin system to surface agent defs
    /// contributed by installed plugins. Standard dirs still win on name
    /// collision because they're loaded first — plugin agents are merged
    /// only when they don't clash.
    pub fn load_with_extra(extra: &[PathBuf]) -> Self {
        let mut config = Self::default();

        // 0. Built-in agent defs compiled into the binary. Seeded first
        // so every other source (legacy JSON, user/project md dirs)
        // overrides by name. Surface area is intentionally small —
        // built-ins ship for first-class operations like `/dream`.
        config.seed_builtins();

        // 1. Legacy JSON config.
        let json_path = Self::default_json_path();
        if json_path.exists() {
            if let Ok(contents) = std::fs::read_to_string(&json_path) {
                if let Ok(json_config) = serde_json::from_str::<AgentDefsConfig>(&contents) {
                    config.agents.extend(json_config.agents);
                }
            }
        }

        // 2. Standard markdown agent directories. Later entries in the
        // list override earlier ones (same name), so the order here sets
        // priority: user-global < project < … any plugin dirs appended
        // below.
        for dir in Self::agent_dirs() {
            if dir.exists() {
                config.load_md_dir(&dir);
            }
        }

        // 3. Plugin-contributed dirs. Walk them via `load_md_dir_no_clobber`
        // so a plugin can't shadow a user's or project's agent by name —
        // the existing entry is kept.
        for dir in extra {
            if dir.exists() {
                config.load_md_dir_no_clobber(dir);
            }
        }

        config
    }

    fn default_json_path() -> PathBuf {
        crate::util::home_dir()
            .map(|h| h.join(".config/thclaws/agents.json"))
            .unwrap_or_else(|| PathBuf::from("agents.json"))
    }

    /// Directories to scan for agent .md files, in priority order.
    /// Later entries override earlier ones (same name).
    fn agent_dirs() -> Vec<PathBuf> {
        let mut dirs = Vec::new();
        if let Some(home) = crate::util::home_dir() {
            dirs.push(home.join(".claude/agents")); // user Claude Code
            dirs.push(home.join(".config/thclaws/agents")); // user thClaws
        }
        dirs.push(PathBuf::from(".claude/agents")); // project Claude Code
        dirs.push(PathBuf::from(".thclaws/agents")); // project thClaws (highest priority)
        dirs
    }

    /// Load agent definitions from a directory of .md files.
    fn load_md_dir(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(agent) = Self::parse_agent_md(&path) {
                // Override existing agent with same name.
                if let Some(existing) = self.agents.iter_mut().find(|a| a.name == agent.name) {
                    *existing = agent;
                } else {
                    self.agents.push(agent);
                }
            }
        }
    }

    /// Variant of [`load_md_dir`] that keeps the existing agent on a name
    /// collision. Used for plugin-contributed dirs so a plugin can't
    /// shadow the user's own agent defs.
    fn load_md_dir_no_clobber(&mut self, dir: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            if let Some(agent) = Self::parse_agent_md(&path) {
                if self.agents.iter().any(|a| a.name == agent.name) {
                    continue;
                }
                self.agents.push(agent);
            }
        }
    }

    /// Seed the config with built-in agent defs compiled into the
    /// binary. Each entry pairs a fallback name (used if the markdown
    /// has no `name:` frontmatter) with the embedded source. Built-ins
    /// land at the lowest priority so any user/project agent def with
    /// the same name will override them.
    /// Apply settings.json overrides for built-in subagents' `model:`
    /// field. Each built-in that needs settings tunability gets a
    /// matching `<name>_subagent_model` field on AppConfig; this
    /// helper resolves them against the loaded AgentDefs by name and
    /// edits in place. Disk-loaded user agent files at
    /// `.thclaws/agents/<name>.md` still win because they replaced
    /// the embedded AgentDef during the prior load_md_dir pass — this
    /// only edits whatever's currently registered under that name
    /// (built-in or user, doesn't matter).
    pub fn apply_builtin_subagent_overrides(&mut self, config: &crate::config::AppConfig) {
        if let Some(ref m) = config.translator_subagent_model {
            if let Some(def) = self.agents.iter_mut().find(|d| d.name == "translator") {
                def.model = Some(m.clone());
            }
        }
        // Future built-in subagents add their override branch here.
        // Pattern: read AppConfig::<name>_subagent_model, find AgentDef
        // by name, replace `model` field. Three lines per built-in.
    }

    fn seed_builtins(&mut self) {
        const BUILTINS: &[(&str, &str)] = &[
            ("dream", include_str!("default_prompts/dream.md")),
            ("translator", include_str!("default_prompts/translator.md")),
            ("kms-linker", include_str!("default_prompts/kms-linker.md")),
            (
                "kms-reconcile",
                include_str!("default_prompts/kms-reconcile.md"),
            ),
        ];
        for (fallback_name, raw) in BUILTINS {
            if let Some(agent) = Self::parse_agent_md_str(raw, fallback_name) {
                self.agents.push(agent);
            }
        }
    }

    /// Parse an agent .md file with YAML frontmatter.
    fn parse_agent_md(path: &Path) -> Option<AgentDef> {
        let raw = std::fs::read_to_string(path).ok()?;
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        Self::parse_agent_md_str(&raw, fallback)
    }

    /// Parse an agent .md body (frontmatter + instructions) from an
    /// in-memory string. `fallback_name` is used when the frontmatter
    /// has no `name:` key — for disk loads this is the file stem; for
    /// embedded built-ins it's a hard-coded name.
    fn parse_agent_md_str(raw: &str, fallback_name: &str) -> Option<AgentDef> {
        let (frontmatter, body) = crate::memory::parse_frontmatter(raw);

        let name = frontmatter
            .get("name")
            .cloned()
            .unwrap_or_else(|| fallback_name.to_string());

        let description = frontmatter.get("description").cloned().unwrap_or_default();
        let model = frontmatter.get("model").cloned();
        let color = frontmatter.get("color").cloned();
        let permission_mode = frontmatter
            .get("permissionMode")
            .or_else(|| frontmatter.get("permission_mode"))
            .cloned();
        let isolation = frontmatter.get("isolation").cloned();

        let max_iterations = frontmatter
            .get("maxTurns")
            .or_else(|| frontmatter.get("max_iterations"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(default_max_iterations());

        let tools = frontmatter
            .get("tools")
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        let disallowed_tools = frontmatter
            .get("disallowedTools")
            .or_else(|| frontmatter.get("disallowed_tools"))
            .map(|s| {
                s.split(',')
                    .map(|t| t.trim().to_string())
                    .filter(|t| !t.is_empty())
                    .collect()
            })
            .unwrap_or_default();

        Some(AgentDef {
            name,
            description,
            instructions: body.trim().to_string(),
            model,
            max_iterations,
            tools,
            disallowed_tools,
            color,
            isolation,
            permission_mode,
        })
    }

    pub fn load_from_path(path: &PathBuf) -> Self {
        if !path.exists() {
            return Self::default();
        }
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str(&contents).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn get(&self, name: &str) -> Option<&AgentDef> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn names(&self) -> Vec<&str> {
        self.agents.iter().map(|a| a.name.as_str()).collect()
    }

    pub fn as_map(&self) -> HashMap<String, AgentDef> {
        self.agents
            .iter()
            .map(|a| (a.name.clone(), a.clone()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn load_from_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(
            &path,
            r#"{"agents": [
                {"name": "researcher", "instructions": "Research things", "max_iterations": 5},
                {"name": "coder", "instructions": "Write code", "tools": ["Read", "Write", "Edit"]}
            ]}"#,
        )
        .unwrap();

        let config = AgentDefsConfig::load_from_path(&path);
        assert_eq!(config.agents.len(), 2);
        assert_eq!(config.get("researcher").unwrap().max_iterations, 5);
        assert_eq!(
            config.get("coder").unwrap().tools,
            vec!["Read", "Write", "Edit"]
        );
        assert!(config.get("nonexistent").is_none());
    }

    #[test]
    fn missing_file_returns_default() {
        let config = AgentDefsConfig::load_from_path(&PathBuf::from("/nonexistent/agents.json"));
        assert!(config.agents.is_empty());
    }

    #[test]
    fn names_lists_all() {
        let config = AgentDefsConfig {
            agents: vec![
                AgentDef {
                    name: "a".into(),
                    ..Default::default()
                },
                AgentDef {
                    name: "b".into(),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(config.names(), vec!["a", "b"]);
    }

    #[test]
    fn parse_agent_md_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("researcher.md");
        std::fs::write(
            &path,
            "\
---
name: researcher
description: Researches topics
model: claude-sonnet-4-5
tools: Read, Grep, Glob, WebSearch
maxTurns: 20
color: blue
---
You are a research agent. Search thoroughly and report findings.
",
        )
        .unwrap();

        let agent = AgentDefsConfig::parse_agent_md(&path).unwrap();
        assert_eq!(agent.name, "researcher");
        assert_eq!(agent.description, "Researches topics");
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(agent.tools, vec!["Read", "Grep", "Glob", "WebSearch"]);
        assert_eq!(agent.max_iterations, 20);
        assert_eq!(agent.color.as_deref(), Some("blue"));
        assert!(agent.instructions.contains("research agent"));
    }

    #[test]
    fn parse_agent_md_name_from_filename() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("backend.md");
        std::fs::write(
            &path,
            "\
---
description: Backend developer
---
Build REST APIs.
",
        )
        .unwrap();

        let agent = AgentDefsConfig::parse_agent_md(&path).unwrap();
        assert_eq!(agent.name, "backend");
        assert_eq!(agent.instructions, "Build REST APIs.");
    }

    #[test]
    fn load_md_dir_no_clobber_keeps_existing() {
        let dir = tempdir().unwrap();

        // A project-level agent already in the config.
        let mut config = AgentDefsConfig {
            agents: vec![AgentDef {
                name: "coder".into(),
                instructions: "project version".into(),
                ..Default::default()
            }],
        };

        // A plugin dir with an agent of the same name PLUS a new one.
        let plugin_dir = dir.path().join("plugin-agents");
        std::fs::create_dir_all(&plugin_dir).unwrap();
        std::fs::write(
            plugin_dir.join("coder.md"),
            "\
---
name: coder
---
plugin version (should NOT win)
",
        )
        .unwrap();
        std::fs::write(
            plugin_dir.join("reviewer.md"),
            "\
---
name: reviewer
---
plugin-only reviewer
",
        )
        .unwrap();

        config.load_md_dir_no_clobber(&plugin_dir);
        assert_eq!(config.get("coder").unwrap().instructions, "project version");
        assert_eq!(
            config.get("reviewer").unwrap().instructions,
            "plugin-only reviewer"
        );
    }

    #[test]
    fn seed_builtins_includes_translator() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let translator = config
            .get("translator")
            .expect("built-in translator agent should be seeded");
        assert_eq!(translator.name, "translator");
        assert!(!translator.instructions.is_empty());
        // Frontmatter declares the gpt-4.1 default.
        assert_eq!(translator.model.as_deref(), Some("gpt-4.1"));
        // Tool whitelist captured — translator has no Bash, no KMS,
        // no Task. Just file I/O.
        assert!(translator.tools.iter().any(|t| t == "Read"));
        assert!(translator.tools.iter().any(|t| t == "Write"));
        assert!(!translator.tools.iter().any(|t| t == "Bash"));
    }

    /// settings.json `translator_subagent_model` swaps the embedded
    /// `gpt-4.1` for the override value before the AgentDef reaches
    /// the factory. Disk-resident user agents at
    /// `.thclaws/agents/translator.md` would have replaced the
    /// AgentDef during the prior load_md_dir pass, so this only
    /// runs against the embedded built-in.
    #[test]
    fn apply_builtin_subagent_overrides_replaces_translator_model() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();

        let mut app_config = crate::config::AppConfig::default();
        app_config.translator_subagent_model = Some("claude-sonnet-4-6".into());

        config.apply_builtin_subagent_overrides(&app_config);
        let translator = config.get("translator").unwrap();
        assert_eq!(translator.model.as_deref(), Some("claude-sonnet-4-6"));
    }

    /// Absent override leaves the embedded default in place.
    #[test]
    fn apply_builtin_subagent_overrides_no_op_when_absent() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();

        let app_config = crate::config::AppConfig::default();
        config.apply_builtin_subagent_overrides(&app_config);
        let translator = config.get("translator").unwrap();
        assert_eq!(translator.model.as_deref(), Some("gpt-4.1"));
    }

    #[test]
    fn seed_builtins_includes_kms_linker() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let linker = config
            .get("kms-linker")
            .expect("built-in kms-linker agent should be seeded");
        assert_eq!(linker.name, "kms-linker");
        assert!(!linker.instructions.is_empty());
        // Tool whitelist: KMS read/write surface only — no Bash, no
        // KmsDelete (the operating procedure forbids deletion).
        assert!(linker.tools.iter().any(|t| t == "KmsRead"));
        assert!(linker.tools.iter().any(|t| t == "KmsSearch"));
        assert!(linker.tools.iter().any(|t| t == "KmsWrite"));
        assert!(linker.tools.iter().any(|t| t == "KmsAppend"));
        assert!(!linker.tools.iter().any(|t| t == "KmsDelete"));
        assert!(!linker.tools.iter().any(|t| t == "Bash"));
    }

    #[test]
    fn seed_builtins_includes_kms_reconcile() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let reconcile = config
            .get("kms-reconcile")
            .expect("built-in kms-reconcile agent should be seeded");
        assert_eq!(reconcile.name, "kms-reconcile");
        assert!(!reconcile.instructions.is_empty());
        // Tool whitelist: same shape as kms-linker — KMS surface only,
        // no KmsDelete (reconcile preserves history; rewrites with
        // History sections, never silently drops claims), no Bash.
        assert!(reconcile.tools.iter().any(|t| t == "KmsRead"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsSearch"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsWrite"));
        assert!(reconcile.tools.iter().any(|t| t == "KmsAppend"));
        assert!(reconcile.tools.iter().any(|t| t == "TodoWrite"));
        assert!(!reconcile.tools.iter().any(|t| t == "KmsDelete"));
        assert!(!reconcile.tools.iter().any(|t| t == "Bash"));
        // Procedure-defining keywords from the body.
        assert!(reconcile.instructions.contains("History"));
        assert!(reconcile.instructions.contains("Conflict"));
    }

    #[test]
    fn seed_builtins_includes_dream() {
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let dream = config
            .get("dream")
            .expect("built-in dream agent should be seeded");
        assert_eq!(dream.name, "dream");
        assert!(!dream.instructions.is_empty());
        // Tool whitelist must be wired up so the dream agent can mutate
        // KMS — bare-bones smoke check.
        assert!(dream.tools.iter().any(|t| t == "KmsDelete"));
    }

    #[test]
    fn user_dream_md_overrides_builtin() {
        let dir = tempdir().unwrap();
        let mut config = AgentDefsConfig::default();
        config.seed_builtins();
        let builtin_instructions = config.get("dream").unwrap().instructions.clone();

        let md_dir = dir.path().join("agents");
        std::fs::create_dir_all(&md_dir).unwrap();
        std::fs::write(
            md_dir.join("dream.md"),
            "\
---
name: dream
---
custom user dream prompt
",
        )
        .unwrap();

        config.load_md_dir(&md_dir);
        let dream = config.get("dream").unwrap();
        assert_eq!(dream.instructions, "custom user dream prompt");
        assert_ne!(dream.instructions, builtin_instructions);
    }

    #[test]
    fn load_md_dir_overrides_json() {
        let dir = tempdir().unwrap();

        // JSON agent.
        let mut config = AgentDefsConfig {
            agents: vec![AgentDef {
                name: "coder".into(),
                instructions: "old instructions".into(),
                ..Default::default()
            }],
        };

        // MD agent with same name overrides.
        let md_dir = dir.path().join("agents");
        std::fs::create_dir_all(&md_dir).unwrap();
        std::fs::write(
            md_dir.join("coder.md"),
            "\
---
name: coder
---
new instructions
",
        )
        .unwrap();

        config.load_md_dir(&md_dir);
        assert_eq!(
            config.get("coder").unwrap().instructions,
            "new instructions"
        );
    }
}
