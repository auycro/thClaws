#![cfg(feature = "gui")]
//! User-driven side-channel agents — `/agent <name> <prompt>`.
//!
//! Spawns a fresh `Agent` instance on its own tokio task, **independent
//! of the main agent**. Differences from the LLM-driven `Task` tool
//! (`crate::subagent`):
//!
//! | | side-channel (this) | subagent (`Task` tool) |
//! |---|---|---|
//! | Trigger | User types `/agent name prompt` | Model calls `Task` tool |
//! | Concurrency | Runs concurrently with main agent | Blocks main's turn |
//! | Main's history | Not affected — main doesn't see prompt or result | Tool result lands in main's history |
//! | Cancel | Independent CancelToken — main's Cmd-C does NOT kill it | Inherits parent's cancel (main Cmd-C kills child) |
//! | Surface | `chat_side_channel_*` events on the chat tab | Single `Task` tool indicator |
//!
//! Per-spawn lifecycle:
//!
//! 1. Resolve `agent_name` → `AgentDef` from the loaded registry. Errors
//!    early if the name isn't known.
//! 2. Generate a stable `SideChannelId` (`side-<8 hex>`).
//! 3. Build a child `Agent` via the supplied factory, with `origin =
//!    SideChannel { id, agent_name }` so every approval request the
//!    child fires is tagged for the GUI.
//! 4. Wire an independent `CancelToken` (NOT a child of main's) — main
//!    Cmd-C doesn't reach the side channel. The token is stashed in the
//!    `SideChannelRegistry` so `/agent cancel <id>` can fire it later.
//! 5. Spawn a tokio task that drives `agent.run_turn(prompt)`, fans
//!    every `AgentEvent` out as `ViewEvent::SideChannel*`, and emits
//!    `SideChannelDone` on natural stop or `SideChannelError` on
//!    cancel / failure. The task removes the channel from the registry
//!    on exit.
//!
//! The registry is a process-level Mutex<HashMap<id, Handle>> exposed
//! via `registry()` so the slash-command dispatch (`/agents`, `/agent
//! cancel`) can introspect / mutate without each surface threading the
//! handle through.

use crate::agent::AgentEvent;
use crate::cancel::CancelToken;
use crate::error::{Error, Result};
use crate::permissions::AgentOrigin;
use crate::shared_session::ViewEvent;
use crate::subagent::AgentFactory;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;
use tokio::sync::broadcast;

/// Stable handle for a running side-channel agent. Used by
/// `/agent cancel <id>` and `/agents` list.
pub type SideChannelId = String;

/// Per-channel state held in the registry. The cancel token is the
/// same instance handed to the spawned `Agent`; firing `cancel.cancel()`
/// from `/agent cancel` propagates into the agent's retry sleeps and
/// the `collect_agent_turn_with_cancel` select gate.
pub struct SideChannelHandle {
    pub agent_name: String,
    pub started_at: Instant,
    pub cancel: CancelToken,
    /// JoinHandle so callers (tests, future shutdown logic) can await
    /// the spawn task. Wrapped in Mutex<Option<>> because `JoinHandle`
    /// is `!Clone` and the registry is shared — we hand it out by
    /// `take()`-ing it once.
    pub join: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

/// Process-wide registry of active side channels. Singleton — same
/// instance shared across surfaces (CLI REPL, GUI Chat dispatch). The
/// slash-command handlers consult this for `/agents` listing and
/// `/agent cancel <id>`. Entries are removed by the spawn task itself
/// when the channel exits (whether by success, error, or cancel).
pub fn registry() -> &'static Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>> {
    static REG: OnceLock<Arc<Mutex<HashMap<SideChannelId, SideChannelHandle>>>> = OnceLock::new();
    REG.get_or_init(|| Arc::new(Mutex::new(HashMap::new())))
}

/// Spawn a new side channel. Returns the assigned id immediately;
/// the actual agent work happens on a tokio task that streams
/// `ViewEvent::SideChannel*` events through `events_tx`.
///
/// Errors:
/// - `agent_name` not found in `factory`'s known agent_defs (caller
///   should have validated, but defensive double-check)
/// - factory.build returns an error (provider issue, etc.)
///
/// The function does NOT block on the agent — it returns once the
/// task is spawned. Use `registry()` to track state, or subscribe
/// to `events_tx` for streaming updates.
pub async fn spawn_side_channel(
    agent_name: String,
    prompt: String,
    factory: Arc<dyn AgentFactory>,
    agent_defs: crate::agent_defs::AgentDefsConfig,
    events_tx: broadcast::Sender<ViewEvent>,
) -> Result<SideChannelId> {
    let agent_def = agent_defs
        .agents
        .iter()
        .find(|d| d.name == agent_name)
        .cloned()
        .ok_or_else(|| {
            Error::Tool(format!(
                "unknown agent '{agent_name}' — try /agents to list known agents"
            ))
        })?;

    let id: SideChannelId = format!(
        "side-{}",
        uuid::Uuid::new_v4()
            .to_string()
            .split('-')
            .next()
            .unwrap_or("anon")
            .to_string()
    );

    // Independent cancel — main's Cmd-C does NOT kill this. User
    // cancels via `/agent cancel <id>`.
    let cancel = CancelToken::new();

    // Build the child agent. The factory currently sets origin =
    // Main (or whatever its parent's origin is); we override here so
    // the side channel's permission requests are tagged correctly.
    let mut agent = factory.build(&prompt, Some(&agent_def), 0).await?;
    agent = agent
        .with_origin(AgentOrigin::SideChannel {
            id: id.clone(),
            agent_name: agent_name.clone(),
        })
        .with_cancel(cancel.clone());

    let started_at = Instant::now();

    // Emit start event before spawning so the UI sees the indicator
    // immediately, not after the task scheduler decides to run.
    let _ = events_tx.send(ViewEvent::SideChannelStart {
        id: id.clone(),
        agent_name: agent_name.clone(),
    });

    let id_for_task = id.clone();
    let agent_name_for_task = agent_name.clone();
    let events_tx_for_task = events_tx.clone();
    let cancel_for_task = cancel.clone();

    let join = tokio::spawn(async move {
        // Stream events as the child runs, forwarding to chat surface.
        let stream = agent.run_turn(prompt);
        let mut stream = Box::pin(stream);
        let mut full_text = String::new();
        let mut errored: Option<String> = None;

        loop {
            let next = tokio::select! {
                ev = futures::StreamExt::next(&mut stream) => ev,
                _ = cancel_for_task.cancelled() => {
                    errored = Some("cancelled".into());
                    break;
                }
            };
            let Some(ev) = next else { break };
            match ev {
                Ok(AgentEvent::Text(s)) => {
                    full_text.push_str(&s);
                    let _ = events_tx_for_task.send(ViewEvent::SideChannelTextDelta {
                        id: id_for_task.clone(),
                        text: s,
                    });
                }
                Ok(AgentEvent::ToolCallStart { name, .. }) => {
                    let _ = events_tx_for_task.send(ViewEvent::SideChannelToolCall {
                        id: id_for_task.clone(),
                        tool_name: name.clone(),
                        label: name,
                    });
                }
                Ok(AgentEvent::Done { .. }) => break,
                Ok(_) => {}
                Err(e) => {
                    errored = Some(format!("{e}"));
                    break;
                }
            }
        }

        let duration_ms = started_at.elapsed().as_millis() as u64;
        match errored {
            Some(error) => {
                let _ = events_tx_for_task.send(ViewEvent::SideChannelError {
                    id: id_for_task.clone(),
                    error,
                });
            }
            None => {
                let _ = events_tx_for_task.send(ViewEvent::SideChannelDone {
                    id: id_for_task.clone(),
                    agent_name: agent_name_for_task,
                    duration_ms,
                    result_text: full_text,
                });
            }
        }

        // Always remove from registry on exit, regardless of outcome.
        if let Ok(mut reg) = registry().lock() {
            reg.remove(&id_for_task);
        }
    });

    if let Ok(mut reg) = registry().lock() {
        reg.insert(
            id.clone(),
            SideChannelHandle {
                agent_name,
                started_at,
                cancel,
                join: Mutex::new(Some(join)),
            },
        );
    }

    Ok(id)
}

/// Cancel a running side channel by id. Returns `true` if the channel
/// was found and signalled, `false` if no such id is active. The
/// channel exits asynchronously after this returns — the cancel
/// token wakes the agent's `cancelled()` await and the spawn task
/// emits `SideChannelError { error: "cancelled" }` before removing
/// itself from the registry.
pub fn cancel_side_channel(id: &str) -> bool {
    if let Ok(reg) = registry().lock() {
        if let Some(handle) = reg.get(id) {
            handle.cancel.cancel();
            return true;
        }
    }
    false
}

/// Snapshot of all active side channels for `/agents` listing.
/// Returns (id, agent_name, elapsed_seconds) tuples — kept simple to
/// avoid exposing the JoinHandle / CancelToken to callers.
pub fn list_side_channels() -> Vec<(String, String, f64)> {
    let Ok(reg) = registry().lock() else {
        return Vec::new();
    };
    reg.iter()
        .map(|(id, h)| {
            (
                id.clone(),
                h.agent_name.clone(),
                h.started_at.elapsed().as_secs_f64(),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Agent;
    use crate::agent_defs::AgentDef;
    use crate::error::Result as CrateResult;
    use crate::permissions::AutoApprover;
    use crate::providers::{EventStream, Provider, ProviderEvent, StreamRequest};
    use async_trait::async_trait;
    use futures::stream;

    /// Tiny in-test provider that emits a fixed text + MessageStop.
    /// Mirrors the pattern used by `agent.rs::tests::ScriptedProvider`
    /// without re-exporting it.
    struct InlineProvider {
        text: String,
    }

    #[async_trait]
    impl Provider for InlineProvider {
        async fn stream(&self, _req: StreamRequest) -> CrateResult<EventStream> {
            let events = vec![
                Ok(ProviderEvent::MessageStart {
                    model: "stub".into(),
                }),
                Ok(ProviderEvent::TextDelta(self.text.clone())),
                Ok(ProviderEvent::ContentBlockStop),
                Ok(ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                }),
            ];
            Ok(Box::pin(stream::iter(events)))
        }
    }

    /// Test factory that builds a tiny Agent backed by InlineProvider.
    struct StubFactory {
        text: String,
    }

    #[async_trait]
    impl AgentFactory for StubFactory {
        async fn build(
            &self,
            _prompt: &str,
            _agent_def: Option<&AgentDef>,
            _child_depth: usize,
        ) -> Result<Agent> {
            let provider = Arc::new(InlineProvider {
                text: self.text.clone(),
            });
            Ok(Agent::new(
                provider,
                crate::tools::ToolRegistry::new(),
                "stub",
                "system",
            )
            .with_approver(Arc::new(AutoApprover))
            .with_max_iterations(2))
        }
    }

    #[tokio::test]
    async fn spawn_emits_start_text_done_events() {
        // Before each test clear any leftover state from other tests
        // sharing the same process-level registry.
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }

        let (events_tx, mut events_rx) = broadcast::channel(64);
        let factory = Arc::new(StubFactory {
            text: "hello world".into(),
        });
        let agent_defs = crate::agent_defs::AgentDefsConfig {
            agents: vec![AgentDef {
                name: "translator".into(),
                max_iterations: 1,
                ..AgentDef::default()
            }],
        };
        let id = spawn_side_channel(
            "translator".into(),
            "test".into(),
            factory,
            agent_defs,
            events_tx,
        )
        .await
        .expect("spawn ok");
        assert!(id.starts_with("side-"));

        // Drain events until Done. 2-second timeout safety.
        let mut saw_start = false;
        let mut saw_text = false;
        let mut saw_done = false;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            let res =
                tokio::time::timeout(std::time::Duration::from_millis(200), events_rx.recv()).await;
            match res {
                Ok(Ok(ViewEvent::SideChannelStart { id: i, .. })) if i == id => {
                    saw_start = true;
                }
                Ok(Ok(ViewEvent::SideChannelTextDelta { id: i, text }))
                    if i == id && text.contains("hello") =>
                {
                    saw_text = true;
                }
                Ok(Ok(ViewEvent::SideChannelDone {
                    id: i, result_text, ..
                })) if i == id => {
                    saw_done = true;
                    assert!(result_text.contains("hello"));
                    break;
                }
                Ok(Ok(_)) => {}
                Ok(Err(_)) | Err(_) => continue,
            }
        }
        assert!(saw_start, "missing SideChannelStart");
        assert!(saw_text, "missing SideChannelTextDelta");
        assert!(saw_done, "missing SideChannelDone");

        // Registry should be empty after the channel exits.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let reg = registry().lock().unwrap();
        assert!(
            !reg.contains_key(&id),
            "channel should be removed from registry on exit"
        );
    }

    #[tokio::test]
    async fn spawn_unknown_agent_errors() {
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        let (events_tx, _rx) = broadcast::channel(8);
        let factory = Arc::new(StubFactory { text: "x".into() });
        let agent_defs = crate::agent_defs::AgentDefsConfig { agents: vec![] };
        let result = spawn_side_channel(
            "nonexistent".into(),
            "test".into(),
            factory,
            agent_defs,
            events_tx,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("unknown agent"), "got: {err}");
    }

    #[test]
    fn list_returns_active_channels() {
        // No async needed — list/cancel are sync.
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        assert!(list_side_channels().is_empty());

        // Insert a fake handle directly to test the snapshot shape.
        let cancel = CancelToken::new();
        let _join: tokio::task::JoinHandle<()> = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .spawn(async {});
        // Construct a join we can stash without running an async ctx.
        // Easier: just stash None.
        if let Ok(mut reg) = registry().lock() {
            reg.insert(
                "side-test".into(),
                SideChannelHandle {
                    agent_name: "translator".into(),
                    started_at: Instant::now(),
                    cancel,
                    join: Mutex::new(None),
                },
            );
        }

        let snapshot = list_side_channels();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].0, "side-test");
        assert_eq!(snapshot[0].1, "translator");

        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
    }

    #[test]
    fn cancel_returns_false_for_unknown() {
        if let Ok(mut r) = registry().lock() {
            r.clear();
        }
        assert!(!cancel_side_channel("does-not-exist"));
    }
}
