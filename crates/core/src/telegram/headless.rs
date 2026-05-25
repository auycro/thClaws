//! Headless Telegram bot (`thclaws --telegram`) — dev-plan/29 Tier 1
//! acceptance test #9: run the bridge with **no GUI feature**.
//!
//! The GUI path routes Telegram messages into the `shared_session`
//! worker, but that whole module is `#[cfg(feature = "gui")]`. So this
//! mode builds its own agent loop instead — the same construction
//! `repl::run_print_mode` uses (project context + memory + KMS system
//! prompt, builtin + KMS/memory tools, configured provider) — and drives
//! it directly from a [`TelegramMessageHandler`]. Turns are serialised
//! through a lock so two inbound messages can't race the agent's shared
//! history.
//!
//! Pairing note: headless has no GUI to approve pairing codes, so set
//! `TELEGRAM_OWNER_ID=<your numeric id>` for instant access. Other users
//! still get a pairing code, but approving it requires the GUI (or
//! pre-listing them in `~/.config/thclaws/telegram.json`).
//!
//! Tier 1 limits (vs the GUI path): no MCP servers, no session
//! persistence (history is in-memory for the process lifetime), single
//! shared session across chats (pairing gates who reaches it).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::StreamExt;

use crate::agent::{Agent, AgentEvent};
use crate::cancel::CancelToken;
use crate::config::AppConfig;
use crate::context::ProjectContext;
use crate::error::Result;
use crate::memory::MemoryStore;
use crate::permissions::{ApprovalSink, PermissionMode};
use crate::tools::ToolRegistry;

use super::approver::TelegramApprover;
use super::client::{TelegramClient, TelegramClientError};
use super::config::{validate_token, TelegramConfig};
use super::pairing::PairingManager;
use super::session::{TelegramMessageHandler, TelegramSession};

/// Env var whose numeric value is auto-added to `allow_from` at startup
/// so the owner can DM the bot without the GUI pairing-approval step.
pub const OWNER_ID_ENV: &str = "TELEGRAM_OWNER_ID";

/// Drives a single in-process [`Agent`] for each inbound message,
/// capturing the final assistant text. Turns are serialised on
/// `turn_lock` because the agent's history is shared mutable state.
struct HeadlessAgentHandler {
    agent: Arc<Agent>,
    turn_lock: Arc<tokio::sync::Mutex<()>>,
}

#[async_trait]
impl TelegramMessageHandler for HeadlessAgentHandler {
    async fn handle_message(&self, text: String) -> Option<String> {
        let _turn = self.turn_lock.lock().await;
        let mut stream = Box::pin(self.agent.run_turn(text));
        // Capture the FINAL assistant text — cleared on each tool call so
        // only post-last-tool narration survives (matches the GUI worker).
        let mut buf = String::new();
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::Text(s)) => buf.push_str(&s),
                Ok(AgentEvent::ToolCallStart { .. }) => buf.clear(),
                Ok(AgentEvent::Done { .. }) => break,
                Err(e) => return Some(format!("⚠️ thClaws hit an error: {e}")),
                _ => {}
            }
        }
        Some(buf)
    }
}

/// Run the headless Telegram bot until Ctrl-C or a fatal error (bad
/// token). Blocks for the process lifetime.
pub async fn run(config: AppConfig) -> Result<()> {
    // 1. Resolve the Telegram config + token (env beats file).
    let mut tg_cfg = TelegramConfig::load().ok().flatten().unwrap_or_default();
    let Some(token) = tg_cfg.resolved_token() else {
        eprintln!(
            "\x1b[31m[telegram] no bot token. Set TELEGRAM_BOT_TOKEN, or run \
             `thclaws telegram pair` for setup help.\x1b[0m"
        );
        std::process::exit(1);
    };
    if let Err(e) = validate_token(&token) {
        eprintln!("\x1b[31m[telegram] {e}\x1b[0m");
        std::process::exit(1);
    }
    // Owner id → allowlist (headless can't approve pairing via the GUI).
    if let Ok(owner) = std::env::var(OWNER_ID_ENV) {
        let owner = owner.trim();
        match owner.parse::<i64>() {
            Ok(id) if tg_cfg.add_allowed_user(id) => {
                eprintln!("[telegram] owner {id} allowlisted (from {OWNER_ID_ENV})");
            }
            Ok(_) => {}
            Err(_) if owner.is_empty() => {}
            Err(_) => eprintln!(
                "\x1b[33m[telegram] {OWNER_ID_ENV}='{owner}' is not a numeric user id; ignoring\x1b[0m"
            ),
        }
    }

    // 2. Build the agent (mirrors repl::run_print_mode construction).
    let cwd = std::env::current_dir()?;
    let ctx = ProjectContext::discover(&cwd)?;
    let memory_store = MemoryStore::default_path().map(MemoryStore::new);
    let system_fallback = if config.system_prompt.is_empty() {
        crate::prompts::defaults::SYSTEM
    } else {
        config.system_prompt.as_str()
    };
    let base_prompt = crate::prompts::load("system", system_fallback);
    let mut system = ctx.build_system_prompt(&base_prompt);
    if let Some(store) = &memory_store {
        if let Some(sec) = store.system_prompt_section() {
            system.push_str("\n\n# Memory\n");
            system.push_str(&sec);
        }
    }
    let kms_section = crate::kms::system_prompt_section(&config.kms_active);
    if !kms_section.is_empty() {
        system.push_str("\n\n");
        system.push_str(&kms_section);
    }

    let mut tools = ToolRegistry::with_builtins();
    tools.register(Arc::new(crate::tools::KmsReadTool));
    tools.register(Arc::new(crate::tools::KmsSearchTool));
    tools.register(Arc::new(crate::tools::KmsWriteTool));
    tools.register(Arc::new(crate::tools::KmsAppendTool));
    tools.register(Arc::new(crate::tools::KmsDeleteTool));
    tools.register(Arc::new(crate::tools::KmsCreateTool));
    tools.register(Arc::new(crate::tools::MemoryReadTool));
    tools.register(Arc::new(crate::tools::MemoryWriteTool));
    tools.register(Arc::new(crate::tools::MemoryAppendTool));

    let provider = crate::repl::build_provider(&config)?;

    // 3. Telegram transport: one client shared by the poller, the
    //    approver (sends prompts), and the session sink (sends replies).
    let cancel = CancelToken::new();
    let client = Arc::new(TelegramClient::new(token).with_cancel(cancel.clone()));
    match client.get_me().await {
        Ok(me) => {
            let label = me
                .username
                .map(|u| format!("@{u}"))
                .unwrap_or_else(|| me.first_name.clone());
            eprintln!("[telegram] connected as {label} (id {})", me.id);
        }
        Err(e) => {
            eprintln!("\x1b[31m[telegram] token rejected by Telegram: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
    let approver = Arc::new(TelegramApprover::new(client.clone()));
    let pairing = Arc::new(PairingManager::new());
    let shared_cfg = Arc::new(Mutex::new(tg_cfg));

    // 4. Agent with the Telegram approver + gated permission mode. Set
    //    the process-global mode too — the agent loop consults
    //    `current_mode()` at each tool gate.
    let agent = Agent::new(provider, tools, config.model.clone(), system)
        .with_max_iterations(config.max_iterations)
        .with_max_tokens(config.max_tokens)
        .with_permission_mode(PermissionMode::TelegramGated)
        .with_approver(approver.clone() as Arc<dyn ApprovalSink>);
    crate::permissions::set_current_mode(PermissionMode::TelegramGated);

    let handler: Arc<dyn TelegramMessageHandler> = Arc::new(HeadlessAgentHandler {
        agent: Arc::new(agent),
        turn_lock: Arc::new(tokio::sync::Mutex::new(())),
    });

    // 5. Ctrl-C → cancel the poll loop for a clean shutdown.
    let cancel_for_signal = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[telegram] shutting down…");
            cancel_for_signal.cancel();
        }
    });

    eprintln!("[telegram] headless bot running — Ctrl-C to stop. Tool approvals appear in-chat.");
    if shared_cfg
        .lock()
        .map(|c| c.allow_from.is_empty())
        .unwrap_or(true)
    {
        eprintln!(
            "\x1b[33m[telegram] no allowlisted users yet — set {OWNER_ID_ENV}=<your id> for \
             instant access, or DM the bot to mint a pairing code (approve it from the GUI).\x1b[0m"
        );
    }

    let session = Arc::new(
        TelegramSession::new(client, handler, shared_cfg, pairing).with_approver(approver),
    );
    match session.run().await {
        // A clean Ctrl-C cancellation is success, not an error.
        Ok(()) | Err(TelegramClientError::Cancelled) => Ok(()),
        Err(e) => {
            eprintln!("\x1b[31m[telegram] session ended: {e}\x1b[0m");
            std::process::exit(1);
        }
    }
}
