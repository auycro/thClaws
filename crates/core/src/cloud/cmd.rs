//! CLI subcommand handlers — wired from `bin/app.rs`.

use std::path::{Path, PathBuf};

use crate::cloud::{client::Client, pack, resolve_cloud_url, CloudConfig};

pub async fn login(
    cloud_url: Option<&str>,
    token: Option<String>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = match token {
        Some(t) => t.trim().to_string(),
        None => prompt_token()?,
    };
    if !token.starts_with("thc_") {
        return Err(
            "expected token to start with 'thc_' — get one from the dashboard at /dashboard".into(),
        );
    }
    let client = Client::new(&url, Some(token.clone()));
    let me = client.me().await?;
    crate::cloud::set_token(&token).map_err(|e| format!("save token: {}", e))?;
    // Persist the URL too so subsequent CLI calls don't need --cloud-url.
    // Same field the GUI Settings → Cloud section writes.
    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    project.set_cloud_url(Some(&url));
    if let Err(e) = project.save() {
        eprintln!("  warning: couldn't persist URL to settings.json: {}", e);
    }
    eprintln!("✓ Signed in to {} as {}", url, me.email);
    if me.can_publish {
        eprintln!("  Publishing enabled.");
    } else {
        eprintln!("  Publishing not enabled for this account.");
    }
    Ok(())
}

fn prompt_token() -> Result<String, String> {
    use std::io::{BufRead, Write};
    eprint!("Paste CLI token (from /dashboard): ");
    std::io::stderr().flush().ok();
    let stdin = std::io::stdin();
    let mut line = String::new();
    stdin
        .lock()
        .read_line(&mut line)
        .map_err(|e| format!("stdin: {}", e))?;
    let t = line.trim().to_string();
    if t.is_empty() {
        return Err("no token entered".into());
    }
    Ok(t)
}

pub fn logout() -> Result<(), String> {
    crate::cloud::clear_token().map_err(|e| format!("clear token: {}", e))?;
    eprintln!("✓ Signed out");
    Ok(())
}

/// Print where the CLI currently thinks the catalog lives and whether
/// it has credentials. Mirrors the Settings → Cloud panel so users can
/// confirm CLI-side state without opening the GUI.
pub fn status(cloud_url: Option<&str>, cloud_cfg: Option<&CloudConfig>) -> Result<(), String> {
    for line in status_lines(cloud_url, cloud_cfg) {
        eprintln!("{line}");
    }
    Ok(())
}

/// Return the same lines `status()` prints, but as a `Vec<String>` so
/// the REPL / GUI slash-command dispatchers can route them through
/// their own output channels (`println!` vs `ViewEvent::SlashOutput`).
pub fn status_lines(cloud_url: Option<&str>, cloud_cfg: Option<&CloudConfig>) -> Vec<String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let has_token = crate::cloud::token().is_some();
    let agent = crate::config::ProjectConfig::load().and_then(|c| c.agent.clone());
    let mut lines = vec![
        format!("Cloud URL: {url}"),
        format!("Token:     {}", if has_token { "set" } else { "(none)" }),
    ];
    match agent {
        Some(a) => {
            lines.push(format!(
                "Agent:     {} ({})",
                a.name.as_deref().unwrap_or("(unnamed)"),
                a.id.as_deref().unwrap_or("?")
            ));
            lines.push(format!(
                "UUID:      {}",
                a.uuid
                    .as_deref()
                    .map(|u| format!("{u} (bound)"))
                    .unwrap_or_else(|| "(unbound — next publish creates new entry)".to_string())
            ));
        }
        None => {
            lines.push("Agent:     (no settings.json::agent block in this folder)".to_string());
        }
    }
    lines
}

/// Hit the catalog and return the lines the slash dispatchers (REPL +
/// GUI) print. Errors surface as a single line so both surfaces render
/// identically.
pub async fn list_lines(
    mine: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    let client = Client::new(&url, token);
    match client.list_agents(mine).await {
        Ok(agents) if agents.is_empty() => vec!["(no agents in catalog)".to_string()],
        Ok(agents) => agents
            .into_iter()
            .map(|a| {
                format!(
                    "{:30}  v{:<10}  {}",
                    a.slug,
                    a.current_version.unwrap_or_default(),
                    a.name
                )
            })
            .collect(),
        Err(e) => vec![format!("/cloud list: {e}")],
    }
}

/// `/cloud publish` from inside a session — packs cwd + uploads.
/// Returns ordered progress lines (including any error). Mirrors
/// [`get_into_cwd_lines`].
pub async fn publish_cwd_lines(
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let cwd = match std::env::current_dir() {
        Ok(c) => c,
        Err(e) => return vec![format!("/cloud publish: can't read cwd: {e}")],
    };
    let mut lines: Vec<String> = Vec::new();
    if let Err(e) = publish_inner(cwd, cloud_url, false, cloud_cfg, &mut lines).await {
        lines.push(format!("/cloud publish: {e}"));
    }
    lines
}

pub async fn publish(
    path: PathBuf,
    cloud_url: Option<&str>,
    dry_run: bool,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    // CLI-facing thin wrapper: mirror the slash-friendly inner impl
    // and dump its lines to stderr so terminal output matches the old
    // eprintln shape exactly.
    let mut lines = Vec::new();
    let result = publish_inner(path, cloud_url, dry_run, cloud_cfg, &mut lines).await;
    for ln in &lines {
        eprintln!("{ln}");
    }
    result
}

async fn publish_inner(
    path: PathBuf,
    cloud_url: Option<&str>,
    dry_run: bool,
    cloud_cfg: Option<&CloudConfig>,
    log: &mut Vec<String>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();

    // Load this folder's project settings so we can read agent identity
    // + write the UUID back after the server assigns one. We deliberately
    // chdir-style here: ProjectConfig::load reads from cwd, so we cd
    // into the agent folder for the duration of the call. (We restore
    // cwd at the end so the caller's environment is unchanged.)
    let prior_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(&path).map_err(|e| format!("entering {}: {}", path.display(), e))?;
    let _restore = scopeguard_chdir(prior_cwd);

    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    let agent = ensure_agent_identity(&mut project, &path)?;

    let fused =
        crate::cloud::manifest::Manifest::fuse_for_publish(&agent, &path.join("manifest.json"))?;
    let fused_json =
        serde_json::to_vec_pretty(&fused).map_err(|e| format!("serialize fused manifest: {e}"))?;

    log.push(format!("Packing {} …", path.display()));
    let result = pack::pack(&path, Some(&fused_json))?;
    log.push(format!(
        "  Included {} file(s), stripped {} file(s), {:.1} KB",
        result.included.len(),
        result.stripped.len(),
        result.bytes.len() as f64 / 1024.0
    ));
    if !result.stripped.is_empty() {
        log.push("  Stripped (showing first 10):".to_string());
        for s in result.stripped.iter().take(10) {
            log.push(format!("    - {}", s));
        }
    }
    if agent.uuid.is_some() {
        log.push(format!(
            "  Publishing as existing agent (uuid: {}…)",
            &agent
                .uuid
                .as_deref()
                .unwrap_or("")
                .chars()
                .take(8)
                .collect::<String>()
        ));
    } else {
        log.push("  First publish — server will assign a UUID.".to_string());
    }
    if dry_run {
        log.push("Dry run — not uploading.".to_string());
        return Ok(());
    }

    if token.is_none() {
        return Err(
            "not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)".into()
        );
    }

    log.push(format!("Uploading to {} …", url));
    let client = Client::new(&url, token);
    let resp = client.publish(result.bytes).await?;
    log.push(format!(
        "✓ Published {} v{} ({} bytes)",
        resp.slug, resp.version, resp.size_bytes
    ));
    log.push(format!("  {}", resp.url));

    // Write the assigned UUID back to settings.json so re-publish from
    // this folder targets the same catalog entry.
    if agent.uuid.as_deref() != Some(resp.uuid.as_str()) {
        project.merge_agent(crate::config::AgentConfig {
            uuid: Some(resp.uuid.clone()),
            ..Default::default()
        });
        project
            .save()
            .map_err(|e| format!("write settings.json: {e}"))?;
        log.push(format!(
            "  settings.json::agent.uuid updated → {}…",
            resp.uuid.chars().take(8).collect::<String>()
        ));
    }
    Ok(())
}

/// Settings-or-manifest helper: pull the agent identity to use for
/// `cloud publish`. If `settings.json::agent` is populated, use it. If
/// it's missing but the legacy `manifest.json` carries id/name/
/// description (pre-Option-A folders), auto-migrate to settings.json
/// and emit a one-line notice. Returns the resolved identity.
fn ensure_agent_identity(
    project: &mut crate::config::ProjectConfig,
    folder: &Path,
) -> Result<crate::config::AgentConfig, String> {
    if let Some(existing) = project.agent.as_ref() {
        if existing.id.is_some() && existing.name.is_some() && existing.description.is_some() {
            return Ok(existing.clone());
        }
    }

    // Try to read identity from legacy manifest.json.
    let manifest_path = folder.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path).map_err(|e| {
        format!(
            "no settings.json::agent block and can't read manifest.json: {e}\n\
             — add an [agent] section to ./.thclaws/settings.json with id/name/description"
        )
    })?;
    let v: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| format!("manifest.json: {e}"))?;
    let id = v.get("id").and_then(|x| x.as_str()).map(String::from);
    let name = v.get("name").and_then(|x| x.as_str()).map(String::from);
    let description = v
        .get("description")
        .and_then(|x| x.as_str())
        .map(String::from);
    if id.is_none() || name.is_none() || description.is_none() {
        return Err(
            "settings.json::agent.{id,name,description} required for publish (none of these \
             could be derived from manifest.json either)"
                .into(),
        );
    }
    eprintln!("  Migrating identity from manifest.json → settings.json::agent");
    project.merge_agent(crate::config::AgentConfig {
        id,
        name,
        description,
        uuid: None,
    });
    project
        .save()
        .map_err(|e| format!("write settings.json: {e}"))?;
    Ok(project.agent.clone().unwrap())
}

/// Restore cwd on drop. Used by `publish` so the caller's environment
/// is unchanged after the publish call returns.
fn scopeguard_chdir(prior: Option<PathBuf>) -> impl Drop {
    struct Guard(Option<PathBuf>);
    impl Drop for Guard {
        fn drop(&mut self) {
            if let Some(p) = self.0.take() {
                let _ = std::env::set_current_dir(p);
            }
        }
    }
    Guard(prior)
}

pub fn unbind() -> Result<(), String> {
    for ln in unbind_lines() {
        eprintln!("{ln}");
    }
    Ok(())
}

/// `/cloud unbind` from inside a session. Same logic as [`unbind`]
/// but returns lines for the SlashOutput stream instead of eprintln.
pub fn unbind_lines() -> Vec<String> {
    let mut project = crate::config::ProjectConfig::load().unwrap_or_default();
    let prior = project
        .agent
        .as_ref()
        .and_then(|a| a.uuid.clone())
        .unwrap_or_default();
    if prior.is_empty() {
        return vec!["Already unbound (no settings.json::agent.uuid).".to_string()];
    }
    project.clear_agent_uuid();
    if let Err(e) = project.save() {
        return vec![format!("/cloud unbind: write settings.json: {e}")];
    }
    vec![format!(
        "✓ Cleared agent UUID ({}…). Next /cloud publish will create a new catalog entry.",
        prior.chars().take(8).collect::<String>()
    )]
}

pub async fn get(
    slug: String,
    target: PathBuf,
    version: Option<String>,
    force: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    for line in get_lines(slug, target, version, force, cloud_url, cloud_cfg).await {
        eprintln!("{line}");
    }
    Ok(())
}

/// `cloud get <slug>` into the caller's cwd with the safety check the
/// slash command needs:
///   - empty cwd → extract fresh
///   - non-empty cwd + matching agent UUID → extract over (safe update)
///   - non-empty cwd + UUID mismatch or no UUID → abort
///
/// No `--force` — for that, use the CLI's `cloud get <slug> <target> --force`.
pub async fn get_into_cwd_lines(
    slug: String,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => return vec![format!("/cloud get: can't read cwd: {e}")],
    };
    get_lines(slug, cwd, None, /*force=*/ false, cloud_url, cloud_cfg).await
}

/// Underlying get-and-report. Errors come back as a single line so
/// both surfaces (CLI eprintln, GUI/REPL SlashOutput) render identically.
/// `force` bypasses the UUID-match safety check on non-empty targets.
async fn get_lines(
    slug: String,
    target: PathBuf,
    version: Option<String>,
    force: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Vec<String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    if token.is_none() {
        return vec![
            "/cloud get: not logged in — paste your CLI token in Settings → thClaws.cloud (mint one at /dashboard)"
                .to_string(),
        ];
    }

    let mut lines = Vec::new();
    // "Has agent content" — not just "is non-empty". REPL startup
    // auto-bootstraps a placeholder .thclaws/settings.json in cwd
    // (via ProjectConfig::ensure_default_exists), which would make
    // a genuinely-fresh folder look non-empty. The real signal that
    // an agent already lives here is AGENTS.md or manifest.json.
    let has_agent_content =
        target.join("AGENTS.md").exists() || target.join("manifest.json").exists();

    lines.push(format!("Downloading {} …", slug));
    let client = Client::new(&url, token);
    let dl = match client.download(&slug, version.as_deref()).await {
        Ok(d) => d,
        Err(e) => {
            lines.push(format!("/cloud get: {e}"));
            return lines;
        }
    };
    lines.push(format!(
        "  v{} ({:.1} KB, sha256 {}…)",
        dl.version,
        dl.bytes.len() as f64 / 1024.0,
        &dl.sha256.chars().take(12).collect::<String>()
    ));

    if !dl.sha256.is_empty() {
        if let Err(e) = pack::verify_sha256(&dl.bytes, &dl.sha256) {
            lines.push(format!("/cloud get: {e}"));
            return lines;
        }
    }

    // Safety check on folders that already hold an agent: refuse unless
    // the bound agent UUID matches what we just downloaded. `--force`
    // (CLI-only) bypasses.
    if has_agent_content && !force {
        let server_uuid = match dl.uuid.as_deref() {
            Some(u) if !u.is_empty() => u.to_string(),
            _ => {
                lines.push(
                    "/cloud get: server didn't return an X-Agent-UUID header — refusing to \
                     overwrite an existing agent folder. (Catalog backend probably needs an update.)"
                        .into(),
                );
                return lines;
            }
        };
        let local_uuid = load_local_agent_uuid(&target);
        match local_uuid.as_deref() {
            Some(local) if local == server_uuid => {
                lines.push(format!(
                    "  Folder matches agent UUID {}… — overwriting in-place.",
                    server_uuid.chars().take(8).collect::<String>()
                ));
            }
            Some(local) => {
                lines.push(format!(
                    "/cloud get: refusing to overwrite. This folder is bound to agent {}…, but \
                     the downloaded agent is {}…. To replace this folder with the downloaded \
                     agent, run /cloud unbind first OR cd to an empty directory.",
                    local.chars().take(8).collect::<String>(),
                    server_uuid.chars().take(8).collect::<String>()
                ));
                return lines;
            }
            None => {
                lines.push(
                    "/cloud get: refusing to overwrite. This folder has agent content \
                     (AGENTS.md / manifest.json) but no bound UUID in .thclaws/settings.json. \
                     Cd to an empty directory and run /cloud get again."
                        .into(),
                );
                return lines;
            }
        }
    }

    lines.push(format!("Extracting to {} …", target.display()));
    // After the UUID match (or empty target, or --force) we always
    // allow overwrite — pack::unpack's per-file refusal is bypassed
    // because the safety gate already lives above.
    let files = match pack::unpack(&dl.bytes, &target, /*force=*/ true) {
        Ok(f) => f,
        Err(e) => {
            lines.push(format!("/cloud get: {e}"));
            return lines;
        }
    };
    lines.push(format!("✓ Extracted {} file(s)", files.len()));

    let manifest_path = target.join("manifest.json");
    if let Ok(m) = crate::cloud::manifest::Manifest::from_path(&manifest_path) {
        if let Err(e) = split_unified_manifest(&target, &m, dl.uuid.as_deref()) {
            lines.push(format!(
                "  warning: couldn't split manifest into settings.json: {e}"
            ));
        }
        for line in post_install_hint_lines(&m, &target) {
            lines.push(line);
        }
    }
    lines
}

/// Read just `<target>/.thclaws/settings.json::agent.uuid` without
/// touching the rest of project config.
fn load_local_agent_uuid(target: &Path) -> Option<String> {
    let prior = std::env::current_dir().ok();
    if std::env::set_current_dir(target).is_err() {
        return None;
    }
    let uuid = crate::config::ProjectConfig::load()
        .and_then(|c| c.agent)
        .and_then(|a| a.uuid);
    if let Some(p) = prior {
        let _ = std::env::set_current_dir(p);
    }
    uuid
}

fn split_unified_manifest(
    target: &Path,
    manifest: &crate::cloud::manifest::Manifest,
    server_uuid: Option<&str>,
) -> Result<(), String> {
    let prior_cwd = std::env::current_dir().ok();
    std::env::set_current_dir(target)
        .map_err(|e| format!("entering {}: {}", target.display(), e))?;
    let _restore = scopeguard_chdir(prior_cwd);

    // Identity AND UUID travel with the package. Preserving the UUID
    // makes "re-get into the same folder" act as an update (same agent
    // → CLI overwrites in place). Fork-safety is enforced server-side:
    // if the recipient tries to `cloud publish`, the server checks
    // UUID ownership and 403s with a clear "run cloud unbind to fork".
    // UUID precedence: server X-Agent-UUID header (authoritative) >
    // manifest.uuid inside the tarball (may be stale or absent).
    let resolved_uuid = server_uuid
        .map(|s| s.to_string())
        .or_else(|| manifest.uuid.clone());
    let agent_block = serde_json::json!({
        "id": manifest.id,
        "name": manifest.name,
        "description": manifest.description,
        "uuid": resolved_uuid,
    });
    // Direct JSON-level merge — set just the `agent` key, preserve
    // everything else the tarball shipped (guiShell, model, etc.).
    // Going through ProjectConfig::save() would also write every
    // Option<bool> default-false field (shellTabEnabled, teamEnabled,
    // …), bloating the installer's settings.json with noise.
    let settings_path = std::path::Path::new(".thclaws").join("settings.json");
    let mut existing: serde_json::Value = match std::fs::read(&settings_path) {
        Ok(raw) => serde_json::from_slice(&raw).unwrap_or_else(|_| serde_json::json!({})),
        Err(_) => serde_json::json!({}),
    };
    if let Some(obj) = existing.as_object_mut() {
        obj.insert("agent".to_string(), agent_block);
    }
    if let Some(parent) = settings_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    std::fs::write(
        &settings_path,
        serde_json::to_string_pretty(&existing)
            .map_err(|e| format!("serialize settings.json: {e}"))?,
    )
    .map_err(|e| format!("write settings.json: {e}"))?;

    // Strip identity fields from the on-disk manifest.json so the local
    // source of truth is unambiguous (settings.json::agent).
    let manifest_path = target.join("manifest.json");
    let raw = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("read {}: {e}", manifest_path.display()))?;
    let mut v: serde_json::Value = serde_json::from_str(&raw)
        .map_err(|e| format!("parse {}: {e}", manifest_path.display()))?;
    if let Some(obj) = v.as_object_mut() {
        for k in ["id", "name", "description", "uuid", "author"] {
            obj.remove(k);
        }
    }
    let slim =
        serde_json::to_string_pretty(&v).map_err(|e| format!("serialize slim manifest: {e}"))?;
    std::fs::write(&manifest_path, slim)
        .map_err(|e| format!("write {}: {e}", manifest_path.display()))?;
    Ok(())
}

pub async fn list(
    mine: bool,
    cloud_url: Option<&str>,
    cloud_cfg: Option<&CloudConfig>,
) -> Result<(), String> {
    let url = resolve_cloud_url(cloud_url, cloud_cfg);
    let token = crate::cloud::token();
    let client = Client::new(&url, token);
    let agents = client.list_agents(mine).await?;
    if agents.is_empty() {
        eprintln!("(no agents)");
        return Ok(());
    }
    for a in agents {
        println!(
            "{:30}  v{:<10}  {}",
            a.slug,
            a.current_version.unwrap_or_default(),
            a.name
        );
    }
    Ok(())
}

fn post_install_hint_lines(m: &crate::cloud::manifest::Manifest, target: &Path) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!("Installed: {} v{}", m.name, m.version),
        format!("  cd {}", target.display()),
    ];
    if !m.requires.provider_keys.is_empty() {
        lines.push(String::new());
        lines.push("This agent expects these provider keys in .env:".to_string());
        for k in &m.requires.provider_keys {
            let mark = if k.required { "*" } else { " " };
            let purpose = k.purpose.as_deref().unwrap_or("");
            lines.push(format!(
                "  {} {}={}",
                mark,
                k.name,
                if purpose.is_empty() {
                    "<your-key>"
                } else {
                    purpose
                }
            ));
        }
    }
    if !m.requires.mcp_servers.is_empty() {
        lines.push(String::new());
        lines.push("Declared MCP servers (configured in .thclaws/mcp.json):".to_string());
        for s in &m.requires.mcp_servers {
            lines.push(format!("  - {s}"));
        }
    }
    lines.push(String::new());
    lines.push("Next: `thclaws` (CLI) or `thclaws --gui` (desktop).".to_string());
    lines
}
