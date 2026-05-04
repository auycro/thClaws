//! Transport-agnostic IPC dispatch — handles the JSON message protocol
//! the React frontend uses to talk to the Rust engine.
//!
//! Pre-M6.36 the dispatch lived as a 1600-LOC `match` block inside
//! `gui.rs::run`'s `with_ipc_handler` closure, capturing wry-specific
//! handles (`EventLoopProxy<UserEvent>`, the wry webview, etc.). That
//! prevented sharing the dispatch with the new `--serve` (Axum + WS)
//! transport.
//!
//! M6.36 SERVE1 promotes the dispatch into [`handle_ipc`] which takes
//! an [`IpcContext`] carrying the transport-agnostic primitives:
//!
//! - [`IpcContext::shared`] — `SharedSessionHandle` (input_tx / events_tx)
//! - [`IpcContext::approver`] — `GuiApprover` so `approval_response`
//!   can resolve pending oneshots regardless of transport
//! - [`IpcContext::pending_asks`] — same for `ask_user_response`
//! - [`IpcContext::dispatch`] — closure that pushes a JSON payload to
//!   the frontend (wry: `webview.evaluate_script("__thclaws_dispatch(...)")`;
//!   web: `ws.send(Message::Text(payload))`)
//! - [`IpcContext::on_quit`] / `on_send_initial_state` / `on_zoom` —
//!   transport-specific bridges for the few non-payload events.
//!
//! Both `gui.rs` (wry) and `server.rs` (Axum/WS — to be added in SERVE2)
//! build their own `IpcContext` flavor and call [`handle_ipc`] uniformly.
//! The body of [`handle_ipc`] is identical regardless of transport.

use crate::permissions::GuiApprover;
use crate::shared_session::{SharedSessionHandle, ShellInput};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Pending `AskUserQuestion` responders, keyed by request id. The IPC
/// handler's `ask_user_response` arm pulls the matching oneshot and
/// completes it with the user's text. Same shape as the Mutex<HashMap>
/// `gui.rs::run` constructs around the `set_gui_ask_sender` plumbing.
pub type PendingAsks = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<String>>>>;

/// Closure that pushes a JSON payload to the frontend. Wry calls
/// `webview.evaluate_script("window.__thclaws_dispatch('<payload>')")`;
/// the future WS layer calls `ws.send(Message::Text(payload))`. The
/// payload is already a complete JSON message — the dispatch is just
/// the transport.
pub type DispatchFn = Arc<dyn Fn(String) + Send + Sync>;

/// Transport-specific bridge fired when the frontend requests a quit
/// (`{"type": "app_close"}`). Wry sets `ControlFlow::Exit`; the WS
/// layer drops the connection / shuts down the server.
pub type QuitFn = Arc<dyn Fn() + Send + Sync>;

/// Transport-specific bridge fired when the frontend signals it's
/// ready (`{"type": "frontend_ready"}`). Triggers the heavyweight
/// initial-state build (provider + model + KMS list + recent sessions
/// + …) and pushes it to the frontend. Wry's impl synthesizes the
/// JSON inline in the event-loop arm; the WS layer's impl will send a
/// snapshot frame.
pub type SendInitialStateFn = Arc<dyn Fn() + Send + Sync>;

/// Transport-specific bridge fired when the frontend persists a new
/// `guiScale` value (`{"type": "gui_set_zoom"}`). Wry calls
/// `webview.zoom(scale)`; the WS layer forwards the scale to the
/// client (the browser's CSS zoom handles the rest).
pub type ZoomFn = Arc<dyn Fn(f64) + Send + Sync>;

/// Everything the IPC dispatch needs from its surrounding transport.
/// Construct one per session in the transport's setup; pass `&` to
/// [`handle_ipc`] for each inbound message.
#[derive(Clone)]
pub struct IpcContext {
    pub shared: Arc<SharedSessionHandle>,
    pub approver: Arc<GuiApprover>,
    pub pending_asks: PendingAsks,
    pub dispatch: DispatchFn,
    pub on_quit: QuitFn,
    pub on_send_initial_state: SendInitialStateFn,
    pub on_zoom: ZoomFn,
}

/// Dispatch a single inbound IPC message. Routes by `msg.type` to one
/// of ~70 message-type arms (see the body for the full inventory).
/// Unknown types fall through silently — the transport already accepted
/// the message; an unknown `type` is the model / frontend's problem,
/// not the dispatch's.
///
/// **Currently a stub** — the body migration from `gui.rs` is staged
/// in subsequent commits. As of SERVE1 commit 1, only the trivial
/// `app_close` arm is wired so the IpcContext design can be exercised
/// end-to-end. The remaining 70+ arms get migrated incrementally with
/// no behavior change at each step.
pub fn handle_ipc(msg: Value, ctx: &IpcContext) {
    let ty = msg.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "app_close" => {
            (ctx.on_quit)();
        }

        // M6.36 SERVE3: minimum-viable WS dispatch surface — just
        // enough that a browser can send a message and observe events
        // come back. Wry continues handling its own (full) dispatch
        // table until the rest is migrated incrementally per SERVE9.
        "shell_input" | "chat_prompt" | "pty_write" => {
            // Plain-text path. (Image-attachment handling stays in
            // gui.rs's closure for now — the migration will pull it
            // over with the rest of the rich-input handlers.)
            let line = msg
                .get("text")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_default();
            let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
            if !trimmed.is_empty() {
                let _ = ctx.shared.input_tx.send(ShellInput::Line(trimmed));
            }
        }

        "frontend_ready" => {
            // Wry: just signal the ready_gate (idempotent).
            // WS: also fire on_send_initial_state so the frontend gets
            // its initial snapshot. The wry path's send_event arm
            // synthesises the same JSON via gui.rs's event-loop.
            ctx.shared.ready_gate.signal();
            (ctx.on_send_initial_state)();
        }

        "approval_response" => {
            let id = msg.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
            let decision_str = msg
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("deny");
            let decision = match decision_str {
                "allow" => crate::permissions::ApprovalDecision::Allow,
                "allow_for_session" => crate::permissions::ApprovalDecision::AllowForSession,
                _ => crate::permissions::ApprovalDecision::Deny,
            };
            ctx.approver.resolve(id, decision);
        }

        "shell_cancel" => {
            // Worker observes ctrl-C / cancel via the cancel token.
            ctx.shared.request_cancel();
        }

        "new_session" => {
            let _ = ctx.shared.input_tx.send(ShellInput::NewSession);
        }

        // SERVE9 staged migration: the rest of the dispatch table
        // continues to live in `gui.rs::with_ipc_handler` for now.
        // Each subsequent migration is incremental — `cargo test` is
        // the regression backstop.
        _ => {
            // No-op — the wry transport's existing closure handles
            // every other `ty` directly.
        }
    }
    // Suppress unused-field warnings while the migration is in-flight.
    let _ = (&ctx.pending_asks, &ctx.dispatch, &ctx.on_zoom, &msg);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// IpcContext can be constructed with stub closures for tests.
    /// Pin the type signature so future refactors that break Send +
    /// Sync surface in CI rather than in production.
    #[test]
    fn ipc_context_is_constructible_with_noop_transport() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let dispatch: DispatchFn = Arc::new(|_payload: String| {});
        let quit_fired = Arc::new(AtomicBool::new(false));
        let quit_fired_clone = quit_fired.clone();
        let on_quit: QuitFn = Arc::new(move || {
            quit_fired_clone.store(true, Ordering::SeqCst);
        });
        let on_send_initial_state: SendInitialStateFn = Arc::new(|| {});
        let on_zoom: ZoomFn = Arc::new(|_scale: f64| {});

        let ctx = IpcContext {
            shared,
            approver,
            pending_asks,
            dispatch,
            on_quit,
            on_send_initial_state,
            on_zoom,
        };

        // Exercise the only currently-wired arm.
        handle_ipc(serde_json::json!({"type": "app_close"}), &ctx);
        assert!(
            quit_fired.load(Ordering::SeqCst),
            "app_close should fire on_quit"
        );
    }

    #[test]
    fn handle_ipc_ignores_unknown_type() {
        let shared = Arc::new(crate::shared_session::spawn());
        let (approver, _rx) = crate::permissions::GuiApprover::new();
        let pending_asks: PendingAsks = Arc::new(Mutex::new(HashMap::new()));
        let ctx = IpcContext {
            shared,
            approver,
            pending_asks,
            dispatch: Arc::new(|_| {}),
            on_quit: Arc::new(|| {}),
            on_send_initial_state: Arc::new(|| {}),
            on_zoom: Arc::new(|_| {}),
        };
        // Should not panic.
        handle_ipc(serde_json::json!({"type": "nonexistent_type"}), &ctx);
        handle_ipc(serde_json::json!({}), &ctx);
        handle_ipc(serde_json::json!({"type": 42}), &ctx);
    }
}
