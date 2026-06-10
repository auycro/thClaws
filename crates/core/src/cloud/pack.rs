//! Agent package tar pack/unpack.
//!
//! `pack(folder)` produces a gzipped tarball with strip rules applied
//! (sessions, secrets, build artefacts dropped). `unpack(bytes, target)`
//! is the reverse — extracts into a target directory, refusing
//! path-traversal entries.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use flate2::{read::GzDecoder, write::GzEncoder, Compression};
use sha2::{Digest, Sha256};

pub const STRIP_PREFIXES: &[&str] = &[
    ".thclaws/sessions/",
    ".thclaws/kms/data/",
    ".thclaws/cache/",
    ".git/",
    "node_modules/",
    "target/",
    "__pycache__/",
    ".venv/",
    ".next/",
    "dist/",
    "build/",
];

pub const STRIP_SUFFIXES: &[&str] = &[".env", ".key", ".pyc", ".log"];

pub fn is_strippable(rel: &Path) -> bool {
    let s = rel.to_string_lossy();
    let s = s.trim_start_matches("./");
    if STRIP_PREFIXES.iter().any(|p| s.starts_with(p)) {
        return true;
    }
    if STRIP_SUFFIXES.iter().any(|sx| s.ends_with(sx)) {
        return true;
    }
    if s.to_lowercase().contains("_secret") {
        return true;
    }
    false
}

pub struct PackResult {
    pub bytes: Vec<u8>,
    pub sha256: String,
    pub included: Vec<String>,
    pub stripped: Vec<String>,
}

/// Pack `folder` into a gzipped tarball. Strips files per the rules.
/// Requires `folder` to contain `AGENTS.md` at its root.
///
/// When `manifest_override` is `Some(bytes)`, that JSON blob is written
/// to the tarball as `manifest.json` instead of whatever exists on disk
/// — used by `cloud publish` to ship the fused identity-plus-catalog
/// manifest while keeping the local `manifest.json` slim (no identity
/// fields) per dev-plan/34 Option A. When `None`, the on-disk
/// `manifest.json` is tarred verbatim (required).
pub fn pack(folder: &Path, manifest_override: Option<&[u8]>) -> Result<PackResult, String> {
    let folder = folder
        .canonicalize()
        .map_err(|e| format!("canonicalize {}: {}", folder.display(), e))?;
    if !folder.is_dir() {
        return Err(format!("{} is not a directory", folder.display()));
    }
    if !folder.join("AGENTS.md").exists() {
        return Err("missing AGENTS.md in folder".into());
    }
    if manifest_override.is_none() && !folder.join("manifest.json").exists() {
        return Err("missing manifest.json in folder".into());
    }

    let mut included = Vec::new();
    let mut stripped = Vec::new();
    let buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(buf, Compression::default());
    let mut tar = tar::Builder::new(enc);
    tar.follow_symlinks(false);

    for entry in walkdir::WalkDir::new(&folder)
        .min_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        let rel = path.strip_prefix(&folder).unwrap();
        let rel_str = rel.to_string_lossy().replace('\\', "/");

        if is_strippable(rel) {
            stripped.push(rel_str);
            continue;
        }
        if !entry.file_type().is_file() {
            continue;
        }

        // When an override is supplied, skip the on-disk manifest.json
        // — we'll append the synthesized version below.
        if manifest_override.is_some() && rel == std::path::Path::new("manifest.json") {
            continue;
        }

        // `.thclaws/settings.json` carries publisher-only fields
        // (the `agent` block — id/name/description/uuid — that binds
        // this folder to its catalog row). The installer's workspace
        // shouldn't inherit that binding, or a subsequent `cloud
        // publish` from their copy would silently clobber the
        // original. Strip the `agent` block before packing; everything
        // else (guiShell, model, etc.) is user-facing config and
        // stays.
        if rel == std::path::Path::new(".thclaws/settings.json") {
            let raw = std::fs::read(path).map_err(|e| format!("read settings.json: {e}"))?;
            let cleaned = strip_publisher_fields(&raw)?;
            let mut header = tar::Header::new_gnu();
            header.set_size(cleaned.len() as u64);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(&mut header, rel, cleaned.as_slice())
                .map_err(|e| format!("tar append settings.json: {e}"))?;
            included.push(rel_str);
            continue;
        }

        let metadata = entry
            .metadata()
            .map_err(|e| format!("stat {}: {}", path.display(), e))?;
        let mut f =
            std::fs::File::open(path).map_err(|e| format!("open {}: {}", path.display(), e))?;
        let mut header = tar::Header::new_gnu();
        header.set_size(metadata.len());
        header.set_mode(0o644);
        header.set_mtime(
            metadata
                .modified()
                .ok()
                .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        );
        header.set_cksum();

        tar.append_data(&mut header, rel, &mut f)
            .map_err(|e| format!("tar append {}: {}", path.display(), e))?;
        included.push(rel_str);
    }

    if let Some(override_bytes) = manifest_override {
        let mut header = tar::Header::new_gnu();
        header.set_size(override_bytes.len() as u64);
        header.set_mode(0o644);
        header.set_mtime(0);
        header.set_cksum();
        tar.append_data(
            &mut header,
            std::path::Path::new("manifest.json"),
            override_bytes,
        )
        .map_err(|e| format!("tar append manifest override: {}", e))?;
        included.push("manifest.json".to_string());
    }

    let enc = tar.into_inner().map_err(|e| format!("tar finish: {}", e))?;
    let bytes = enc.finish().map_err(|e| format!("gzip finish: {}", e))?;
    let sha = Sha256::digest(&bytes);
    Ok(PackResult {
        bytes,
        sha256: hex_encode(&sha),
        included,
        stripped,
    })
}

/// Remove publisher-only fields from a packed `.thclaws/settings.json`.
/// Currently drops the `agent` block (publish-binding identity that
/// shouldn't carry over to installers). Falls back to passing the
/// original bytes through unchanged if the file isn't valid JSON
/// (don't fail the whole publish over a settings parse error).
fn strip_publisher_fields(raw: &[u8]) -> Result<Vec<u8>, String> {
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(raw) else {
        return Ok(raw.to_vec());
    };
    if let Some(obj) = v.as_object_mut() {
        obj.remove("agent");
    }
    serde_json::to_vec_pretty(&v).map_err(|e| format!("re-serialize settings.json: {e}"))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Peek at the agent UUID inside a gzipped tarball's manifest.json
/// without unpacking. Used by `cloud get` to safety-check before
/// overwriting an existing folder.
pub fn peek_manifest_uuid(bytes: &[u8]) -> Result<Option<String>, String> {
    use std::io::Read;
    let dec = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {e}"))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {e}"))?;
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {e}"))?
            .into_owned();
        if path == Path::new("manifest.json") {
            let mut content = String::new();
            entry
                .read_to_string(&mut content)
                .map_err(|e| format!("read manifest.json: {e}"))?;
            let v: serde_json::Value =
                serde_json::from_str(&content).map_err(|e| format!("parse manifest.json: {e}"))?;
            return Ok(v
                .get("uuid")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string()));
        }
    }
    Err("manifest.json not found in tarball".into())
}

/// Verify gzipped tarball matches `expected_sha256` (hex).
pub fn verify_sha256(bytes: &[u8], expected_sha256: &str) -> Result<(), String> {
    let actual = hex_encode(&Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected_sha256) {
        Ok(())
    } else {
        Err(format!(
            "sha256 mismatch: got {}, expected {}",
            actual, expected_sha256
        ))
    }
}

/// Unpack a gzipped tarball into `target`. Refuses to overwrite existing
/// files unless `force` is true. Refuses path-traversal entries.
pub fn unpack(bytes: &[u8], target: &Path, force: bool) -> Result<Vec<PathBuf>, String> {
    if target.exists() && !target.is_dir() {
        return Err(format!(
            "{} exists and is not a directory",
            target.display()
        ));
    }
    std::fs::create_dir_all(target).map_err(|e| format!("mkdir {}: {}", target.display(), e))?;

    let dec = GzDecoder::new(bytes);
    let mut archive = tar::Archive::new(dec);
    let mut extracted = Vec::new();

    let canonical_target = target
        .canonicalize()
        .map_err(|e| format!("canonicalize target {}: {}", target.display(), e))?;

    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {}", e))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {}", e))?;
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {}", e))?
            .into_owned();
        if path.is_absolute()
            || path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(format!("refused unsafe entry path: {}", path.display()));
        }
        let out = canonical_target.join(&path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
        }
        if out.exists() && !force {
            return Err(format!(
                "refusing to overwrite existing file {} (use --force)",
                out.display()
            ));
        }
        let mut f =
            std::fs::File::create(&out).map_err(|e| format!("create {}: {}", out.display(), e))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("write {}: {}", out.display(), e))?;
        extracted.push(out);
    }
    Ok(extracted)
}

#[allow(dead_code)]
fn ensure_read<R: Read>(r: R) -> R {
    r
}

#[allow(dead_code)]
fn ensure_write<W: Write>(w: W) -> W {
    w
}
