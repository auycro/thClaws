//! Sub-agent tool — spawn nested agents with depth tracking and
//! named agent definitions.
//!
//! Supports multi-level recursion up to `max_depth` (default 3).
//! Child agents include their own `Task` tool at `depth + 1`, so
//! they can delegate further. At max depth, the tool refuses.
//!
//! Named agents: if `agent` field is provided in the input, loads
//! the definition from `~/.config/thclaws/agents.json` and uses
//! its instructions, model override, and tool subset.

use crate::agent::{collect_agent_turn_with_cancel, Agent};
use crate::agent_defs::{AgentDef, AgentDefsConfig};
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::providers::Provider;
use crate::tools::{req_str, Tool, ToolRegistry};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};

pub const TOOL_NAME: &str = "Task";
pub const DEFAULT_MAX_DEPTH: usize = 3;

/// Mutable state shared between a `ProductionAgentFactory` and its
/// owning worker (CLI `run_repl` locals, GUI `WorkerState`).
///
/// Pre-fix the factory captured `system: String` + `base_tools:
/// ToolRegistry` at construction and never refreshed them — mid-
/// session mutators (`/mcp add`, `/skill install`, `/kms use`,
/// `/reload-prompt`, AGENTS.md / memory edits via `/reload-prompt`)
/// reached the parent agent but not the factory. Subagents spawned
/// after any of those saw the startup-time system prompt with no
/// new MCP tools.
///
/// Now the worker holds an `Arc<RwLock<FactorySnapshot>>` and the
/// factory holds a clone of that same `Arc`. Worker writes through
/// `refresh_factory_snapshot` / `update_factory_snapshot`; factory
/// reads on every `build()`. Child factories inherit the same `Arc`
/// (via `snapshot.clone()`) so nested subagents also pick up live
/// state.
///
/// Cheap: cloning a `ToolRegistry` is just cloning a `HashMap<String,
/// Arc<dyn Tool>>` — tool objects themselves are Arc'd, only the
/// map shape is copied.
pub struct FactorySnapshot {
    pub system: String,
    pub tools: ToolRegistry,
}

/// How to construct a child agent. Implementations produce a brand-new
/// `Agent` with the appropriate configuration.
#[async_trait]
pub trait AgentFactory: Send + Sync {
    /// Build a child agent. `agent_def` is `Some` if the Task input
    /// specified a named agent; `None` for the default.
    async fn build(
        &self,
        prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent>;
}

/// M6.33: production agent factory shared by CLI (`run_repl`) and GUI
/// (`build_state`). Pre-fix the CLI had its own `ReplAgentFactory` and
/// the GUI had no factory at all (Task tool unregistered — SUB1).
/// Consolidated here so both surfaces get identical subagent behavior.
///
/// Fields capture the parent's runtime state for propagation to child
/// agents:
/// - `provider` / `model` — wire layer for the child's LLM calls
/// - `base_tools` — tool registry the child inherits (filtered by
///   agent_def.tools allow-list + agent_def.disallowed_tools deny-list
///   inside `build`)
/// - `system` — parent's full system prompt (CLAUDE.md + memory + KMS +
///   plan + todos), copied to the child + agent_def addendum + the
///   embedded `subagent.md` "you are a sub-agent" wording
/// - `max_iterations` — fallback when agent_def doesn't specify
/// - `max_depth` — recursion ceiling; child gets a Task tool only when
///   child_depth < max_depth
/// - `agent_defs` — registry of named agents (for nested Task calls)
/// - `approver` + `permission_mode` — M6.20 BUG H1: parent's gate
///   propagates so subagents can't silently bypass Ask mode
/// - `cancel` — M6.33 SUB4: parent's cancel token propagates so
///   ctrl-C reaches a runaway subagent. CLI passes `None` (no cancel
///   plumbing yet); GUI passes the worker's CancelToken.
pub struct ProductionAgentFactory {
    pub provider: Arc<dyn Provider>,
    /// Live view of the parent agent's system prompt + tool registry.
    /// Shared by Arc with the worker — see [`FactorySnapshot`] docs.
    pub snapshot: Arc<RwLock<FactorySnapshot>>,
    pub model: String,
    pub max_iterations: usize,
    pub max_depth: usize,
    /// Per-request output token budget propagated from `AppConfig::max_tokens`.
    /// Subagents inherit the parent's value so a project's `settings.json`
    /// `maxTokens` override applies uniformly. Issue #72: pre-fix subagents
    /// hit the hardcoded `Agent::new` default of 8192 even when the parent
    /// was correctly configured.
    pub max_tokens: u32,
    pub agent_defs: AgentDefsConfig,
    pub approver: Arc<dyn ApprovalSink>,
    pub permission_mode: PermissionMode,
    pub cancel: Option<CancelToken>,
    /// M6.35 HOOK1: lifecycle hooks propagate parent → subagent so a
    /// pre/post_tool_use hook fires for tool calls inside a Task spawn,
    /// not just at the top-level agent. Audit hooks would otherwise miss
    /// every subagent action — silent gap.
    pub hooks: Option<Arc<crate::hooks::HooksConfig>>,
}

#[async_trait]
impl AgentFactory for ProductionAgentFactory {
    async fn build(
        &self,
        _prompt: &str,
        agent_def: Option<&AgentDef>,
        child_depth: usize,
    ) -> Result<Agent> {
        let model = agent_def
            .and_then(|d| d.model.as_deref())
            .unwrap_or(&self.model);

        // Snapshot the live parent state ONCE — system + tools are
        // both needed and we want them to come from the same instant
        // (so a refresh between the two reads can't tear).
        let (parent_system, base_tools) = {
            let snap = self.snapshot.read().expect("factory snapshot read lock");
            (snap.system.clone(), snap.tools.clone())
        };

        // System prompt: parent's full prompt + (optional) agent
        // instructions + (when nested) the subagent-mode addendum.
        let mut system = agent_def
            .map(|d| {
                if d.instructions.is_empty() {
                    parent_system.clone()
                } else {
                    format!(
                        "{}\n\n# Agent instructions\n{}",
                        parent_system, d.instructions
                    )
                }
            })
            .unwrap_or_else(|| parent_system.clone());
        if child_depth > 0 {
            system.push_str(&crate::prompts::load(
                "subagent",
                crate::prompts::defaults::SUBAGENT,
            ));
        }
        let max_iter = agent_def
            .map(|d| d.max_iterations)
            .unwrap_or(self.max_iterations);

        // Tool registry: agent_def.tools allow-list (when non-empty)
        // intersects base_tools, then agent_def.disallowed_tools
        // deny-list removes anything in it. M6.33 SUB2: pre-fix
        // disallowed_tools was parsed but never applied — agent
        // definitions claiming `disallowed_tools: Bash` got Bash anyway.
        let mut tools = if let Some(def) = agent_def {
            if def.tools.is_empty() {
                base_tools.clone()
            } else {
                let mut filtered = ToolRegistry::new();
                for name in &def.tools {
                    if let Some(tool) = base_tools.get(name) {
                        filtered.register(tool);
                    }
                }
                filtered
            }
        } else {
            base_tools.clone()
        };
        if let Some(def) = agent_def {
            for name in &def.disallowed_tools {
                tools.remove(name);
            }
        }

        // Add a Task tool at the next depth (multi-level recursion).
        // child_depth < max_depth → register; otherwise the leaf
        // subagent has no Task tool and the chain stops.
        if child_depth < self.max_depth {
            let child_factory = Arc::new(ProductionAgentFactory {
                provider: self.provider.clone(),
                // Share the SAME snapshot Arc so nested subagents
                // also see live state updates. Cloning the Arc is
                // O(1) — just a refcount bump.
                snapshot: self.snapshot.clone(),
                model: self.model.clone(),
                max_iterations: self.max_iterations,
                max_depth: self.max_depth,
                max_tokens: self.max_tokens,
                agent_defs: self.agent_defs.clone(),
                approver: self.approver.clone(),
                permission_mode: self.permission_mode,
                cancel: self.cancel.clone(),
                hooks: self.hooks.clone(),
            });
            let mut child_tool = SubAgentTool::new(child_factory)
                .with_depth(child_depth)
                .with_max_depth(self.max_depth)
                .with_agent_defs(self.agent_defs.clone());
            if let Some(c) = self.cancel.clone() {
                child_tool = child_tool.with_cancel(c);
            }
            tools.register(Arc::new(child_tool));
        }

        // M6.33 SUB4: thread parent's cancel token into the child agent
        // so retry-backoff sleeps + collect_agent_turn observe ctrl-C.
        let mut agent = Agent::new(self.provider.clone(), tools, model, &system)
            .with_max_iterations(max_iter)
            .with_max_tokens(self.max_tokens)
            .with_approver(self.approver.clone())
            .with_permission_mode(self.permission_mode);
        if let Some(c) = self.cancel.clone() {
            agent = agent.with_cancel(c);
        }
        // M6.35 HOOK1: subagent inherits parent's hooks so audit hooks
        // see Task-spawned tool calls too.
        if let Some(h) = self.hooks.clone() {
            agent = agent.with_hooks(h);
        }
        Ok(agent)
    }
}

pub struct SubAgentTool {
    factory: Arc<dyn AgentFactory>,
    depth: usize,
    max_depth: usize,
    /// Agent definitions loaded at startup.
    agent_defs: crate::agent_defs::AgentDefsConfig,
    /// M6.33 SUB4: parent's cancel token. Observed by
    /// `collect_agent_turn_with_cancel` so ctrl-C reaches a runaway
    /// subagent. None when no parent cancel is wired (CLI today).
    cancel: Option<CancelToken>,
}

impl SubAgentTool {
    pub fn new(factory: Arc<dyn AgentFactory>) -> Self {
        Self {
            factory,
            depth: 0,
            max_depth: DEFAULT_MAX_DEPTH,
            agent_defs: crate::agent_defs::AgentDefsConfig::load_with_extra(
                &crate::plugins::plugin_agent_dirs(),
            ),
            cancel: None,
        }
    }

    pub fn with_depth(mut self, depth: usize) -> Self {
        self.depth = depth;
        self
    }

    pub fn with_max_depth(mut self, max_depth: usize) -> Self {
        self.max_depth = max_depth;
        self
    }

    pub fn with_agent_defs(mut self, defs: crate::agent_defs::AgentDefsConfig) -> Self {
        self.agent_defs = defs;
        self
    }

    /// M6.33 SUB4: wire a cancel token. The token is observed inside
    /// `collect_agent_turn_with_cancel` so a parent ctrl-C / `/cancel`
    /// short-circuits the subagent's stream instead of waiting for it
    /// to run to completion.
    pub fn with_cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }
}

#[async_trait]
impl Tool for SubAgentTool {
    fn name(&self) -> &'static str {
        TOOL_NAME
    }

    fn description(&self) -> &'static str {
        "Launch a sub-agent with its own history to handle a bounded subtask. \
         The sub-agent runs independently, may call tools (and spawn further \
         sub-agents up to the recursion limit), and returns its final response \
         as text. Use `agent` to pick a named agent definition from agents.json."
    }

    fn input_schema(&self) -> Value {
        let mut agent_names = self.agent_defs.names();
        agent_names.sort();
        json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short label for the sub-task (shown in logs)."
                },
                "prompt": {
                    "type": "string",
                    "description": "The full instruction for the sub-agent."
                },
                "agent": {
                    "type": "string",
                    "description": format!(
                        "Optional named agent from agents.json. Available: {}",
                        if agent_names.is_empty() { "none configured".to_string() }
                        else { agent_names.join(", ") }
                    )
                }
            },
            "required": ["prompt"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        if self.depth >= self.max_depth {
            return Err(Error::Agent(format!(
                "sub-agent recursion limit reached (depth {}/{})",
                self.depth, self.max_depth
            )));
        }

        let prompt = req_str(&input, "prompt")?.to_string();
        let agent_name = input.get("agent").and_then(Value::as_str);

        // Look up named agent definition if specified.
        let agent_def = agent_name.and_then(|name| self.agent_defs.get(name));
        if agent_name.is_some() && agent_def.is_none() {
            let available = self.agent_defs.names().join(", ");
            return Err(Error::Agent(format!(
                "unknown agent '{}'. Available: {}",
                agent_name.unwrap(),
                if available.is_empty() {
                    "none"
                } else {
                    &available
                }
            )));
        }

        let child_depth = self.depth + 1;
        let agent = self.factory.build(&prompt, agent_def, child_depth).await?;
        let stream = agent.run_turn(prompt);
        // M6.33 SUB4: collect_agent_turn_with_cancel observes the
        // parent's cancel token between stream events. Pre-fix the
        // subagent stream ran to completion regardless of ctrl-C.
        let outcome = collect_agent_turn_with_cancel(stream, self.cancel.clone()).await?;

        // dev-plan/32 Stage I: push this turn's Usage to the workflow
        // usage sink if one is active on this thread. No-op outside
        // `/workflow run` — model-driven Task calls and tests stay
        // unaffected.
        if let Some(u) = outcome.usage.as_ref() {
            crate::workflow::push_worker_usage(u.clone());
        }

        if outcome.text.is_empty() {
            Err(Error::Agent("sub-agent returned empty response".into()))
        } else {
            Ok(outcome.text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentEvent;
    use crate::error::Error;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use crate::tools::ToolRegistry;
    use async_trait::async_trait;
    use futures::stream;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct ScriptedProvider {
        scripts: Arc<Mutex<VecDeque<Vec<ProviderEvent>>>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<ProviderEvent>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(VecDeque::from(scripts))),
            })
        }
    }

    #[async_trait]
    impl Provider for ScriptedProvider {
        async fn stream(&self, _req: StreamRequest) -> Result<EventStream> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| Error::Provider("no more scripts".into()))?;
            let events: Vec<Result<ProviderEvent>> = script.into_iter().map(Ok).collect();
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn text_script(chunks: &[&str]) -> Vec<ProviderEvent> {
        let mut out = vec![ProviderEvent::MessageStart {
            model: "test".into(),
        }];
        for c in chunks {
            out.push(ProviderEvent::TextDelta((*c).to_string()));
        }
        out.push(ProviderEvent::ContentBlockStop);
        out.push(ProviderEvent::MessageStop {
            stop_reason: Some("end_turn".into()),
            usage: None,
        });
        out
    }

    struct SimpleFactory {
        scripts: Arc<Mutex<Vec<Vec<Vec<ProviderEvent>>>>>,
    }

    impl SimpleFactory {
        fn new(scripts: Vec<Vec<Vec<ProviderEvent>>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: Arc::new(Mutex::new(scripts)),
            })
        }
    }

    #[async_trait]
    impl AgentFactory for SimpleFactory {
        async fn build(
            &self,
            _prompt: &str,
            _def: Option<&AgentDef>,
            _depth: usize,
        ) -> Result<Agent> {
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| Error::Agent("factory exhausted".into()))?;
            let provider = ScriptedProvider::new(script);
            Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
        }
    }

    #[tokio::test]
    async fn sub_agent_returns_text() {
        let factory = SimpleFactory::new(vec![vec![text_script(&["done"])]]);
        let tool = SubAgentTool::new(factory);
        let out = tool.call(json!({"prompt": "go"})).await.unwrap();
        assert_eq!(out, "done");
    }

    #[tokio::test]
    async fn depth_limit_enforced() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory).with_depth(3).with_max_depth(3);
        let err = tool.call(json!({"prompt": "go"})).await.unwrap_err();
        assert!(format!("{err}").contains("recursion limit"));
    }

    #[tokio::test]
    async fn unknown_agent_errors() {
        let factory = SimpleFactory::new(vec![]);
        let tool = SubAgentTool::new(factory);
        let err = tool
            .call(json!({"prompt": "go", "agent": "nonexistent"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("unknown agent"));
    }

    struct EchoTool {
        name: &'static str,
    }
    #[async_trait]
    impl Tool for EchoTool {
        fn name(&self) -> &'static str {
            self.name
        }
        fn description(&self) -> &'static str {
            "echo"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _input: Value) -> Result<String> {
            Ok(String::new())
        }
    }

    struct StubProvider;
    #[async_trait]
    impl Provider for StubProvider {
        async fn stream(&self, _r: StreamRequest) -> Result<EventStream> {
            Ok(Box::pin(stream::iter(vec![Ok(
                ProviderEvent::MessageStart {
                    model: "test".into(),
                },
            )])))
        }
    }

    /// M6.33 SUB2: agent_def.disallowed_tools must be honored. Pre-fix
    /// the field was parsed but never applied — agent definitions
    /// claiming `disallowed_tools: Bash` got Bash anyway.
    #[tokio::test]
    async fn production_factory_applies_agent_def_disallowed_tools() {
        let mut base = ToolRegistry::new();
        base.register(Arc::new(EchoTool { name: "Bash" }));
        base.register(Arc::new(EchoTool { name: "Read" }));

        let factory = ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            snapshot: Arc::new(RwLock::new(FactorySnapshot {
                system: String::new(),
                tools: base,
            })),
            model: "test".into(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        };
        let def = AgentDef {
            name: "restricted".into(),
            disallowed_tools: vec!["Bash".into()],
            ..Default::default()
        };
        let child = factory.build("go", Some(&def), 1).await.unwrap();
        let names = child.tools.names();
        assert!(
            !names.contains(&"Bash"),
            "Bash should be removed by disallowed_tools, got {names:?}"
        );
        assert!(names.contains(&"Read"), "Read should remain, got {names:?}");
    }

    /// M6.33 SUB4: parent's cancel token propagates into the built
    /// child agent so retry-backoff sleeps + the streaming collector
    /// observe ctrl-C. Pre-fix the subagent ran to completion.
    #[tokio::test]
    async fn production_factory_propagates_cancel_token() {
        let cancel = CancelToken::new();
        let factory = ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            snapshot: Arc::new(RwLock::new(FactorySnapshot {
                system: String::new(),
                tools: ToolRegistry::new(),
            })),
            model: "test".into(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: Some(cancel.clone()),
            hooks: None,
        };
        let child = factory.build("go", None, 1).await.unwrap();
        cancel.cancel();
        assert!(
            child
                .cancel
                .as_ref()
                .map(|c| c.is_cancelled())
                .unwrap_or(false),
            "child agent should observe parent's cancel token"
        );
    }

    /// Regression: the factory must see the LIVE system prompt + tool
    /// registry — not a snapshot frozen at construction time. Pre-fix
    /// (everything before this commit) ProductionAgentFactory held
    /// `system: String` + `base_tools: ToolRegistry` as owned fields
    /// populated once at worker init. Mid-session `/mcp add` /
    /// `/skill install` / `/kms use` / `/reload-prompt` updated the
    /// PARENT agent's system + tool_registry but never reached the
    /// factory — subagents spawned post-mutator saw the startup-time
    /// snapshot, missing newly-attached MCP tools and stale on the
    /// `# MCP server instructions` / KMS / Memory sections.
    #[tokio::test]
    async fn production_factory_reads_live_snapshot() {
        let mut initial_tools = ToolRegistry::new();
        initial_tools.register(Arc::new(EchoTool { name: "OldTool" }));
        let snapshot = Arc::new(RwLock::new(FactorySnapshot {
            system: "INITIAL_SYSTEM".into(),
            tools: initial_tools,
        }));
        let factory = ProductionAgentFactory {
            provider: Arc::new(StubProvider),
            snapshot: snapshot.clone(),
            model: "test".into(),
            max_iterations: 1,
            max_depth: 3,
            max_tokens: 8192,
            agent_defs: AgentDefsConfig::default(),
            approver: Arc::new(crate::permissions::DenyApprover),
            permission_mode: PermissionMode::Auto,
            cancel: None,
            hooks: None,
        };

        // Build once with the initial snapshot — child sees OldTool
        // and the initial system.
        let child1 = factory.build("go", None, 1).await.unwrap();
        let names1 = child1.tools.names();
        assert!(
            names1.contains(&"OldTool"),
            "child should see initial tools, got {names1:?}"
        );
        assert!(
            child1.system_text().contains("INITIAL_SYSTEM"),
            "child should see initial system; got: {:?}",
            child1.system_text()
        );

        // Worker-side mutation: a `/mcp add` would do this — update
        // tool registry, refresh system prompt, then push both into
        // the shared snapshot.
        let mut updated_tools = ToolRegistry::new();
        updated_tools.register(Arc::new(EchoTool { name: "NewTool" }));
        {
            let mut snap = snapshot.write().unwrap();
            snap.system = "REFRESHED_SYSTEM".into();
            snap.tools = updated_tools;
        }

        // Build AGAIN with the same factory — the new child must see
        // the refreshed state, NOT the initial snapshot.
        let child2 = factory.build("go", None, 1).await.unwrap();
        let names2 = child2.tools.names();
        assert!(
            names2.contains(&"NewTool"),
            "child built after refresh must see new tool, got {names2:?}"
        );
        assert!(
            !names2.contains(&"OldTool"),
            "child built after refresh must NOT see old tool, got {names2:?}"
        );
        assert!(
            child2.system_text().contains("REFRESHED_SYSTEM"),
            "child built after refresh must see fresh system; got: {:?}",
            child2.system_text()
        );
        assert!(
            !child2.system_text().contains("INITIAL_SYSTEM"),
            "child built after refresh must NOT see stale system; got: {:?}",
            child2.system_text()
        );
    }

    #[tokio::test]
    async fn named_agent_passed_to_factory() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let saw_def = Arc::new(AtomicBool::new(false));
        let saw_def_clone = saw_def.clone();

        struct DefCheckFactory(Arc<AtomicBool>);
        #[async_trait]
        impl AgentFactory for DefCheckFactory {
            async fn build(&self, _p: &str, def: Option<&AgentDef>, _d: usize) -> Result<Agent> {
                if let Some(d) = def {
                    assert_eq!(d.name, "researcher");
                    self.0.store(true, Ordering::Relaxed);
                }
                let provider = ScriptedProvider::new(vec![text_script(&["found it"])]);
                Ok(Agent::new(provider, ToolRegistry::new(), "test", ""))
            }
        }

        let defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "researcher".into(),
                instructions: "Research things".into(),
                max_iterations: 5,
                ..Default::default()
            }],
        };

        let factory = Arc::new(DefCheckFactory(saw_def_clone));
        let tool = SubAgentTool::new(factory).with_agent_defs(defs);
        let out = tool
            .call(json!({"prompt": "find X", "agent": "researcher"}))
            .await
            .unwrap();
        assert_eq!(out, "found it");
        assert!(saw_def.load(Ordering::Relaxed));
    }
}
