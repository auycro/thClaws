//! Lifecycle hooks — user-defined shell commands that fire on agent events.
//!
//! Configured in `~/.config/thclaws/settings.json`:
//! ```toml
//! [hooks]
//! pre_tool_use = "echo 'tool: $THCLAWS_TOOL_NAME' >> /tmp/thclaws.log"
//! post_tool_use = "echo 'result: $THCLAWS_TOOL_OUTPUT' >> /tmp/thclaws.log"
//! session_start = "notify-send 'thClaws session started'"
//! session_end = "notify-send 'thClaws session ended'"
//! ```
//!
//! Hook commands run via `/bin/sh -c` with environment variables providing context.
//! Hooks are fire-and-forget — failures are logged but never block the agent.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct HooksConfig {
    pub pre_tool_use: Option<String>,
    pub post_tool_use: Option<String>,
    pub post_tool_use_failure: Option<String>,
    pub permission_denied: Option<String>,
    pub session_start: Option<String>,
    pub session_end: Option<String>,
    pub pre_compact: Option<String>,
    pub post_compact: Option<String>,
}

impl HooksConfig {
    /// Get the command for a hook event, if configured.
    pub fn get(&self, event: HookEvent) -> Option<&str> {
        let cmd = match event {
            HookEvent::PreToolUse => self.pre_tool_use.as_deref(),
            HookEvent::PostToolUse => self.post_tool_use.as_deref(),
            HookEvent::PostToolUseFailure => self.post_tool_use_failure.as_deref(),
            HookEvent::PermissionDenied => self.permission_denied.as_deref(),
            HookEvent::SessionStart => self.session_start.as_deref(),
            HookEvent::SessionEnd => self.session_end.as_deref(),
            HookEvent::PreCompact => self.pre_compact.as_deref(),
            HookEvent::PostCompact => self.post_compact.as_deref(),
        };
        cmd.filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Copy)]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    PermissionDenied,
    SessionStart,
    SessionEnd,
    PreCompact,
    PostCompact,
}

/// Fire a hook command with the given environment variables. Non-blocking,
/// fire-and-forget. Failures are logged to stderr but never propagated.
pub fn fire(config: &HooksConfig, event: HookEvent, env: &HashMap<String, String>) {
    let Some(cmd) = config.get(event) else { return };

    let mut command = crate::util::shell_command_sync(cmd);

    // Standard env vars for all hooks.
    command.env("THCLAWS_HOOK_EVENT", format!("{event:?}"));
    for (k, v) in env {
        command.env(k, v);
    }

    // Fire and forget — spawn in background, don't wait.
    match command.spawn() {
        Ok(_) => {}
        Err(e) => {
            eprintln!("\x1b[33m[hook {event:?} failed: {e}]\x1b[0m");
        }
    }
}

/// Convenience: fire a pre_tool_use hook with tool name and input.
pub fn fire_pre_tool_use(config: &HooksConfig, tool_name: &str, input: &str) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_TOOL_NAME".into(), tool_name.into());
    env.insert(
        "THCLAWS_TOOL_INPUT".into(),
        input.chars().take(1000).collect(),
    );
    fire(config, HookEvent::PreToolUse, &env);
}

/// Convenience: fire a post_tool_use hook with tool name and output.
pub fn fire_post_tool_use(config: &HooksConfig, tool_name: &str, output: &str, is_error: bool) {
    let event = if is_error {
        HookEvent::PostToolUseFailure
    } else {
        HookEvent::PostToolUse
    };
    let mut env = HashMap::new();
    env.insert("THCLAWS_TOOL_NAME".into(), tool_name.into());
    env.insert(
        "THCLAWS_TOOL_OUTPUT".into(),
        output.chars().take(1000).collect(),
    );
    env.insert("THCLAWS_TOOL_ERROR".into(), is_error.to_string());
    fire(config, event, &env);
}

/// Convenience: fire session start/end.
pub fn fire_session(config: &HooksConfig, event: HookEvent, session_id: &str, model: &str) {
    let mut env = HashMap::new();
    env.insert("THCLAWS_SESSION_ID".into(), session_id.into());
    env.insert("THCLAWS_MODEL".into(), model.into());
    fire(config, event, &env);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_for_unconfigured_hooks() {
        let config = HooksConfig::default();
        assert!(config.get(HookEvent::PreToolUse).is_none());
        assert!(config.get(HookEvent::SessionStart).is_none());
    }

    #[test]
    fn get_returns_command_for_configured_hook() {
        let config = HooksConfig {
            pre_tool_use: Some("echo test".into()),
            ..Default::default()
        };
        assert_eq!(config.get(HookEvent::PreToolUse), Some("echo test"));
    }

    #[test]
    fn get_skips_empty_string() {
        let config = HooksConfig {
            pre_tool_use: Some(String::new()),
            ..Default::default()
        };
        assert!(config.get(HookEvent::PreToolUse).is_none());
    }

    #[test]
    fn fire_handles_missing_hook_gracefully() {
        let config = HooksConfig::default();
        fire(&config, HookEvent::PreToolUse, &HashMap::new());
        // No panic = pass.
    }
}
