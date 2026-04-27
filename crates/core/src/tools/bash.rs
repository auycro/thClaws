//! `Bash` — run an arbitrary shell command via `/bin/sh -c`.
//!
//! Always requires approval (`requires_approval -> true`) until allow-list
//! patterns land. Captures stdout + stderr separately, interleaves in the
//! returned string, and enforces a default 120000ms timeout (max 600000ms).
//! On timeout the child is killed and any partial output is discarded —
//! we report the timeout clearly rather than return half-baked state.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::time::{timeout, Duration};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;

pub struct BashTool;

#[async_trait]
impl Tool for BashTool {
    fn name(&self) -> &'static str {
        "Bash"
    }

    fn description(&self) -> &'static str {
        "Run a shell command via `/bin/sh -c`. Captures stdout and stderr. \
         Default timeout: 120000ms (override with `timeout` in milliseconds, max 600000). \
         Always requires approval. Use this for general operations (git, build, \
         test, curl, ls -l, rm, etc.) that the specialized tools don't cover. \
         IMPORTANT: For long-running processes (servers, watchers, dev servers), \
         append ` &` to run in background, or use `timeout 10 command` to sample \
         initial output. Never run a server in foreground — it blocks until timeout."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "The shell command to run"
                },
                "cwd": {
                    "type": "string",
                    "description": "Working directory (default: current directory)"
                },
                "timeout": {
                    "type": "integer",
                    "description": "Timeout in milliseconds (default 120000, max 600000)"
                },
                "timeout_secs": {
                    "type": "integer",
                    "description": "Legacy alias: timeout in seconds (converted to ms internally)"
                },
                "description": {
                    "type": "string",
                    "description": "Brief description of what this command does"
                }
            },
            "required": ["command"]
        })
    }

    fn requires_approval(&self, input: &Value) -> bool {
        // Always require approval, but flag destructive commands so the
        // approval prompt can highlight the risk.
        if let Some(cmd) = input.get("command").and_then(Value::as_str) {
            if is_destructive_command(cmd) {
                return true; // could be a higher tier in the future
            }
        }
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw_command = req_str(&input, "command")?;
        let cwd = input.get("cwd").and_then(Value::as_str);

        let resolved_cwd = if let Some(c) = cwd {
            crate::sandbox::Sandbox::check(c)?
        } else if let Some(root) = crate::sandbox::Sandbox::root() {
            root
        } else {
            std::env::current_dir()?
        };

        // Auto-activate venv for pip/python commands when no venv exists yet.
        let raw_command = maybe_wrap_with_venv(raw_command, &resolved_cwd);

        let timeout_ms = input
            .get("timeout")
            .and_then(Value::as_u64)
            .or_else(|| {
                input
                    .get("timeout_secs")
                    .and_then(Value::as_u64)
                    .map(|s| s * 1000)
            })
            .unwrap_or(DEFAULT_TIMEOUT_MS)
            .min(MAX_TIMEOUT_MS);

        // Chained commands like "pip install X && uvicorn app --port 8800":
        // Split at `&&`, run setup parts synchronously, then run the server
        // part with a short capture timeout so it doesn't block forever.
        let (setup_parts, server_part) = split_chained_server_command(&raw_command);

        // Run setup commands first (if any).
        let mut setup_output = String::new();
        if !setup_parts.is_empty() {
            let setup_cmd = setup_parts.join(" && ");
            eprintln!(
                "\x1b[33m[running setup: {}]{}\x1b[0m",
                setup_cmd.chars().take(80).collect::<String>(),
                if setup_cmd.len() > 80 { "…" } else { "" }
            );
            setup_output = run_shell_command(&setup_cmd, &resolved_cwd, timeout_ms, false).await?;
            // If setup failed, return its output (includes exit code).
            if setup_output.contains("[exit code") {
                return Ok(setup_output);
            }
            // If there's no server part, just return setup output.
            if server_part.is_none() {
                return Ok(setup_output);
            }
        }

        // If we split out a server part, ensure venv is activated for it too.
        let command = match server_part {
            Some(ref srv) => {
                let venv_activate = resolved_cwd.join(".venv/bin/activate");
                if venv_activate.exists() {
                    format!("source {} && {}", venv_activate.display(), srv)
                } else {
                    srv.clone()
                }
            }
            None => raw_command.to_string(),
        };
        let is_server = is_server_command(&command) && !command.trim().ends_with('&');

        // Lead-only hard block. The team lead is a coordinator — destructive
        // workspace ops have repeatedly cascade-killed teammate worktrees
        // and processes when the LLM lead reached for `git reset --hard` or
        // `rm -rf` to "clean up" unexpected state. The prompt rule alone is
        // honor-system in --accept-all mode; this is the seatbelt.
        if let Some(reason) = lead_forbidden_command(&command) {
            return Err(Error::Tool(format!(
                "team lead is not allowed to run this command: it would {reason}. \
                 Lead is a COORDINATOR — destructive workspace ops belong to \
                 teammates inside their own worktrees, never the lead. If a \
                 merge looks weird or git state is unexpected, send a message \
                 to the user describing what you see — do NOT attempt recovery \
                 with `git reset`, `rm -rf`, or `git worktree remove`. Use \
                 `git status`, `git log`, `git diff` to inspect; use TeamMerge \
                 and SendMessage to act."
            )));
        }
        // Teammate-only hard block. Catches the cross-branch `git reset
        // --hard main` pattern that wiped frontend's worktree last run.
        // Same-branch recovery (HEAD~N, sha) stays allowed.
        if let Some(reason) = teammate_forbidden_command(&command) {
            return Err(Error::Tool(format!(
                "teammate is not allowed to run this command: it would {reason}."
            )));
        }

        if is_destructive_command(&command) {
            eprintln!(
                "\x1b[33m⚠ destructive command detected: {}\x1b[0m",
                command.chars().take(80).collect::<String>()
            );
        }

        if is_server {
            eprintln!(
                "\x1b[33m[server command detected — will capture 5s of startup then return]\x1b[0m"
            );
        }

        let effective_timeout = if is_server { 5000 } else { timeout_ms };
        let server_output =
            run_shell_command(&command, &resolved_cwd, effective_timeout, is_server).await?;

        // Combine setup output with server output.
        if setup_output.is_empty() {
            Ok(server_output)
        } else {
            Ok(format!("{setup_output}\n{server_output}"))
        }
    }
}

/// Run a single shell command, capturing stdout/stderr.
/// If `is_server` is true, a timeout is expected — the server keeps running
/// and we return immediately without killing it.
async fn run_shell_command(
    command: &str,
    cwd: &std::path::Path,
    timeout_ms: u64,
    is_server: bool,
) -> Result<String> {
    let mut cmd = crate::util::shell_command_async(command);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(cwd);

    let mut child = cmd
        .spawn()
        .map_err(|e| Error::Tool(format!("spawn: {e}")))?;

    let mut stdout_pipe = child
        .stdout
        .take()
        .ok_or_else(|| Error::Tool("missing stdout pipe".into()))?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| Error::Tool("missing stderr pipe".into()))?;

    let stdout_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf).await;
        buf
    });
    let stderr_task = tokio::spawn(async move {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf).await;
        buf
    });

    let wait_result = timeout(Duration::from_millis(timeout_ms), child.wait()).await;
    match wait_result {
        Err(_) if is_server => {
            // Server command — timeout is expected. Server keeps running.
            // DON'T await reader tasks (pipes still open, would block forever).
            drop(stdout_task);
            drop(stderr_task);
            Ok(format!(
                "Server started and running in background.\n\
                 The process will continue after this tool returns.\n\
                 Use `curl localhost:PORT` or a browser to verify."
            ))
        }
        Err(_) => {
            let _ = child.kill().await;
            Err(Error::Tool(format!(
                "timeout after {}ms running: {command}",
                timeout_ms
            )))
        }
        Ok(Err(e)) => Err(Error::Tool(format!("wait: {e}"))),
        Ok(Ok(status)) => {
            let stdout_bytes = stdout_task.await.unwrap_or_default();
            let stderr_bytes = stderr_task.await.unwrap_or_default();
            let stdout = String::from_utf8_lossy(&stdout_bytes);
            let stderr = String::from_utf8_lossy(&stderr_bytes);
            let exit_code = status.code().unwrap_or(-1);
            Ok(format_output(&stdout, &stderr, exit_code))
        }
    }
}

/// Split a chained command like "pip install X && uvicorn app --port 8800"
/// into setup parts and an optional server part. If the last segment of a
/// `&&`-chain is a server command, it's extracted separately so we can run
/// setup synchronously and then start the server with a short capture timeout.
fn split_chained_server_command(cmd: &str) -> (Vec<String>, Option<String>) {
    // Only split on top-level `&&` (not inside quotes/subshells — good enough
    // for the common pip install && uvicorn pattern).
    let parts: Vec<&str> = cmd.split("&&").map(|s| s.trim()).collect();
    if parts.len() < 2 {
        // Single command — no splitting needed.
        return (vec![], None);
    }
    let last = parts.last().unwrap();
    if is_server_command(last) {
        let setup: Vec<String> = parts[..parts.len() - 1]
            .iter()
            .map(|s| s.to_string())
            .collect();
        (setup, Some(last.to_string()))
    } else {
        // No server command at the end — run as one unit.
        (vec![], None)
    }
}

/// If `cmd` contains a bare `pip install` and there's no venv in the cwd,
/// create one and activate it before running the command.
fn maybe_wrap_with_venv(cmd: &str, cwd: &std::path::Path) -> String {
    if !needs_venv(cmd) {
        return cmd.to_string();
    }
    // Already inside a venv (e.g. the command itself sources activate)?
    if cmd.contains("activate") || cmd.contains("venv/bin/") || cmd.contains(".venv/bin/") {
        return cmd.to_string();
    }
    let venv_dir = cwd.join(".venv");
    if venv_dir.join("bin/activate").exists() {
        // venv exists but isn't activated — activate it.
        eprintln!("\x1b[33m[auto-activating .venv before pip]\x1b[0m");
        format!("source {}/bin/activate && {}", venv_dir.display(), cmd)
    } else {
        // No venv at all — create + activate.
        eprintln!("\x1b[33m[creating .venv and activating before pip]\x1b[0m");
        format!(
            "python3 -m venv {} && source {}/bin/activate && {}",
            venv_dir.display(),
            venv_dir.display(),
            cmd
        )
    }
}

/// Does this command need a Python venv? Any python/pip command should use
/// the project venv if one exists, plus specific tool commands.
fn needs_venv(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // Any python/pip invocation should use the venv.
    lower.starts_with("python ")
        || lower.starts_with("python3 ")
        || lower.contains("pip install")
        || lower.contains("pip3 install")
        || lower.contains("uvicorn ")
        || lower.contains("gunicorn ")
        || lower.contains("hypercorn ")
        || lower.contains("flask run")
        || lower.contains("django")
        || lower.contains("manage.py")
        || lower.contains("fastapi")
        || lower.contains("pytest")
        || lower.contains("celery ")
}

/// Detect commands that are potentially destructive to the filesystem or system.
///
/// This feeds the approval prompt's risk-highlighting; `BashTool` already
/// requires approval for every command. We lowercase + normalise
/// whitespace before matching so a crafty `rm  -rf` (double-space) or
/// tab-separated variant can't slip past the classifier just because
/// it doesn't hit the exact ASCII byte sequence we listed.
/// True when this process is a teammate (spawned by SpawnTeammate with
/// `THCLAWS_TEAM_AGENT` set), as opposed to the lead or a standalone session.
fn is_teammate_process() -> bool {
    std::env::var("THCLAWS_TEAM_AGENT").is_ok()
}

/// Distinguish a benign `git reset --hard` ref (recovery on the teammate's
/// own branch) from the dangerous "reset to a different branch" pattern
/// that wiped frontend's worktree in our last run.
///
/// Allowed (safe): `HEAD`, `HEAD~N`, `HEAD^`, `HEAD@{N}`, hex shas (≥7 hex
/// chars), tags (`tags/...`).
/// Blocked: anything else — bare branch names like `main`, `master`, `dev`,
/// remote refs like `origin/main`, sibling team branches like `team/backend`.
fn ref_resets_to_different_branch(target: &str) -> bool {
    if target.is_empty() {
        return false;
    }
    let lower = target.to_lowercase();
    if lower == "head" || lower.starts_with("head~") || lower.starts_with("head^") {
        return false;
    }
    if lower.starts_with("head@{") {
        return false;
    }
    if lower.starts_with("tags/") || lower.starts_with("refs/tags/") {
        return false;
    }
    // Hex SHA (full or abbreviated, ≥7 chars). Anything less is too short
    // to disambiguate and most likely a branch name.
    if target.len() >= 7 && target.chars().all(|c| c.is_ascii_hexdigit()) {
        return false;
    }
    true
}

/// Commands a teammate must never run. Catches the specific footguns that
/// have wiped teammate worktrees in past runs (`git reset --hard main`,
/// `git reset --hard origin/...`, `git reset --hard team/<sibling>`).
/// `git reset --hard HEAD~N` and `git reset --hard <sha>` stay allowed —
/// those are legitimate same-branch recovery moves.
pub fn teammate_forbidden_command(cmd: &str) -> Option<&'static str> {
    if !is_teammate_process() {
        return None;
    }
    let lower = cmd.to_lowercase();
    let collapsed: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    let padded = format!(" {collapsed} ");

    // Find `git reset --hard <ref>` and inspect the ref. Use the original
    // (case-preserved) cmd to extract the ref so SHAs stay matchable.
    if let Some(after) = padded.split(" git reset --hard ").nth(1) {
        let target_lc = after.split_whitespace().next().unwrap_or("");
        // Map back to the original-case token so a SHA passes the hex check.
        let target_orig = cmd
            .split_whitespace()
            .skip_while(|t| t.to_lowercase() != "--hard")
            .nth(1)
            .unwrap_or(target_lc);
        if ref_resets_to_different_branch(target_orig) {
            return Some(
                "reset to a different branch / remote ref — would discard your branch's commits and overwrite your worktree with someone else's tree. Use `git reset --hard HEAD~N` or `git reset --hard <sha>` if you genuinely need to undo your own commits, OR ask the lead to handle the merge instead",
            );
        }
    }

    None
}

/// Commands the team lead must never run. Returns the human-readable reason
/// (used in the error message) or None when allowed. Always None for non-lead
/// processes — teammates legitimately use these inside their own worktrees.
pub fn lead_forbidden_command(cmd: &str) -> Option<&'static str> {
    if !crate::team::is_team_lead() {
        return None;
    }
    let collapsed: String = cmd
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let lower = format!(" {collapsed} ");

    let blocked: &[(&str, &str)] = &[
        ("git reset --hard", "discard committed work via hard reset"),
        ("git clean -f", "delete untracked files"),
        ("git clean -d", "delete untracked directories"),
        ("git push --force", "rewrite shared history with force-push"),
        ("git push -f ", "rewrite shared history with force-push"),
        ("git rebase", "rewrite committed history"),
        (
            "git worktree remove",
            "kill a teammate's active worktree (and its process)",
        ),
        (
            "git worktree prune",
            "purge worktree metadata referenced by live teammates",
        ),
        ("git checkout -- ", "discard a teammate's uncommitted work"),
        ("git checkout .", "discard a teammate's uncommitted work"),
        (
            "git restore --worktree",
            "discard a teammate's uncommitted work",
        ),
        ("git restore .", "discard a teammate's uncommitted work"),
        (
            "git merge --abort",
            "tear down a merge instead of resolving via the responsible teammate",
        ),
        ("rm -rf", "destructively remove files"),
        ("rm -fr", "destructively remove files"),
        ("rm -r ", "recursively remove files"),
    ];

    for (pat, why) in blocked {
        if lower.contains(pat) {
            return Some(why);
        }
    }
    None
}

pub fn is_destructive_command(cmd: &str) -> bool {
    let raw = cmd.to_lowercase();
    // Collapse any run of whitespace (tabs, newlines, multi-space) to a
    // single space AND pad with a space on both ends so patterns that
    // want to match a flag-in-context (e.g. ` -delete`, ` source `) can
    // anchor against the padding without missing commands that happen
    // to start or end with the target token.
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let padded = format!(" {collapsed} ");
    let lower = padded.as_str();

    let simple_patterns = [
        // Filesystem destruction
        "rm -rf",
        "rm -fr",
        "rmdir",
        "rm -r",
        "rm -f ",
        "mv ",
        "truncate",
        "> /",
        "dd if=",
        "mkfs",
        "shred ",
        "wipe ",
        // Permission/ownership sweeps
        "chmod -r",
        "chown -r",
        // Process control
        "kill -9",
        "killall",
        "pkill",
        // Privilege escalation
        "sudo ",
        "doas ",
        // System power state
        "shutdown",
        "reboot",
        "poweroff",
        "halt ",
        "systemctl poweroff",
        "systemctl reboot",
        "systemctl halt",
        // Fork-bomb
        ":(){ :|:& };:",
        // Low-level format
        "format ",
        // Git history + working-tree destruction
        "git reset --hard",
        "git clean -f",
        "git clean -d",
        "git push --force",
        "git push -f ",
        "git push --delete",
        "git branch -d ",
        "git branch -d",
        "git tag -d ",
        "git filter-branch",
        "git filter-repo",
        "git update-ref -d",
        "git checkout -- ",
        "git checkout .",
        "git restore --staged",
        "git restore --worktree",
        "git restore .",
        "git stash drop",
        "git stash clear",
        // Archive / sync that can silently overwrite
        "tar --overwrite",
        "rsync --delete",
        "rsync -a --delete",
        // Filesystem search-and-destroy — match the flag with a
        // leading space so it catches `find ... -delete` regardless of
        // trailing args, without being triggered by the literal string
        // `-delete` appearing mid-word.
        " -delete",
        " -exec rm",
        // Low-level removal
        "unlink ",
        "fallocate -p",
        // Piped script execution (dot-source, `source`, process sub)
        " . ./",
        " . /",
        " source ",
        "| bash",
        "|bash",
        "| zsh",
        "|zsh",
        "| python",
        "|python",
        "| perl",
        "|perl",
        "| ruby",
        "|ruby",
        " bash <(",
        " zsh <(",
        " sh <(",
        " python <(",
        // Windows destructive (matched post-lowercase)
        "del /f",
        "del /s",
        "del /q",
        "rd /s",
        "rd /q",
        "cipher /w",
        // Container / orchestrator destruction
        "docker rm -f",
        "docker rmi -f",
        "docker system prune",
        "docker volume rm",
        "docker network rm",
        "podman rm -f",
        "podman system prune",
        "kubectl delete",
        "helm uninstall",
        "helm delete",
        "terraform destroy",
        // Cloud CLIs
        "aws s3 rb",
        "aws s3 rm",
        "aws ec2 terminate-instances",
        "aws rds delete",
        "gcloud compute instances delete",
        "gcloud projects delete",
        "az group delete",
        // SQL (very coarse — only blocks the obvious DDL/DML)
        "drop database",
        "drop table",
        "truncate table",
        "delete from ",
        // Package-manager wipes
        "apt-get remove",
        "apt remove",
        "yum remove",
        "dnf remove",
        "brew uninstall",
        "npm uninstall -g",
        "pnpm remove -g",
        "pip uninstall -y",
        "cargo uninstall",
        // Filesystem snapshot
        "zfs destroy",
        "btrfs subvolume delete",
    ];
    if simple_patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // Detect piping download commands into a shell: curl ... | sh, wget ... | bash
    if lower.contains("| sh")
        || lower.contains("|sh")
        || lower.contains("| bash")
        || lower.contains("|bash")
        || lower.contains("| zsh")
        || lower.contains("|zsh")
    {
        if lower.contains("curl") || lower.contains("wget") || lower.contains("fetch ") {
            return true;
        }
    }

    false
}

/// Detect commands that start long-running server processes.
pub fn is_server_command(cmd: &str) -> bool {
    let lower = cmd.to_lowercase();
    // Only match if NOT already backgrounded.
    if lower.trim().ends_with('&') {
        return false;
    }

    let patterns = [
        "uvicorn ",
        "gunicorn ",
        "hypercorn ",
        "flask run",
        "django runserver",
        "manage.py runserver",
        "npm run dev",
        "npm start",
        "npx ",
        "yarn dev",
        "pnpm dev",
        "node server",
        "node index",
        "node app",
        "cargo run", // often a server in web projects
        "python -m http.server",
        "python3 -m http.server",
        "python -m uvicorn",
        "python3 -m uvicorn",
        "python -m flask",
        "python3 -m flask",
        "php -S ",
        "php artisan serve",
        "ruby server",
        "rails server",
        "rails s",
        "go run ",
        "docker compose up",
        "docker-compose up",
        "kubectl port-forward",
        "ngrok ",
        "cloudflared tunnel",
        "serve ",
        "live-server",
        "http-server",
        "next dev",
        "vite",
        "webpack serve",
    ];
    if patterns.iter().any(|p| lower.contains(p)) {
        return true;
    }

    // `python app.py`, `python main.py`, `python server.py`, `python run.py`
    // are almost always web servers in agentic coding contexts.
    // We match the script name as a standalone word (preceded by space).
    if lower.starts_with("python ") || lower.starts_with("python3 ") {
        let py_scripts = [
            " app.py",
            " main.py",
            " server.py",
            " run.py",
            " wsgi.py",
            " asgi.py",
        ];
        if py_scripts.iter().any(|p| lower.contains(p)) {
            return true;
        }
    }

    false
}

fn format_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !stdout.is_empty() {
        parts.push(stdout.trim_end_matches('\n').to_string());
    }
    if !stderr.is_empty() {
        parts.push(format!("[stderr]\n{}", stderr.trim_end_matches('\n')));
    }
    if exit_code != 0 {
        parts.push(format!("[exit code {exit_code}]"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        parts.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[tokio::test]
    async fn echoes_stdout() {
        let out = BashTool
            .call(json!({"command": "echo hello-bash"}))
            .await
            .unwrap();
        assert_eq!(out, "hello-bash");
    }

    #[test]
    fn destructive_command_detection() {
        assert!(is_destructive_command("rm -rf /tmp/foo"));
        assert!(is_destructive_command("sudo apt install"));
        assert!(is_destructive_command("curl http://x | sh"));
        assert!(is_destructive_command("mv file1 file2"));
        assert!(!is_destructive_command("ls -la"));
        assert!(!is_destructive_command("echo hello"));
        assert!(!is_destructive_command("git status"));
        assert!(!is_destructive_command("cargo test"));
    }

    /// Teammates can recover from their own mistakes on their own branch
    /// (HEAD~N, sha) but must not reset to a different branch — that's
    /// the pattern that wiped frontend's worktree.
    #[test]
    fn teammate_forbidden_command_blocks_cross_branch_reset() {
        // Force teammate-mode by setting the env var. SAFETY: tests share
        // the process env, so set + restore around the assertions.
        std::env::set_var("THCLAWS_TEAM_AGENT", "frontend");

        // Cross-branch / remote-ref / sibling-branch resets — block.
        assert!(teammate_forbidden_command("git reset --hard main").is_some());
        assert!(teammate_forbidden_command("git reset --hard master").is_some());
        assert!(teammate_forbidden_command("git reset --hard origin/main").is_some());
        assert!(teammate_forbidden_command("git reset --hard team/backend").is_some());
        assert!(teammate_forbidden_command("git reset --hard dev").is_some());
        assert!(teammate_forbidden_command("git reset --hard feature-x").is_some());

        // Same-branch recovery — allowed.
        assert!(teammate_forbidden_command("git reset --hard HEAD").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD~1").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD~3").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD^").is_none());
        assert!(teammate_forbidden_command("git reset --hard HEAD@{2}").is_none());
        assert!(teammate_forbidden_command("git reset --hard a11930a").is_none());
        assert!(teammate_forbidden_command("git reset --hard a11930af0e9c").is_none());

        // Tags — allowed.
        assert!(teammate_forbidden_command("git reset --hard tags/v1.0").is_none());

        // Other commands — allowed (they're for the destructive-warning
        // layer, not this one).
        assert!(teammate_forbidden_command("git status").is_none());
        assert!(teammate_forbidden_command("rm -rf node_modules").is_none());

        std::env::remove_var("THCLAWS_TEAM_AGENT");

        // When NOT a teammate, every command passes — the lead and
        // standalone sessions don't have this restriction (they have
        // their own guards or none).
        assert!(teammate_forbidden_command("git reset --hard main").is_none());
    }

    #[test]
    fn lead_forbidden_command_behavior() {
        // Tests share the AtomicBool, so toggle explicitly in this test
        // and never rely on default state. All assertions about "off"
        // run first and "on" later in the same test, then restore off.
        crate::team::set_is_team_lead(false);
        assert!(lead_forbidden_command("git reset --hard HEAD").is_none());
        assert!(lead_forbidden_command("rm -rf /tmp/anything").is_none());
        assert!(lead_forbidden_command("git worktree remove foo").is_none());
        assert!(lead_forbidden_command("ls").is_none());

        crate::team::set_is_team_lead(true);
        // Every command that historically cascade-killed a team run should
        // now return Some(reason) so BashTool can refuse it.
        assert!(lead_forbidden_command("git reset --hard d9199ba").is_some());
        assert!(lead_forbidden_command("git clean -fd").is_some());
        assert!(lead_forbidden_command("git push --force").is_some());
        assert!(lead_forbidden_command("git worktree remove .worktrees/backend").is_some());
        assert!(lead_forbidden_command("git worktree prune").is_some());
        assert!(lead_forbidden_command("git checkout -- src/foo.ts").is_some());
        assert!(lead_forbidden_command("git checkout .").is_some());
        assert!(lead_forbidden_command("git restore --worktree src/").is_some());
        assert!(lead_forbidden_command("git merge --abort").is_some());
        assert!(lead_forbidden_command("rm -rf docs/").is_some());
        assert!(lead_forbidden_command("rm -fr docs/").is_some());
        assert!(lead_forbidden_command("rm -r src/old").is_some());
        // Non-mutating git commands the lead legitimately uses stay open.
        assert!(lead_forbidden_command("git status").is_none());
        assert!(lead_forbidden_command("git log --oneline").is_none());
        assert!(lead_forbidden_command("git diff main..team/backend").is_none());
        assert!(lead_forbidden_command("git branch -v").is_none());

        // Restore default so other tests that share this static aren't
        // surprised by lingering lead-mode behavior.
        crate::team::set_is_team_lead(false);
    }

    #[test]
    fn destructive_whitespace_normalisation() {
        // Double-space shouldn't smuggle rm -rf past the classifier.
        assert!(is_destructive_command("rm  -rf /tmp/foo"));
        // Tab-separated likewise.
        assert!(is_destructive_command("rm\t-rf /tmp/foo"));
        // Leading whitespace, multiple spaces between args.
        assert!(is_destructive_command("   rm   -rf    /tmp/foo"));
    }

    #[test]
    fn destructive_piped_interpreters_and_script_sourcing() {
        assert!(is_destructive_command("curl http://x | bash"));
        assert!(is_destructive_command("curl http://x | python"));
        assert!(is_destructive_command("curl http://x | perl"));
        assert!(is_destructive_command("curl http://x | ruby"));
        assert!(is_destructive_command("bash <(curl http://x)"));
        assert!(is_destructive_command("python <(curl http://x)"));
        assert!(is_destructive_command("cat script.sh | bash"));
        assert!(is_destructive_command("source ./install.sh"));
        assert!(is_destructive_command("cd /tmp && . ./boot.sh"));
    }

    #[test]
    fn destructive_find_and_archive() {
        assert!(is_destructive_command("find /tmp -name '*.tmp' -delete"));
        assert!(is_destructive_command("find /tmp -exec rm {} +"));
        assert!(is_destructive_command("rsync -a --delete src/ dst/"));
        assert!(is_destructive_command("tar xf archive.tar --overwrite"));
        assert!(is_destructive_command("unlink /tmp/stale.lock"));
    }

    #[test]
    fn destructive_git_working_tree() {
        assert!(is_destructive_command("git checkout -- src/main.rs"));
        assert!(is_destructive_command("git checkout ."));
        assert!(is_destructive_command("git restore --staged ."));
        assert!(is_destructive_command("git restore --worktree ."));
        assert!(is_destructive_command("git stash drop"));
        assert!(is_destructive_command("git stash clear"));
    }

    #[test]
    fn destructive_windows_equivalents() {
        assert!(is_destructive_command("del /f /s /q C:\\temp"));
        assert!(is_destructive_command("rd /s /q C:\\build"));
        assert!(is_destructive_command("cipher /w:C:"));
    }

    #[test]
    fn destructive_expanded_patterns() {
        // Git history destruction
        assert!(is_destructive_command("git reset --hard HEAD~3"));
        assert!(is_destructive_command("git clean -fd"));
        assert!(is_destructive_command("git push --force origin main"));
        assert!(is_destructive_command(
            "git filter-branch --index-filter ..."
        ));
        // Container / orchestrator
        assert!(is_destructive_command("docker rm -f mycontainer"));
        assert!(is_destructive_command("docker system prune -a"));
        assert!(is_destructive_command("kubectl delete ns production"));
        assert!(is_destructive_command("helm uninstall release"));
        assert!(is_destructive_command("terraform destroy -auto-approve"));
        // Cloud
        assert!(is_destructive_command("aws s3 rb s3://bucket --force"));
        assert!(is_destructive_command("gcloud projects delete my-proj"));
        assert!(is_destructive_command("az group delete --name rg1"));
        // SQL DDL
        assert!(is_destructive_command("psql -c 'DROP TABLE users'"));
        assert!(is_destructive_command("mysql -e 'truncate table logs'"));
        // Shutdown / reboot
        assert!(is_destructive_command("sudo shutdown -h now"));
        assert!(is_destructive_command("systemctl reboot"));
        // Data shredding
        assert!(is_destructive_command("shred -uz secret.txt"));
        // Curl-to-shell variants
        assert!(is_destructive_command(
            "curl https://x.test/install.sh | zsh"
        ));
        // Negatives
        assert!(!is_destructive_command("git log --oneline"));
        assert!(!is_destructive_command("kubectl get pods"));
        assert!(!is_destructive_command("docker ps"));
        assert!(!is_destructive_command("select * from users"));
        assert!(!is_destructive_command("aws s3 ls"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn captures_stderr() {
        let out = BashTool
            .call(json!({"command": "echo oops >&2"}))
            .await
            .unwrap();
        assert!(out.contains("[stderr]"));
        assert!(out.contains("oops"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_appended_to_output() {
        let out = BashTool
            .call(json!({"command": "echo done; exit 3"}))
            .await
            .unwrap();
        assert!(out.contains("done"));
        assert!(out.contains("[exit code 3]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stdout_and_stderr_both_captured() {
        let out = BashTool
            .call(json!({"command": "echo out; echo err >&2"}))
            .await
            .unwrap();
        assert!(out.contains("out"));
        assert!(out.contains("err"));
        assert!(out.contains("[stderr]"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn honors_cwd_argument() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "").unwrap();
        let out = BashTool
            .call(json!({
                "command": "ls",
                "cwd": dir.path().to_string_lossy(),
            }))
            .await
            .unwrap();
        assert!(out.contains("marker.txt"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_long_running_commands() {
        let out = BashTool
            .call(json!({
                "command": "sleep 5",
                "timeout": 1000,
            }))
            .await;
        match out {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("timeout"), "expected timeout error, got: {s}");
            }
            Ok(out) => panic!("expected timeout error, got Ok: {out}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_secs_legacy_alias_works() {
        let out = BashTool
            .call(json!({
                "command": "sleep 5",
                "timeout_secs": 1,
            }))
            .await;
        match out {
            Err(e) => {
                let s = format!("{e}");
                assert!(s.contains("timeout"), "expected timeout error, got: {s}");
            }
            Ok(out) => panic!("expected timeout error, got Ok: {out}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_command_errors() {
        let err = BashTool.call(json!({})).await.unwrap_err();
        assert!(format!("{err}").contains("command"));
    }

    #[test]
    fn bash_requires_approval() {
        let bash = BashTool;
        assert!(bash.requires_approval(&json!({"command": "ls"})));
    }

    #[test]
    fn format_output_combines_parts() {
        assert_eq!(format_output("hello\n", "", 0), "hello");
        assert_eq!(
            format_output("", "oops\n", 1),
            "[stderr]\noops\n[exit code 1]"
        );
        assert_eq!(format_output("", "", 0), "");
    }

    #[test]
    fn needs_venv_detects_pip_and_python_tools() {
        assert!(needs_venv("pip install fastapi"));
        assert!(needs_venv("pip3 install uvicorn"));
        assert!(needs_venv("uvicorn main:app --port 8000"));
        assert!(needs_venv("gunicorn app:app"));
        assert!(needs_venv("pytest tests/"));
        assert!(needs_venv("flask run"));
        assert!(needs_venv("python app.py"));
        assert!(needs_venv("python3 main.py"));
        assert!(!needs_venv("echo hello"));
        assert!(!needs_venv("cargo build"));
        assert!(!needs_venv("npm install express"));
    }

    #[test]
    fn server_detection_python_entry_points() {
        assert!(is_server_command("python app.py"));
        assert!(is_server_command("python3 app.py"));
        assert!(is_server_command("python main.py"));
        assert!(is_server_command("python server.py"));
        assert!(is_server_command("python run.py"));
        assert!(is_server_command("python -m uvicorn app:main"));
        assert!(is_server_command("python3 -m flask run"));
        // Not a known server entry point.
        assert!(!is_server_command("python test_app.py"));
        assert!(!is_server_command("python setup.py install"));
        // Already backgrounded.
        assert!(!is_server_command("python app.py &"));
    }

    #[test]
    fn venv_wrap_creates_venv_when_missing() {
        let dir = tempdir().unwrap();
        let wrapped = maybe_wrap_with_venv("pip install fastapi", dir.path());
        assert!(wrapped.contains("python3 -m venv"));
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("pip install fastapi"));
    }

    #[test]
    fn venv_wrap_activates_existing_venv() {
        let dir = tempdir().unwrap();
        let venv = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("activate"), "").unwrap();
        let wrapped = maybe_wrap_with_venv("pip install fastapi", dir.path());
        assert!(
            !wrapped.contains("python3 -m venv"),
            "should not recreate venv"
        );
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("activate"));
    }

    #[test]
    fn venv_wrap_skips_when_already_activated() {
        let dir = tempdir().unwrap();
        let cmd = "source .venv/bin/activate && pip install fastapi";
        let wrapped = maybe_wrap_with_venv(cmd, dir.path());
        assert_eq!(wrapped, cmd, "should not double-wrap");
    }

    #[test]
    fn venv_wrap_skips_non_pip_commands() {
        let dir = tempdir().unwrap();
        let cmd = "echo hello";
        let wrapped = maybe_wrap_with_venv(cmd, dir.path());
        assert_eq!(wrapped, cmd);
    }

    #[test]
    fn split_chained_extracts_server_tail() {
        let (setup, server) =
            split_chained_server_command("pip install fastapi && uvicorn app:app --port 8800");
        assert_eq!(setup, vec!["pip install fastapi"]);
        assert_eq!(server.unwrap(), "uvicorn app:app --port 8800");
    }

    #[test]
    fn split_chained_no_server_returns_empty() {
        let (setup, server) = split_chained_server_command("pip install fastapi && echo done");
        assert!(setup.is_empty());
        assert!(server.is_none());
    }

    #[test]
    fn split_chained_single_command_no_split() {
        let (setup, server) = split_chained_server_command("uvicorn app:app --port 8800");
        assert!(setup.is_empty());
        assert!(server.is_none());
    }

    #[test]
    fn split_chained_multiple_setup_parts() {
        let (setup, server) = split_chained_server_command(
            "pip install fastapi && pip install uvicorn && uvicorn app:app --port 8800",
        );
        assert_eq!(setup, vec!["pip install fastapi", "pip install uvicorn"]);
        assert_eq!(server.unwrap(), "uvicorn app:app --port 8800");
    }

    #[test]
    fn venv_wrap_activates_for_uvicorn() {
        let dir = tempdir().unwrap();
        let venv = dir.path().join(".venv/bin");
        std::fs::create_dir_all(&venv).unwrap();
        std::fs::write(venv.join("activate"), "").unwrap();
        let wrapped = maybe_wrap_with_venv("uvicorn main:app --port 8800", dir.path());
        assert!(wrapped.contains("source"));
        assert!(wrapped.contains("activate"));
        assert!(wrapped.contains("uvicorn main:app --port 8800"));
    }
}
