//! Dynamic Workflows — code-driven subagent fan-out (dev-plan/32).
//!
//! Fourth orchestration tier alongside the model-driven `Task` tool,
//! user-driven `/agent` side-channels, and multi-process Agent Teams.
//! Claude *authors* a JavaScript orchestration script from a user
//! prompt; Boa *executes* the script deterministically; workers run as
//! stateless subagents with fresh context.
//!
//! Stage A scope (this milestone):
//! - [`runtime`] — `WorkflowSandbox`: Boa context, `thclaws.*` host
//!   bindings (stub `subagent`), `eval` / `Function` stripped.
//!
//! Later stages (see dev-plan/32):
//! - Stage B — `/workflow run` slash command + author phase + script
//!   review panel.
//! - Stage C — real subagent spawn through the `Task` primitive,
//!   tokio-semaphore concurrency cap.
//! - Stage D — `state.jsonl` checkpoint after each top-level
//!   statement; `--resume` is Tier 2.
//! - Stage E — REPL worker grid + `ch25-workflows.md`.

pub mod approval;
pub mod headless;
mod inspect;
mod runtime;
mod script;
mod state;

pub use approval::{parse_chat_decision, WorkflowApprover, WorkflowDecision};

/// Heuristic: is this provider/tool error string a *transient* failure
/// worth retrying (a dropped SSE stream, network blip, upstream
/// overload) versus a deterministic one (auth, bad request, schema
/// mismatch, empty output) that would just fail again?
///
/// Used by the workflow author phase and the subagent retry loop so a
/// single flaky stream during a parallel fan-out — e.g. the
/// `stream: error decoding response body` we saw kill a whole
/// WorkflowRun — doesn't abort the run. Deliberately conservative:
/// matches transient signatures only, so deterministic failures still
/// fail fast.
pub(crate) fn is_transient_provider_error(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    const TRANSIENT: &[&str] = &[
        "error decoding response body",
        "error sending request",
        "stream error",
        "stream closed",
        "stream: ", // provider stream wrapper, e.g. "stream: <io error>"
        "connection reset",
        "connection closed",
        "connection refused",
        "broken pipe",
        "unexpected eof",
        "incomplete message",
        "timed out",
        "timeout",
        "overloaded",
        "temporarily unavailable",
        "service unavailable",
        "too many requests",
        "rate limit",
        "429",
        "502",
        "503",
        "504",
        "529",
    ];
    TRANSIENT.iter().any(|s| m.contains(s))
}

pub(crate) use inspect::{
    delete_workflow, list_workflows, read_completed_workers, read_events, read_workflow_script,
    resolve_id_prefix, write_workflow_script,
};
pub use runtime::check_kms_write_capability;
pub(crate) use runtime::{
    is_inside_workflow, push_worker_usage, replay_remaining, set_include_base, set_replay_cache,
    set_task_tool, set_usage_sink, take_all_usages, WorkflowSandbox,
};

#[cfg(feature = "gui")]
pub(crate) use runtime::{set_cancel, set_events_tx, WORKFLOW_CANCELLED_MSG};
pub(crate) use script::author;
pub(crate) use state::{generate_workflow_id, set_logger, LoggerHandle, WorkflowLogger};
