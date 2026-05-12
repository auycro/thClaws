//! Claude Agent SDK provider — runs Claude Code as a local subprocess over
//! the Agent SDK control protocol.
//!
//! Spawns:
//!   `claude --output-format stream-json --input-format stream-json --verbose
//!    --permission-mode bypassPermissions --system-prompt <sys> [--model <m>]
//!    [--session-id <sid>]`
//!
//! Protocol (matches anthropics/claude-agent-sdk-python):
//!
//!   1. We send an `initialize` control_request on stdin:
//!        `{"type":"control_request","request_id":"req_<n>","request":{"subtype":"initialize","hooks":null}}`
//!   2. CLI replies with a matching `control_response` on stdout. We wait for
//!      it before anything else — otherwise the CLI ignores user input.
//!   3. We write a user message envelope on stdin:
//!        `{"type":"user","session_id":"","message":{"role":"user","content":"..."},"parent_tool_use_id":null}`
//!   4. We close stdin (no bidirectional hooks / SDK MCP servers, so we
//!      don't need it open past the first message).
//!   5. We stream stdout lines and parse events. Terminal event is
//!      `{"type":"result",...}` — emit MessageStop with usage.
//!
//! Model prefix: `agent/` (e.g. `agent/claude-sonnet-4-6`). Billing goes
//! through the user's Claude subscription — no ANTHROPIC_API_KEY required.
//!
//! Session persistence: we capture `session_id` from whichever frame
//! surfaces it first, store it in an `Arc<Mutex<Option<String>>>` owned by
//! the provider, and pass `--session-id <uuid>` on subsequent turns so the
//! CLI resumes the same conversation server-side.

use super::{EventStream, ModelInfo, Provider, ProviderEvent, StreamRequest};
use crate::error::{Error, Result};
use async_stream::try_stream;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};

pub struct AgentSdkProvider {
    /// Path to the `claude` CLI binary.
    claude_bin: String,
    /// Reusable session ID for context persistence.
    session_id: Arc<Mutex<Option<String>>>,
    /// Monotonic request id counter for control_request messages.
    next_req: Arc<Mutex<u64>>,
}

impl AgentSdkProvider {
    pub fn new() -> Self {
        let bin = std::env::var("CLAUDE_BIN").unwrap_or_else(|_| "claude".to_string());
        Self {
            claude_bin: bin,
            session_id: Arc::new(Mutex::new(None)),
            next_req: Arc::new(Mutex::new(0)),
        }
    }

    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.claude_bin = bin.into();
        self
    }

    fn next_request_id(&self) -> String {
        let mut n = self.next_req.lock().unwrap();
        *n += 1;
        // Random-ish suffix to avoid collisions across runs of the same
        // provider instance.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        format!("req_{}_{:08x}", *n, nanos)
    }
}

impl Default for AgentSdkProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Provider for AgentSdkProvider {
    async fn stream(&self, req: StreamRequest) -> Result<EventStream> {
        // Pull the user's latest turn. Prior history lives server-side under
        // --session-id, so we only send the new user message.
        let user_text = req
            .messages
            .last()
            .and_then(|m| {
                m.content.iter().find_map(|b| match b {
                    crate::types::ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            })
            .unwrap_or_default();

        // Build the CLI command.
        let mut cmd = Command::new(&self.claude_bin);
        cmd.arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--verbose")
            // Don't block on permission prompts — thClaws is the surface the
            // user interacts with; let Claude just run.
            .arg("--permission-mode")
            .arg("bypassPermissions");

        // Always set --system-prompt explicitly. Passing an empty string
        // suppresses Claude Code's bundled system prompt so the model sees
        // only thClaws's. Non-empty values replace the bundled prompt.
        let system = req.system.clone().unwrap_or_default();
        cmd.arg("--system-prompt").arg(&system);

        // Strip the `agent/` prefix for the actual model name.
        let model = req.model.strip_prefix("agent/").unwrap_or(&req.model);
        if !model.is_empty() {
            cmd.arg("--model").arg(model);
        }

        // Resume the previous session when we have one. `--resume <uuid>`
        // reattaches to an existing session; `--session-id` is for *setting*
        // a new session's id and errors with "Session ID is already in use"
        // if we re-pass it on turn 2.
        {
            let guard = self.session_id.lock().unwrap();
            if let Some(ref sid) = *guard {
                cmd.arg("--resume").arg(sid);
            }
        }

        // Mirror the Python SDK env hygiene: identify ourselves via
        // CLAUDE_CODE_ENTRYPOINT, and scrub CLAUDECODE so the child doesn't
        // think it's nested inside another Claude Code session.
        cmd.env("CLAUDE_CODE_ENTRYPOINT", "sdk-thclaws");
        cmd.env_remove("CLAUDECODE");

        cmd.stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd
            .spawn()
            .map_err(|e| Error::Provider(format!("spawn claude: {e}")))?;

        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| Error::Provider("no stdin handle".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| Error::Provider("no stdout handle".into()))?;
        let stderr = child.stderr.take();

        // Pipe child stderr to our stderr for visibility. Warnings about
        // outdated CLI version, auth problems, etc. surface here.
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while let Ok(n) = reader.read_line(&mut line).await {
                    if n == 0 {
                        break;
                    }
                    eprint!("\x1b[2m[claude] {}\x1b[0m", line);
                    line.clear();
                }
            });
        }

        let mut reader = BufReader::new(stdout);

        // ── 1. Send initialize ────────────────────────────────────────────
        let init_id = self.next_request_id();
        let init_req = json!({
            "type": "control_request",
            "request_id": init_id,
            "request": { "subtype": "initialize", "hooks": null }
        });
        stdin
            .write_all(init_req.to_string().as_bytes())
            .await
            .map_err(|e| Error::Provider(format!("write initialize: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Provider(format!("write initialize: {e}")))?;

        // ── 2. Wait for initialize response ──────────────────────────────
        // Anything that arrives before the response is discarded — the CLI
        // shouldn't emit anything else prior to it, but we stay defensive.
        let mut ack_line = String::new();
        loop {
            ack_line.clear();
            let n = tokio::time::timeout(
                std::time::Duration::from_secs(30),
                reader.read_line(&mut ack_line),
            )
            .await
            .map_err(|_| {
                Error::Provider(
                    "claude agent SDK: timed out waiting for initialize response. \
                     Is the claude CLI version current? (`claude --version`)"
                        .into(),
                )
            })?
            .map_err(|e| Error::Provider(format!("read initialize: {e}")))?;
            if n == 0 {
                return Err(Error::Provider(
                    "claude agent SDK: process exited before initialize response".into(),
                ));
            }
            let trimmed = ack_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            if v.get("type").and_then(Value::as_str) != Some("control_response") {
                continue;
            }
            if v.pointer("/response/request_id").and_then(Value::as_str) == Some(&init_id) {
                break;
            }
        }

        // ── 3. Send user message ─────────────────────────────────────────
        let user_msg = json!({
            "type": "user",
            "session_id": "",
            "message": { "role": "user", "content": user_text },
            "parent_tool_use_id": null,
        });
        stdin
            .write_all(user_msg.to_string().as_bytes())
            .await
            .map_err(|e| Error::Provider(format!("write user message: {e}")))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| Error::Provider(format!("write user message: {e}")))?;

        // No bidirectional hooks / SDK MCP servers on our end, so closing
        // stdin now signals EOF and lets the CLI commit the session file
        // cleanly once the turn finishes.
        drop(stdin);

        // ── 4. Stream stdout until `result` ──────────────────────────────
        let session_for_stream = self.session_id.clone();
        let raw_dump = super::RawDump::new(format!("agent-sdk {}", req.model));
        let event_stream = try_stream! {
            // Hold the child handle so the subprocess is reaped when the
            // stream is dropped.
            let _child = child;
            let mut reader: BufReader<ChildStdout> = reader;
            let mut line_buf = String::new();
            let mut seen_start = false;
            let mut first_text_yielded = false;
            let mut raw = raw_dump;

            loop {
                line_buf.clear();
                let n = reader.read_line(&mut line_buf).await
                    .map_err(|e| Error::Provider(format!("read stdout: {e}")))?;
                if n == 0 { break; } // EOF
                let trimmed = line_buf.trim();
                if trimmed.is_empty() { continue; }
                let Ok(v) = serde_json::from_str::<Value>(trimmed) else { continue };
                let msg_type = v.get("type").and_then(Value::as_str).unwrap_or("");

                // Capture the session id the first time it appears so the
                // next turn can pass --session-id <uuid>.
                if let Some(sid) = v.get("session_id").and_then(Value::as_str) {
                    if !sid.is_empty() {
                        *session_for_stream.lock().unwrap() = Some(sid.to_string());
                    }
                }

                if !seen_start {
                    yield ProviderEvent::MessageStart { model: req.model.clone() };
                    seen_start = true;
                }

                match msg_type {
                    // Full assistant message (claude -p emits one per reply,
                    // not per token). Content is an array of typed blocks.
                    "assistant" => {
                        if let Some(blocks) = v.pointer("/message/content").and_then(Value::as_array) {
                            for block in blocks {
                                let btype = block.get("type").and_then(Value::as_str).unwrap_or("");
                                match btype {
                                    "text" => {
                                        if let Some(text) = block.get("text").and_then(Value::as_str) {
                                            if !text.is_empty() {
                                                raw.push(text);
                                                let s = if first_text_yielded {
                                                    format!("\n\n{text}")
                                                } else {
                                                    first_text_yielded = true;
                                                    text.to_string()
                                                };
                                                yield ProviderEvent::TextDelta(s);
                                            }
                                        }
                                    }
                                    "tool_use" => {
                                        // Surface tool calls inline as a dim
                                        // marker — actual execution happens
                                        // server-side in Claude Code.
                                        let name = block.get("name").and_then(Value::as_str).unwrap_or("tool");
                                        yield ProviderEvent::TextDelta(
                                            format!("\n\x1b[2m🔧 [{name}]\x1b[0m\n")
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                    // Tool result / user echo — claude echoes these on stdout
                    // as it runs tools server-side. We ignore them; the model
                    // already has them server-side.
                    "user" => {}
                    // Any inbound control_request means claude is asking us
                    // something (permission, hook callback). With
                    // --permission-mode bypassPermissions we shouldn't get
                    // permission prompts, but acknowledge other subtypes as
                    // no-ops so we don't hang.
                    "control_request" => {}
                    "control_response" => {}
                    // Terminal frame: the turn is done.
                    "result" => {
                        let usage = v.get("usage").map(|u| super::Usage {
                            input_tokens: u.get("input_tokens")
                                .and_then(Value::as_u64).unwrap_or(0) as u32,
                            output_tokens: u.get("output_tokens")
                                .and_then(Value::as_u64).unwrap_or(0) as u32,
                            cache_creation_input_tokens: u.get("cache_creation_input_tokens")
                                .and_then(Value::as_u64).map(|v| v as u32),
                            cache_read_input_tokens: u.get("cache_read_input_tokens")
                                .and_then(Value::as_u64).map(|v| v as u32),
                        });
                        yield ProviderEvent::MessageStop {
                            stop_reason: Some("end_turn".into()),
                            usage,
                        };
                        break;
                    }
                    // Known non-terminal events we don't need to surface.
                    "system" | "rate_limit_event" | "keep_alive"
                    | "stream_event" | "task_started" | "task_progress"
                    | "task_notification" => {}
                    _ => {}
                }
            }

            // Stream closed without a `result` frame — emit a stop anyway so
            // the agent's turn doesn't hang.
            if seen_start {
                yield ProviderEvent::MessageStop {
                    stop_reason: Some("end_turn".into()),
                    usage: None,
                };
            }
        };

        Ok(Box::pin(event_stream))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>> {
        if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            let anthropic = crate::providers::anthropic::AnthropicProvider::new(api_key);
            let mut models = anthropic.list_models().await?;
            for m in &mut models {
                m.id = format!("agent/{}", m.id);
                if let Some(ref name) = m.display_name {
                    m.display_name = Some(format!("{} (Agent SDK)", name));
                }
            }
            Ok(models)
        } else {
            Err(crate::error::Error::Provider(
                "set ANTHROPIC_API_KEY to list models (or hard-code a `agent/<name>` in settings)"
                    .into(),
            ))
        }
    }

    /// Expose the captured Claude-side session UUID so the worker can
    /// persist it to the session JSONL after every turn. Pre-fix this
    /// value only lived in the `Arc<Mutex<>>` for the lifetime of the
    /// provider instance — process restart or `/load` lost it and the
    /// SDK started a fresh conversation that saw only the latest
    /// user message.
    fn provider_session_id(&self) -> Option<String> {
        self.session_id.lock().ok().and_then(|g| g.clone())
    }

    /// Rehydrate the Claude-side session UUID from a previously
    /// persisted value (typically a freshly-loaded session JSONL).
    /// The next `stream()` call will pass `--resume <uuid>` to the
    /// subprocess and the SDK restores its server-side history.
    /// `None` clears the slot (used on provider switch or explicit
    /// reset).
    fn set_provider_session_id(&self, id: Option<String>) {
        if let Ok(mut g) = self.session_id.lock() {
            *g = id;
        }
    }
}
