//! Bridge between the Telegram polling client and the agent loop.
//!
//! [`TelegramSession`] is what the worker spawns once a bot token is
//! configured. It owns a [`TelegramClient`] and routes each polled
//! [`Update`] through:
//!
//! 1. **callback_query** → resolve a pending tool approval
//!    ([`TelegramApprover`]), `answerCallbackQuery`, edit the prompt.
//! 2. **message** → authorize (DM allowlist / pairing; group allowlist),
//!    then either run the pairing flow or forward the text to the
//!    [`TelegramMessageHandler`] and ship the reply back (HTML-escaped +
//!    chunked).
//!
//! Tier 1 model (decision #7): all *authorized* chats forward into the
//! single shared worker session — there is no per-chat agent isolation
//! yet (that's Tier 2 forum-topic routing). [`ChatRegistry`] tracks
//! per-`chat_id` liveness for the status pill and 24h idle GC, and the
//! pairing flow gates *who* may reach the agent, so in practice a Tier 1
//! deployment is single-owner. Concurrency note: `active_chat` on the
//! approver is last-writer-wins, which is correct for the single-owner
//! case and revisited when per-chat sessions land.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::approver::{ApprovalReply, TelegramApprover};
use super::client::{TelegramClient, TelegramClientError, TelegramUpdateSink};
use super::config::{DmPolicy, TelegramConfig};
use super::filter::format_for_telegram;
use super::pairing::PairingManager;
use super::protocol::{
    AnswerCallbackQuery, CallbackQuery, Chat, ChatKind, EditMessageText, Message, Update,
};

/// Idle window after which a chat's registry entry is GC'd (decision #7).
pub const IDLE_GC: Duration = Duration::from_secs(24 * 60 * 60);

/// What to do when an authorized Telegram user sends text. The worker
/// provides the concrete impl (forwards into the shared session).
#[async_trait]
pub trait TelegramMessageHandler: Send + Sync + 'static {
    /// Run one inbound text as an agent turn; return the final assistant
    /// text. `None` skips the Telegram reply.
    async fn handle_message(&self, text: String) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct ChatState {
    pub kind: ChatKind,
    pub last_active: Instant,
    pub message_count: u64,
}

/// `chat_id → ChatState` map with idle GC. Drives the GUI status pill's
/// "active chats / messages" counts.
pub struct ChatRegistry {
    chats: Mutex<HashMap<i64, ChatState>>,
    idle: Duration,
}

impl Default for ChatRegistry {
    fn default() -> Self {
        Self::new(IDLE_GC)
    }
}

impl ChatRegistry {
    pub fn new(idle: Duration) -> Self {
        Self {
            chats: Mutex::new(HashMap::new()),
            idle,
        }
    }

    /// Record activity for a chat, bumping its message count and
    /// last-active stamp. GCs idle entries as a side effect so the map
    /// can't grow unbounded across many short-lived group chats.
    pub fn touch(&self, chat_id: i64, kind: ChatKind) {
        let Ok(mut g) = self.chats.lock() else { return };
        let now = Instant::now();
        g.retain(|_, s| now.duration_since(s.last_active) < self.idle);
        let entry = g.entry(chat_id).or_insert(ChatState {
            kind,
            last_active: now,
            message_count: 0,
        });
        entry.kind = kind;
        entry.last_active = now;
        entry.message_count += 1;
    }

    /// Count of chats active within the idle window.
    pub fn active_count(&self) -> usize {
        let Ok(mut g) = self.chats.lock() else {
            return 0;
        };
        let now = Instant::now();
        g.retain(|_, s| now.duration_since(s.last_active) < self.idle);
        g.len()
    }

    /// Total messages seen across all live chats.
    pub fn total_messages(&self) -> u64 {
        self.chats
            .lock()
            .map(|g| g.values().map(|s| s.message_count).sum())
            .unwrap_or(0)
    }
}

pub struct TelegramSession {
    client: Arc<TelegramClient>,
    handler: Arc<dyn TelegramMessageHandler>,
    approver: Option<Arc<TelegramApprover>>,
    pairing: Arc<PairingManager>,
    /// Shared with the worker: pairing approval mutates `allow_from`
    /// here (and persists), and the sink reads it for authorization.
    config: Arc<Mutex<TelegramConfig>>,
    registry: Arc<ChatRegistry>,
    output_ceiling: u32,
}

impl TelegramSession {
    pub fn new(
        client: Arc<TelegramClient>,
        handler: Arc<dyn TelegramMessageHandler>,
        config: Arc<Mutex<TelegramConfig>>,
        pairing: Arc<PairingManager>,
    ) -> Self {
        let output_ceiling = config
            .lock()
            .map(|c| c.output_ceiling)
            .unwrap_or(super::config::DEFAULT_OUTPUT_CEILING);
        Self {
            client,
            handler,
            approver: None,
            pairing,
            config,
            registry: Arc::new(ChatRegistry::default()),
            output_ceiling,
        }
    }

    pub fn with_approver(mut self, approver: Arc<TelegramApprover>) -> Self {
        self.approver = Some(approver);
        self
    }

    pub fn registry(&self) -> Arc<ChatRegistry> {
        self.registry.clone()
    }

    /// Drive the polling loop forever (until cancelled / fatal error).
    pub async fn run(self: Arc<Self>) -> Result<(), TelegramClientError> {
        let sink = SessionSink {
            client: self.client.clone(),
            handler: self.handler.clone(),
            approver: self.approver.clone(),
            pairing: self.pairing.clone(),
            config: self.config.clone(),
            registry: self.registry.clone(),
            output_ceiling: self.output_ceiling,
        };
        self.client.run(sink).await
    }
}

struct SessionSink {
    client: Arc<TelegramClient>,
    handler: Arc<dyn TelegramMessageHandler>,
    approver: Option<Arc<TelegramApprover>>,
    pairing: Arc<PairingManager>,
    config: Arc<Mutex<TelegramConfig>>,
    registry: Arc<ChatRegistry>,
    output_ceiling: u32,
}

impl SessionSink {
    /// True when `user_id` may DM without pairing.
    fn dm_authorized(&self, user_id: i64) -> bool {
        self.config
            .lock()
            .map(|c| c.allows_dm(user_id))
            .unwrap_or(false)
    }

    fn dm_policy(&self) -> DmPolicy {
        self.config.lock().map(|c| c.dm_policy).unwrap_or_default()
    }

    fn group_authorized(&self, chat_id: i64) -> bool {
        self.config
            .lock()
            .map(|c| c.allows_group(chat_id))
            .unwrap_or(false)
    }

    /// Forward an authorized text to the agent and ship the reply back,
    /// chunked. Spawned so the polling loop never blocks on a turn.
    fn spawn_turn(&self, chat_id: i64, text: String) {
        if let Some(approver) = &self.approver {
            approver.set_active_chat(chat_id);
        }
        let handler = self.handler.clone();
        let client = self.client.clone();
        let ceiling = self.output_ceiling;
        tokio::spawn(async move {
            let Some(reply) = handler.handle_message(text).await else {
                return;
            };
            for chunk in format_for_telegram(&reply, ceiling) {
                if let Err(e) = client.send_text(chat_id, chunk).await {
                    eprintln!("[telegram] reply send failed (chat {chat_id}): {e}");
                    break;
                }
            }
        });
    }

    /// First-contact pairing: mint/get a code, DM it to the user, and
    /// leave the request pending for the owner to approve in the GUI.
    fn spawn_pairing_prompt(&self, chat_id: i64, user_id: i64, display: String) {
        let pair = self.pairing.mint(user_id, chat_id, display);
        let client = self.client.clone();
        let code = pair.code.clone();
        tokio::spawn(async move {
            let body = format!(
                "👋 You're not paired with this thClaws yet.\n\nYour pairing code is <b>{code}</b>. \
                 Ask the thClaws owner to approve it in the desktop app. \
                 This code expires in 1 hour.",
            );
            if let Err(e) = client
                .send_message(&super::protocol::SendMessage::text(chat_id, body))
                .await
            {
                eprintln!("[telegram] pairing prompt send failed (chat {chat_id}): {e}");
            }
        });
    }

    /// Resolve a tapped inline-keyboard button: stop the spinner, edit
    /// the prompt to the chosen verdict, and unblock the waiting turn.
    fn handle_callback(&self, cbq: CallbackQuery) {
        let Some(approver) = self.approver.clone() else {
            return;
        };
        let data = cbq.data.clone().unwrap_or_default();
        // Resolve synchronously — this is what UNBLOCKS the agent turn
        // awaiting approval. Network confirmation (answer + edit) is
        // spawned so on_update returns promptly.
        let resolved = approver.record_decision_from_callback(&data);
        let client = self.client.clone();
        tokio::spawn(async move {
            let (toast, verdict_line) = match resolved {
                Some((ApprovalReply::Allow, _)) => ("Approved", "✅ Approved — running now."),
                Some((ApprovalReply::AllowAlways, _)) => (
                    "Always allowed",
                    "♾️ Approved for the rest of this session.",
                ),
                Some((ApprovalReply::Deny, _)) => ("Denied", "🚫 Denied — tool will not run."),
                _ => ("Expired", "This approval is no longer pending."),
            };
            let _ = client
                .answer_callback_query(&AnswerCallbackQuery::with_toast(cbq.id, toast))
                .await;
            // Edit the prompt message in place so the buttons disappear
            // and the verdict is shown. The CallbackQuery carries the
            // message to edit — no stored state needed.
            if let Some(msg) = cbq.message {
                let edit = EditMessageText::new(msg.chat.id, msg.message_id, verdict_line);
                if let Err(e) = client.edit_message_text(&edit).await {
                    eprintln!("[telegram] edit after approval failed: {e}");
                }
            }
        });
    }

    fn handle_message(&self, msg: Message) {
        let chat: Chat = msg.chat.clone();
        self.registry.touch(chat.id, chat.kind);

        let Some(text) = msg.text.clone() else {
            // Tier 1 is text-only; non-text messages (photos, stickers,
            // voice) are ignored until Tier 3 media support.
            return;
        };

        match chat.kind {
            ChatKind::Private => {
                let user_id = msg.from.as_ref().map(|u| u.id).unwrap_or(chat.id);
                if self.dm_authorized(user_id) {
                    self.route_authorized_text(chat.id, text);
                    return;
                }
                match self.dm_policy() {
                    DmPolicy::Pairing => {
                        let display = msg
                            .from
                            .as_ref()
                            .map(|u| u.display())
                            .unwrap_or_else(|| format!("id {user_id}"));
                        self.spawn_pairing_prompt(chat.id, user_id, display);
                    }
                    DmPolicy::Allowlist => {
                        // Silent ignore — no pairing prompt under
                        // allowlist policy (decision: unknown senders
                        // get no signal the bot exists).
                        eprintln!("[telegram] ignoring DM from unallowlisted user {user_id}");
                    }
                }
            }
            ChatKind::Group | ChatKind::Supergroup => {
                if self.group_authorized(chat.id) {
                    self.route_authorized_text(chat.id, text);
                } else {
                    eprintln!(
                        "[telegram] ignoring message from unallowlisted group {}",
                        chat.id
                    );
                }
            }
            ChatKind::Channel => {
                // Broadcast channel posts are Tier 2 (channel.rs).
            }
        }
    }

    /// Authorized text: either resolve a pending approval (free-text
    /// fallback) or run a turn. Mirrors the LINE sink's short-circuit so
    /// a typed "approve"/"deny" answers the gate instead of starting a
    /// new turn.
    fn route_authorized_text(&self, chat_id: i64, text: String) {
        if let Some(approver) = &self.approver {
            if approver.has_pending() {
                if let Some(reply) = approver.record_decision_from_text(&text) {
                    let msg = match reply {
                        ApprovalReply::Allow => "✅ Approved — running now.",
                        ApprovalReply::AllowAlways => "♾️ Approved for the session.",
                        ApprovalReply::Deny => "🚫 Denied.",
                        ApprovalReply::Unrecognised => {
                            "Reply with the buttons above, or type approve / deny."
                        }
                    };
                    let client = self.client.clone();
                    let msg = msg.to_string();
                    tokio::spawn(async move {
                        let _ = client.send_text(chat_id, msg).await;
                    });
                    return;
                }
            }
        }
        self.spawn_turn(chat_id, text);
    }
}

#[async_trait]
impl TelegramUpdateSink for SessionSink {
    async fn on_update(&self, update: Update) {
        if let Some(cbq) = update.callback_query {
            self.handle_callback(cbq);
            return;
        }
        if let Some(msg) = update.incoming_message() {
            self.handle_message(msg.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_counts_and_gcs() {
        let reg = ChatRegistry::new(Duration::from_secs(3600));
        reg.touch(111, ChatKind::Private);
        reg.touch(111, ChatKind::Private);
        reg.touch(-100, ChatKind::Group);
        assert_eq!(reg.active_count(), 2);
        assert_eq!(reg.total_messages(), 3);
    }

    #[test]
    fn registry_gc_drops_idle_entries() {
        // Zero idle window ⇒ every prior entry is immediately stale, so
        // a later touch sees only itself.
        let reg = ChatRegistry::new(Duration::ZERO);
        reg.touch(111, ChatKind::Private);
        // With ZERO idle, the entry is considered expired on the next
        // observation.
        assert_eq!(reg.active_count(), 0);
    }
}
