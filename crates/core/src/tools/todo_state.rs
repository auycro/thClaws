//! Todo state broadcaster — sidebar plumbing for `TodoWrite`.
//!
//! Mirrors [`crate::tools::plan_state`]'s broadcaster pattern, but for
//! the casual scratchpad checklist instead of structured plan mode.
//! TodoWrite calls [`fire`] with the freshly-written list after a
//! successful disk write; the GUI worker registers a [`set_broadcaster`]
//! closure that turns each fire into a `ViewEvent::TodoUpdate`, which
//! the frontend renders as a live `TodoSidebar` panel.
//!
//! Lower complexity than `plan_state` because TodoWrite has no
//! sequential gate, no failure semantics, no stalled detector. It's a
//! flat list mutation, broadcast as a snapshot. The CLI ignores the
//! broadcast (no broadcaster registered when running in REPL).

use super::todo::TodoItem;
use std::sync::{Mutex, OnceLock};

type Broadcaster = Box<dyn Fn(Vec<TodoItem>) + Send + Sync>;

fn broadcaster() -> &'static Mutex<Option<Broadcaster>> {
    static B: OnceLock<Mutex<Option<Broadcaster>>> = OnceLock::new();
    B.get_or_init(|| Mutex::new(None))
}

/// Register a broadcaster invoked after every successful TodoWrite.
/// Replaces any prior registration — there's only one active GUI
/// session worker at a time. Pass a closure that captures the
/// `events_tx` you want todo deltas to land on.
pub fn set_broadcaster<F>(f: F)
where
    F: Fn(Vec<TodoItem>) + Send + Sync + 'static,
{
    if let Ok(mut g) = broadcaster().lock() {
        *g = Some(Box::new(f));
    }
}

/// Called by `TodoWriteTool::call` after the markdown file has been
/// written successfully. Recovers from mutex poisoning (matches the
/// `plan_state::fire` posture — a panic in another thread holding the
/// lock would otherwise silently disable every subsequent broadcast).
pub fn fire(todos: Vec<TodoItem>) {
    let g = broadcaster().lock().unwrap_or_else(|p| p.into_inner());
    if let Some(f) = g.as_ref() {
        f(todos);
    }
}
