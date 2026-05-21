//! `thclaws deploy` client — builds a filtered tar of the current
//! project's `.thclaws/` and ships it to a pod's `/v1/deploy*`.
//!
//! Dev-plan/28 Phase 1: replace-all upload via `POST /v1/deploy`.
//! Phase 2: `POST /v1/deploy/manifest` first to identify what the pod
//! is missing, then ship only the diff via the same `/v1/deploy`
//! endpoint. Same auth, same SSE event shape.
//!
//! Filter rules (kept in sync with the server's allow / preserve
//! lists at `api_v1/deploy.rs`):
//!
//! - Top-level entries shipped: settings.json, mcp.json, AGENTS.md,
//!   agents/, skills/, commands/, plugins/, plugins.json, prompt/,
//!   rules/, kms/. memory/ added when --include-memory.
//! - Never shipped: sessions/, team/, .env (server would refuse
//!   anyway — defense in depth on both sides).
//! - mcp.json with stdio entries refused unless --allow-stdio-mcp
//!   (paths/binaries reference the laptop, won't resolve on the pod).

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

#[derive(Debug)]
pub struct DeployArgs {
    pub pod: String,
    pub token: Option<String>,
    pub include_memory: bool,
    pub allow_stdio_mcp: bool,
    pub dry_run: bool,
    pub full: bool,
}

const ALLOWED_TOP_LEVEL: &[&str] = &[
    "settings.json",
    "mcp.json",
    "AGENTS.md",
    "agents",
    "skills",
    "commands",
    "plugins",
    "plugins.json",
    "prompt",
    "rules",
    "kms",
];

const NEVER_SHIP: &[&str] = &["sessions", "team", ".env"];

/// Entry point invoked by the `thclaws deploy` subcommand. Returns the
/// process exit code.
pub async fn run(args: DeployArgs) -> i32 {
    let cwd = match std::env::current_dir() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] cannot resolve current dir: {e}\x1b[0m");
            return 1;
        }
    };
    let thclaws_root = cwd.join(".thclaws");
    if !thclaws_root.exists() {
        eprintln!(
            "\x1b[31m[deploy] no .thclaws/ in {}: run thclaws here first to create one\x1b[0m",
            cwd.display()
        );
        return 1;
    }

    // Collect candidate files.
    let candidates = match collect_files(&thclaws_root, args.include_memory) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] scan failed: {e}\x1b[0m");
            return 1;
        }
    };

    if candidates.is_empty() {
        eprintln!(
            "\x1b[33m[deploy] nothing to ship under .thclaws/ — bundle is empty\x1b[0m"
        );
        return 1;
    }

    // Validate stdio MCP if mcp.json is included.
    if !args.allow_stdio_mcp {
        if let Some(rel) = candidates.keys().find(|k| k.as_str() == "mcp.json") {
            if let Err(e) = scan_mcp_json(&thclaws_root.join(rel)) {
                eprintln!("\x1b[31m[deploy] {e}\x1b[0m");
                eprintln!(
                    "\x1b[33m  use --allow-stdio-mcp to skip this check (entries will fail on the pod)\x1b[0m"
                );
                return 1;
            }
        }
    }

    if args.dry_run {
        let total_bytes: u64 = candidates.values().map(|m| m.size).sum();
        println!(
            "[deploy] dry run — would ship {} file(s), {} bytes:",
            candidates.len(),
            total_bytes
        );
        for (path, meta) in &candidates {
            println!("  {} ({} bytes)", path, meta.size);
        }
        return 0;
    }

    let token = args
        .token
        .or_else(|| std::env::var("THCLAWS_DEPLOY_TOKEN").ok())
        .filter(|s| !s.trim().is_empty());
    let Some(token) = token else {
        eprintln!(
            "\x1b[31m[deploy] no token: pass --token <BEARER> or set THCLAWS_DEPLOY_TOKEN\x1b[0m"
        );
        return 1;
    };

    let base_url = args.pod.trim_end_matches('/').to_string();
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] reqwest build failed: {e}\x1b[0m");
            return 1;
        }
    };

    // Phase-2 diff handshake unless --full.
    let to_ship: Vec<String> = if args.full {
        candidates.keys().cloned().collect()
    } else {
        match diff_manifest(&client, &base_url, &token, &candidates).await {
            Ok(missing) => {
                println!(
                    "[deploy] diff: pod is missing {}/{} file(s)",
                    missing.len(),
                    candidates.len()
                );
                if missing.is_empty() {
                    println!("[deploy] pod is already up to date — nothing to ship");
                    return 0;
                }
                missing
            }
            Err(e) => {
                eprintln!(
                    "\x1b[33m[deploy] manifest handshake failed ({e}); falling back to full upload\x1b[0m"
                );
                candidates.keys().cloned().collect()
            }
        }
    };

    // Build the tar with only the to_ship subset.
    let tar_bytes = match build_tar(&thclaws_root, &to_ship) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] tar build failed: {e}\x1b[0m");
            return 1;
        }
    };
    println!(
        "[deploy] bundled {} file(s), {} bytes — uploading to {}",
        to_ship.len(),
        tar_bytes.len(),
        base_url
    );

    // POST /v1/deploy and stream SSE progress.
    let url = format!("{base_url}/v1/deploy");
    let resp = client
        .post(&url)
        .bearer_auth(&token)
        .header("content-type", "application/x-tar")
        .body(tar_bytes)
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] upload failed: {e}\x1b[0m");
            return 1;
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        eprintln!(
            "\x1b[31m[deploy] pod rejected upload: HTTP {status}: {}\x1b[0m",
            body.chars().take(500).collect::<String>()
        );
        return 1;
    }

    // Read SSE events. Body is one block once SSE stream ends; for
    // Phase 1 we render after the fact rather than streaming live —
    // the upload itself is the slow part, the server side is < 5 s
    // for a typical bundle.
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("\x1b[31m[deploy] read SSE body failed: {e}\x1b[0m");
            return 1;
        }
    };
    render_sse(&text);
    0
}

struct FileMeta {
    size: u64,
    sha256: String,
}

fn collect_files(
    thclaws_root: &Path,
    include_memory: bool,
) -> std::io::Result<BTreeMap<String, FileMeta>> {
    let mut out: BTreeMap<String, FileMeta> = BTreeMap::new();
    for top in ALLOWED_TOP_LEVEL.iter().chain(if include_memory {
        ["memory"].iter()
    } else {
        [].iter()
    }) {
        let path = thclaws_root.join(top);
        if !path.exists() {
            continue;
        }
        if path.is_file() {
            let rel = (*top).to_string();
            if NEVER_SHIP.contains(top) {
                continue;
            }
            insert_file(&mut out, &rel, &path)?;
        } else if path.is_dir() {
            for entry in walkdir::WalkDir::new(&path) {
                let entry = entry.map_err(std::io::Error::other)?;
                if !entry.file_type().is_file() {
                    continue;
                }
                let abs = entry.path();
                let rel = abs
                    .strip_prefix(thclaws_root)
                    .map_err(std::io::Error::other)?
                    .to_string_lossy()
                    .replace('\\', "/");
                // Defense in depth: skip never-ship even if walked from
                // an allowed parent (won't happen with current consts
                // but cheap).
                let first = rel.split('/').next().unwrap_or("");
                if NEVER_SHIP.contains(&first) {
                    continue;
                }
                insert_file(&mut out, &rel, abs)?;
            }
        }
    }
    Ok(out)
}

fn insert_file(
    out: &mut BTreeMap<String, FileMeta>,
    rel: &str,
    abs: &Path,
) -> std::io::Result<()> {
    let bytes = std::fs::read(abs)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    out.insert(
        rel.to_string(),
        FileMeta {
            size: bytes.len() as u64,
            sha256: format!("{:x}", h.finalize()),
        },
    );
    Ok(())
}

fn scan_mcp_json(path: &Path) -> Result<(), String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read mcp.json: {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parse mcp.json: {e}"))?;
    let Some(servers) = v.get("mcpServers").and_then(|s| s.as_object()) else {
        return Ok(());
    };
    let stdio: Vec<String> = servers
        .iter()
        .filter(|(_, cfg)| {
            cfg.get("transport")
                .and_then(|t| t.as_str())
                .map(|t| t == "stdio")
                .unwrap_or(true)
        })
        .map(|(name, _)| name.clone())
        .collect();
    if !stdio.is_empty() {
        return Err(format!(
            "mcp.json contains stdio MCP servers that won't resolve on the pod: {} \
             (each spawns a local binary)",
            stdio.join(", ")
        ));
    }
    Ok(())
}

async fn diff_manifest(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    candidates: &BTreeMap<String, FileMeta>,
) -> Result<Vec<String>, String> {
    let files: Vec<serde_json::Value> = candidates
        .iter()
        .map(|(path, meta)| {
            serde_json::json!({
                "path": path,
                "sha256": meta.sha256,
            })
        })
        .collect();
    let url = format!("{base_url}/v1/deploy/manifest");
    let resp = client
        .post(&url)
        .bearer_auth(token)
        .json(&serde_json::json!({ "files": files }))
        .send()
        .await
        .map_err(|e| format!("send: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "HTTP {}: {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        ));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("decode: {e}"))?;
    let missing = body
        .get("missing")
        .and_then(|m| m.as_array())
        .ok_or("manifest response missing `missing` array")?
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    Ok(missing)
}

fn build_tar(thclaws_root: &Path, paths: &[String]) -> std::io::Result<Vec<u8>> {
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    {
        let mut builder = tar::Builder::new(&mut buf);
        for rel in paths {
            let abs = thclaws_root.join(rel);
            if !abs.is_file() {
                continue;
            }
            let mut f = std::fs::File::open(&abs)?;
            let mut header = tar::Header::new_gnu();
            let metadata = f.metadata()?;
            header.set_size(metadata.len());
            header.set_mode(0o644);
            header.set_mtime(
                metadata
                    .modified()
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            );
            header.set_cksum();
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            f.read_to_end(&mut bytes)?;
            builder.append_data(&mut header, rel, bytes.as_slice())?;
        }
        builder.finish()?;
    }
    Ok(buf)
}

fn render_sse(text: &str) {
    // Minimal SSE parser — `event:` and `data:` lines, blank line
    // separates events. We just print one summary line per event so
    // the operator sees progression without raw SSE noise.
    let mut event: Option<String> = None;
    let mut data: Option<String> = None;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            data = Some(rest.trim().to_string());
        } else if line.trim().is_empty() {
            if let (Some(e), Some(d)) = (event.take(), data.take()) {
                let summary = summarize(&d);
                if e == "error" {
                    eprintln!("\x1b[31m[deploy] {e}: {summary}\x1b[0m");
                    let _ = std::io::stderr().flush();
                } else {
                    println!("[deploy] {e}: {summary}");
                }
            }
        }
    }
    // Flush trailing event if the stream didn't end on a blank line.
    if let (Some(e), Some(d)) = (event, data) {
        println!("[deploy] {e}: {}", summarize(&d));
    }
}

fn summarize(json: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return json.to_string();
    };
    if let Some(obj) = v.as_object() {
        let parts: Vec<String> = obj
            .iter()
            .filter(|(_, val)| !val.is_null())
            .map(|(k, val)| format!("{k}={val}"))
            .collect();
        return parts.join(" ");
    }
    json.to_string()
}
