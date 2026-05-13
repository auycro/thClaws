//! Bridge between the LINE WS client and the agent loop.
//!
//! `LineSession` is what `gui.rs` / `repl.rs` spawn when a binding
//! token is present. It owns:
//! - A `LineClient` for WS + reply API
//! - A shared `Agent` to run turns against
//! - The current `Session` (shared with the rest of thClaws so
//!   LINE-driven turns appear in the user's normal chat history)
//!
//! Phase 1.1 scope: simplest possible relay — when a
//! `UserMessage` envelope arrives, push it as a user turn into
//! the shared session via the `LineMessageHandler` trait. The
//! caller controls the agent / permission posture (which is why
//! we don't ship a full implementation here yet — `gui.rs` and
//! `repl.rs` will provide concrete impls in Phase 1.2/1.3).
//!
//! Phase 1.2 will add a built-in `ToolGate` that suspends turns
//! on mutating tool calls and round-trips a Quick Reply.

use std::sync::Arc;

use async_trait::async_trait;

use super::approver::{ApprovalReply, LineApprover};
use super::client::{LineClient, LineClientError, LineEnvelopeSink};
use super::config::LineConfig;
use super::filter::filter_for_line;
use super::protocol::WsEnvelope;

/// Pluggable handler — what to do when a LINE user message
/// arrives. Implementations live in `gui.rs` (drives the shared
/// session + GUI broadcasts) and `repl.rs` (drives the standalone
/// LINE-only agent loop).
#[async_trait]
pub trait LineMessageHandler: Send + Sync + 'static {
    /// Called once per inbound user text. Implementer drives the
    /// agent and returns the final assistant text. `None` skips
    /// the LINE reply (e.g. recognised a `/help` command and
    /// handled it inline).
    async fn handle_message(&self, text: String) -> Option<String>;

    /// Called for Quick-Reply postbacks (Phase 1.2 permission
    /// gate). Default no-op so Phase 1.1 implementations don't
    /// have to override.
    async fn handle_postback(&self, _data: String) {}
}

pub struct LineSession {
    client: Arc<LineClient>,
    handler: Arc<dyn LineMessageHandler>,
    /// When `Some`, inbound text + postbacks are routed to the
    /// approver first; an approval reply short-circuits the agent
    /// turn. `None` when the worker isn't running in
    /// `PermissionMode::LineGated` — the session falls back to
    /// the plain handler-only flow used for Phase 1.1 smoke
    /// testing.
    approver: Option<Arc<LineApprover>>,
}

impl LineSession {
    pub fn new(config: LineConfig, handler: Arc<dyn LineMessageHandler>) -> Self {
        Self {
            client: Arc::new(LineClient::new(config)),
            handler,
            approver: None,
        }
    }

    /// Attach a `LineApprover` so inbound text / postbacks can
    /// resolve pending tool-approval prompts.
    pub fn with_approver(mut self, approver: Arc<LineApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn with_cancel(mut self, token: crate::cancel::CancelToken) -> Self {
        // Replace the Arc'd client with one carrying the cancel
        // token. Cheap — only one client per session.
        let client = Arc::try_unwrap(self.client)
            .map(|c| c.with_cancel(token.clone()))
            .unwrap_or_else(|arc| {
                let cfg = LineConfig {
                    binding_token: String::new(),
                    server_url: None,
                };
                // Should never hit this branch — `new` is the
                // only constructor — but fail-safe rather than
                // unwrap-panic.
                let _ = arc;
                LineClient::new(cfg).with_cancel(token)
            });
        self.client = Arc::new(client);
        self
    }

    /// Drive the WS loop forever. Returns only on cancellation or
    /// a permanent error (rare — reconnect handles transient).
    pub async fn run(self: Arc<Self>) -> Result<(), LineClientError> {
        let sink = SessionSink {
            client: self.client.clone(),
            handler: self.handler.clone(),
            approver: self.approver.clone(),
        };
        self.client.run(sink).await
    }
}

struct SessionSink {
    client: Arc<LineClient>,
    handler: Arc<dyn LineMessageHandler>,
    approver: Option<Arc<LineApprover>>,
}

#[async_trait]
impl LineEnvelopeSink for SessionSink {
    async fn on_envelope(&self, envelope: WsEnvelope) {
        match envelope {
            WsEnvelope::UserMessage {
                text, request_id, ..
            } => {
                eprintln!(
                    "[line] user message ({} chars, request_id={})",
                    text.chars().count(),
                    request_id
                );
                // If an approval is waiting on a reply, the next
                // inbound text is interpreted as that answer
                // *first* — don't drop the message into a new
                // agent turn while a tool dispatch is suspended
                // on `oneshot::Receiver`. An `Unrecognised` reply
                // re-prompts via the relay; anything that parses
                // resolves the pending decision and skips the
                // agent loop for this turn.
                if let Some(approver) = &self.approver {
                    if approver.has_pending() {
                        match approver.record_decision_from_text(&text) {
                            Some(ApprovalReply::Allow) => {
                                let _ = self
                                    .client
                                    .send_reply(&request_id, "✅ Approved — running tool now.")
                                    .await;
                                return;
                            }
                            Some(ApprovalReply::Deny) => {
                                let _ = self
                                    .client
                                    .send_reply(
                                        &request_id,
                                        "🚫 Denied — agent will not run the tool.",
                                    )
                                    .await;
                                return;
                            }
                            Some(ApprovalReply::Unrecognised) => {
                                let _ = self
                                    .client
                                    .send_reply(
                                        &request_id,
                                        "I didn't catch that. Please reply 'approve' or 'deny'.",
                                    )
                                    .await;
                                return;
                            }
                            None => {
                                // Race — pending was cleared
                                // between has_pending() and the
                                // resolve attempt. Fall through
                                // to the normal handler path.
                            }
                        }
                    }
                }

                if let Some(reply) = self.handler.handle_message(text).await {
                    let body = filter_for_line(&reply);
                    if let Err(e) = self.client.send_reply(&request_id, body).await {
                        eprintln!("[line] reply failed (request_id={}): {}", request_id, e);
                    }
                }
            }
            WsEnvelope::Postback { data } => {
                eprintln!("[line] postback: {data}");
                // Phase 1.2.b path — once the relay forwards Quick
                // Reply taps as `tool:<verb>:<id>` postbacks, this
                // resolves the matching pending approval *before*
                // the generic handler sees the postback.
                if let Some(approver) = &self.approver {
                    if approver.record_decision_from_postback(&data).is_some() {
                        return;
                    }
                }
                self.handler.handle_postback(data).await;
            }
            WsEnvelope::Notice { text } => {
                // Surface as a regular eprintln — Phase 1.3 GUI
                // will also drop a side-bubble on Notice.
                eprintln!("[line] notice: {text}");
            }
        }
    }
}
