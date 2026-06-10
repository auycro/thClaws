//! Shell-as-default routing (dev-plan/39 Tier 1).
//!
//! Resolves which shell — if any — should be served at the workspace's
//! root URL when `--serve` starts up. Three sources in precedence order:
//!
//!   1. Explicit `--gui-shell <id>` CLI flag (`ShellServeMode.shell_id`)
//!   2. `settings.json::guiShell.serveDefault`
//!   3. `manifest.json::default_shell` (NEW — Tier 1)
//!
//! When any of these resolves, `--serve` mounts the shell handler at `/`
//! and shifts the classic chat UI to `/chat/`. When all are `None`, the
//! server keeps its pre-Tier-1 behavior (chat at `/`).
//!
//! The CLI flag + settings paths are already wired in `bin/app.rs`
//! before this module runs. This module adds the manifest fallback so
//! hosted runners don't need an explicit flag — extracting an agent
//! with `default_shell` in its manifest.json is enough.

use std::path::Path;

use crate::cloud::manifest::Manifest;

/// Look up the agent's `manifest.json` (if any) next to the given
/// working directory and return its `default_shell` field. Errors are
/// swallowed — a missing/invalid manifest just means "no default
/// shell," which is the correct behavior for chat-only agents.
pub fn manifest_default_shell(workdir: &Path) -> Option<String> {
    let manifest_path = workdir.join("manifest.json");
    let manifest = Manifest::from_path(&manifest_path).ok()?;
    manifest
        .default_shell
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Final precedence ladder. Caller passes whatever it has from earlier
/// resolution steps (CLI flag, settings) and we fall through to the
/// manifest as the last resort. Returning `None` means "no shell at
/// root — serve chat as before."
pub fn resolve_default_shell(
    cli_flag: Option<&str>,
    settings_default: Option<&str>,
    workdir: &Path,
) -> Option<String> {
    if let Some(s) = cli_flag.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    if let Some(s) = settings_default.map(str::trim).filter(|s| !s.is_empty()) {
        return Some(s.to_string());
    }
    manifest_default_shell(workdir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_manifest(dir: &Path, body: &str) {
        fs::write(dir.join("manifest.json"), body).unwrap();
    }

    #[test]
    fn no_manifest_returns_none() {
        let tmp = TempDir::new().unwrap();
        assert!(manifest_default_shell(tmp.path()).is_none());
    }

    #[test]
    fn manifest_without_default_shell_returns_none() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d"}"#,
        );
        assert!(manifest_default_shell(tmp.path()).is_none());
    }

    #[test]
    fn manifest_with_default_shell_is_returned() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
                "default_shell":"shells/dashboard"}"#,
        );
        assert_eq!(
            manifest_default_shell(tmp.path()).as_deref(),
            Some("shells/dashboard"),
        );
    }

    #[test]
    fn cli_flag_wins_over_settings_and_manifest() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
                "default_shell":"manifest-shell"}"#,
        );
        let resolved = resolve_default_shell(Some("cli-shell"), Some("settings-shell"), tmp.path());
        assert_eq!(resolved.as_deref(), Some("cli-shell"));
    }

    #[test]
    fn settings_wins_over_manifest() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
                "default_shell":"manifest-shell"}"#,
        );
        let resolved = resolve_default_shell(None, Some("settings-shell"), tmp.path());
        assert_eq!(resolved.as_deref(), Some("settings-shell"));
    }

    #[test]
    fn manifest_used_when_neither_cli_nor_settings_set() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
                "default_shell":"shells/grid"}"#,
        );
        let resolved = resolve_default_shell(None, None, tmp.path());
        assert_eq!(resolved.as_deref(), Some("shells/grid"));
    }

    #[test]
    fn empty_string_flag_falls_through() {
        let tmp = TempDir::new().unwrap();
        write_manifest(
            tmp.path(),
            r#"{"id":"x","name":"X","version":"0.1.0","description":"d",
                "default_shell":"shells/grid"}"#,
        );
        let resolved = resolve_default_shell(Some(""), Some("  "), tmp.path());
        assert_eq!(resolved.as_deref(), Some("shells/grid"));
    }
}
