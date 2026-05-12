//! `SessionRename` — give a stored session a meaningful title.
//!
//! Wraps [`crate::session::SessionStore::rename`], which appends a
//! `{"type":"rename","title":...}` event to the session's JSONL.
//! No content is rewritten — the rename is an audit-trailed metadata
//! change.
//!
//! Primarily exposed to the built-in `dream` agent so it can clean up
//! sessions while it's already mining them. Sessions auto-titled by
//! the chat surface are usually `sess-<8hex>` strings — meaningless
//! when scrolling through history. Dream sees the conversation
//! content, so it's the right surface to propose a one-line summary
//! ("debugging webview2 user-data folder permission" beats
//! "sess-7a3c1f9d" every time).
//!
//! Auto (no approval) — same risk profile as `Memory*` tools. The
//! audit trail in the session JSONL itself is the safety net.

use crate::error::{Error, Result};
use crate::session::SessionStore;
use crate::tools::{req_str, Tool};
use async_trait::async_trait;
use serde_json::{json, Value};

pub struct SessionRenameTool;

#[async_trait]
impl Tool for SessionRenameTool {
    fn name(&self) -> &'static str {
        "SessionRename"
    }
    fn description(&self) -> &'static str {
        "Rename (re-title) a stored session by appending a `rename` event to its JSONL. \
         Use this after reading a session to give it a human-readable title — e.g. \
         'debugging webview2 user-data folder' instead of 'sess-7a3c1f9d'. The session id \
         stays the same; only the display title changes. Audit-trailed."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": {
                    "type": "string",
                    "description": "The session's id (e.g. 'sess-7a3c1f9d') — usually the .jsonl filename stem under .thclaws/sessions/."
                },
                "title": {
                    "type": "string",
                    "description": "New display title — keep under ~80 chars; control characters are stripped automatically."
                }
            },
            "required": ["session_id", "title"]
        })
    }
    async fn call(&self, input: Value) -> Result<String> {
        let session_id = req_str(&input, "session_id")?;
        let title = req_str(&input, "title")?;
        let store_path = SessionStore::default_path()
            .ok_or_else(|| Error::Tool("no $HOME — session store unavailable".into()))?;
        let store = SessionStore::new(store_path);
        let renamed = store.rename(session_id, title)?;
        Ok(format!(
            "renamed {session_id} → {}",
            renamed.title.as_deref().unwrap_or("<unset>")
        ))
    }
}
