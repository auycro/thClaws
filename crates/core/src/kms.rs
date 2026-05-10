//! Knowledge Management System (KMS) — Karpathy-style LLM wikis.
//!
//! A KMS is a directory of markdown pages plus an `index.md` table of
//! contents and a `log.md` change history. Two scopes:
//!
//! - **User**: `~/.config/thclaws/kms/<name>/`
//! - **Project**: `.thclaws/kms/<name>/`
//!
//! Users mark any subset of KMS as "active" in `.thclaws/settings.json`'s
//! `kms.active` array. When a chat turn runs, each active KMS's
//! `index.md` is concatenated into the system prompt, and the
//! `KmsRead` / `KmsSearch` tools let the model pull in specific pages
//! on demand. No embeddings, no vector store — just grep + read, per
//! Karpathy's pattern.
//!
//! Layout of a KMS directory:
//!
//! ```text
//! <kms_root>/
//!   index.md     — table of contents, one line per page (model reads this)
//!   log.md       — append-only change log (human and model write here)
//!   SCHEMA.md    — optional: shape rules for pages (not enforced in code)
//!   pages/       — individual wiki pages, one per topic
//!   sources/     — raw source material (URLs, PDFs, notes) — optional
//! ```

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KmsScope {
    User,
    Project,
}

impl KmsScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KmsScope::User => "user",
            KmsScope::Project => "project",
        }
    }
}

/// A KMS instance — its scope, name, and root directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmsRef {
    pub name: String,
    pub scope: KmsScope,
    pub root: PathBuf,
}

impl KmsRef {
    pub fn index_path(&self) -> PathBuf {
        self.root.join("index.md")
    }

    pub fn log_path(&self) -> PathBuf {
        self.root.join("log.md")
    }

    pub fn pages_dir(&self) -> PathBuf {
        self.root.join("pages")
    }

    pub fn schema_path(&self) -> PathBuf {
        self.root.join("SCHEMA.md")
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root.join("manifest.json")
    }

    /// Read `index.md`. Returns `""` (not an error) when the file is absent,
    /// OR when the path is a symlink (refused to prevent a cloned KMS
    /// with `index.md -> /etc/passwd` from exfiltrating through the
    /// system prompt). A fresh KMS with no entries yet is a valid state.
    pub fn read_index(&self) -> String {
        let path = self.index_path();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return String::new();
            }
        }
        std::fs::read_to_string(&path).unwrap_or_default()
    }

    /// Read `manifest.json`. Returns `None` when the file is absent (legacy
    /// KMS predating manifests is a valid state), when the path is a symlink
    /// (same exfiltration concern as `read_index`), or when the JSON fails
    /// to parse (treat malformed as absent rather than poisoning lint).
    pub fn read_manifest(&self) -> Option<KmsManifest> {
        let path = self.manifest_path();
        if let Ok(md) = std::fs::symlink_metadata(&path) {
            if md.file_type().is_symlink() {
                return None;
            }
        }
        let raw = std::fs::read_to_string(&path).ok()?;
        serde_json::from_str(&raw).ok()
    }

    /// Resolve a page name to a file path inside `pages/`. `.md` is added
    /// if missing. Returns an error if the resolved path escapes the KMS
    /// directory via `..`, an absolute path, path separators, null bytes,
    /// or symlink trickery (e.g. `pages/` itself symlinked outside, or a
    /// page file symlinked to `/etc/passwd`).
    pub fn page_path(&self, page: &str) -> Result<PathBuf> {
        // Reject obviously-bad names before touching the filesystem.
        if page.is_empty()
            || page.contains("..")
            || page.contains('/')
            || page.contains('\\')
            || page.contains('\0')
            || page.chars().any(|c| c.is_control())
            || Path::new(page).is_absolute()
        {
            return Err(Error::Tool(format!(
                "invalid page name '{page}' — no '..', path separators, or control chars"
            )));
        }
        let name = if page.ends_with(".md") {
            page.to_string()
        } else {
            format!("{page}.md")
        };
        let candidate = self.pages_dir().join(&name);

        // Canonicalize the scope root and require the candidate to resolve
        // *within* this specific KMS directory under it. This defeats
        // symlink bypasses: if `pages/` or the page file itself is a
        // symlink pointing outside, the canonical candidate escapes the
        // KMS root and we reject.
        let canon_candidate = std::fs::canonicalize(&candidate).map_err(|e| {
            Error::Tool(format!(
                "cannot resolve page path '{}': {e}",
                candidate.display()
            ))
        })?;
        let canon_scope = scope_root(self.scope)
            .and_then(|p| std::fs::canonicalize(&p).ok())
            .ok_or_else(|| Error::Tool("kms scope root not resolvable".into()))?;
        let canon_kms_root = canon_scope.join(&self.name);
        if !canon_candidate.starts_with(&canon_kms_root) {
            return Err(Error::Tool(format!(
                "page '{page}' resolves outside the KMS directory — symlink escape rejected"
            )));
        }
        // Also require it's a regular file, not a directory.
        let meta = std::fs::metadata(&canon_candidate)
            .map_err(|e| Error::Tool(format!("cannot stat page '{page}': {e}")))?;
        if !meta.is_file() {
            return Err(Error::Tool(format!("page '{page}' is not a regular file")));
        }
        Ok(candidate)
    }
}

/// Optional per-KMS manifest at `<root>/manifest.json`. Declares the schema
/// version (for `/kms migrate` later) and required frontmatter fields per
/// page category (consumed by `lint`). Absent for legacy KMSes; new ones
/// seeded by `create()` get a v1.0 manifest with empty enforcement so
/// existing tests + workflows are unaffected and policy is opt-in.
///
/// `#[serde(default)]` on every field means future additions don't break
/// older manifests on read — they just take the field's default.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct KmsManifest {
    #[serde(default)]
    pub schema_version: String,
    /// Keys: `"global"` (every page) or a category name (e.g. `"research"`).
    /// Values: required frontmatter field names. Lint flags any page whose
    /// `category:` matches a key but is missing one of the listed fields.
    #[serde(default)]
    pub frontmatter_required: std::collections::BTreeMap<String, Vec<String>>,
}

pub const KMS_SCHEMA_VERSION: &str = "1.0";

fn user_root() -> Option<PathBuf> {
    crate::util::home_dir().map(|h| h.join(".config/thclaws/kms"))
}

fn project_root() -> PathBuf {
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".thclaws/kms")
}

fn scope_root(scope: KmsScope) -> Option<PathBuf> {
    match scope {
        KmsScope::User => user_root(),
        KmsScope::Project => Some(project_root()),
    }
}

/// Enumerate KMS directories under one scope. Silently ignores missing
/// roots — fresh installs have neither. Symlinks are intentionally
/// skipped: a user can't turn a KMS directory into a symlink to `/etc`
/// and have thClaws enumerate it.
fn list_in(scope: KmsScope) -> Vec<KmsRef> {
    let Some(root) = scope_root(scope) else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(&root) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        // symlink_metadata → file_type doesn't follow the symlink, so
        // a `ln -s /etc foo` sitting in the kms dir returns is_symlink.
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_symlink() || !ft.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        out.push(KmsRef {
            name,
            scope,
            root: entry.path(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// List every KMS visible to this process — project entries first, then
/// user. If the same name exists in both scopes, both are returned;
/// callers that need to pick one treat project as higher priority.
pub fn list_all() -> Vec<KmsRef> {
    let mut out = list_in(KmsScope::Project);
    out.extend(list_in(KmsScope::User));
    out
}

/// Find a KMS by name. Project scope wins over user on collision — this
/// matches how project instructions override user instructions elsewhere
/// in thClaws. Returns `None` when no KMS by that name exists, or when
/// the matching directory is a symlink (symlinks are rejected to prevent
/// `ln -s /etc <kms-name>` style exfiltration).
pub fn resolve(name: &str) -> Option<KmsRef> {
    for scope in [KmsScope::Project, KmsScope::User] {
        if let Some(root) = scope_root(scope) {
            let candidate = root.join(name);
            // symlink_metadata doesn't follow the symlink.
            let Ok(meta) = std::fs::symlink_metadata(&candidate) else {
                continue;
            };
            if meta.is_symlink() || !meta.is_dir() {
                continue;
            }
            return Some(KmsRef {
                name: name.to_string(),
                scope,
                root: candidate,
            });
        }
    }
    None
}

/// Create a new KMS. Seeds `index.md`, `log.md`, and `SCHEMA.md` with
/// minimal starter content so the model has something to read on day
/// one. No-op and returns `Ok(existing)` if a KMS by that name already
/// exists at the requested scope.
pub fn create(name: &str, scope: KmsScope) -> Result<KmsRef> {
    if name.is_empty() {
        return Err(Error::Config("kms name must not be empty".into()));
    }
    if name.contains('/')
        || name.contains('\\')
        || name.contains("..")
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || name.starts_with('.')
        || Path::new(name).is_absolute()
    {
        return Err(Error::Config(format!(
            "invalid kms name '{name}' — no path separators, '..', control chars, or leading '.'"
        )));
    }
    let root = scope_root(scope)
        .ok_or_else(|| Error::Config("cannot locate user home directory".into()))?
        .join(name);
    if root.is_dir() {
        return Ok(KmsRef {
            name: name.to_string(),
            scope,
            root,
        });
    }
    std::fs::create_dir_all(root.join("pages"))?;
    std::fs::create_dir_all(root.join("sources"))?;
    let kref = KmsRef {
        name: name.to_string(),
        scope,
        root,
    };
    std::fs::write(
        kref.index_path(),
        format!("# {name}\n\nKnowledge base index — list each page with a one-line summary.\n"),
    )?;
    std::fs::write(
        kref.log_path(),
        "# Change log\n\nAppend-only list of ingests / edits / lints.\n",
    )?;
    std::fs::write(
        kref.schema_path(),
        "# Schema\n\nDescribe the shape of pages in this KMS — required\n\
         sections, naming conventions, cross-link style. Both you and the\n\
         agent read this before editing pages.\n",
    )?;
    let manifest = KmsManifest {
        schema_version: KMS_SCHEMA_VERSION.into(),
        frontmatter_required: std::collections::BTreeMap::new(),
    };
    std::fs::write(
        kref.manifest_path(),
        serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
    )?;
    Ok(kref)
}

/// Extensions a user can ingest into a KMS. Deliberately narrow: these
/// are the text formats `KmsRead` can hand to the model meaningfully,
/// and that a human would expect to grep with `KmsSearch`. Binary
/// formats (PDF, images, archives) are rejected with a hint to convert
/// them to markdown first — we'd rather make the user choose the
/// conversion than silently store a blob the model can't read.
pub const INGEST_EXTENSIONS: &[&str] = &["md", "markdown", "txt", "rst", "log", "json"];

/// Reserved aliases that collide with the KMS starter files — refuse
/// to ingest into them, otherwise a `/kms ingest notes README.md as index`
/// would clobber the index with no way back except `--force`.
const RESERVED_PAGE_STEMS: &[&str] = &["index", "log", "SCHEMA"];

/// What `ingest()` did. `overwrote == true` means `--force` replaced an
/// existing page; the handler surfaces that to the user so a typo in
/// the alias doesn't silently nuke a page. `cascaded` is the count of
/// dependent pages marked stale (M6.25 BUG #10).
#[derive(Debug)]
pub struct IngestResult {
    pub alias: String,
    pub target: PathBuf,
    pub summary: String,
    pub overwrote: bool,
    pub cascaded: usize,
}

/// M6.25 BUG #2: Ingest now SPLITS raw source from wiki page.
///
/// Pre-fix: `ingest()` copied the source straight into `pages/` and
/// treated it as both layer-1 (raw, immutable) and layer-2 (LLM-
/// authored synthesis). The llm-wiki concept requires those to be
/// distinct.
///
/// Post-fix: copy raw to `sources/<alias>.<ext>`, then write a stub
/// page in `pages/<alias>.md` with frontmatter pointing at the
/// source. The page stub is plain markdown the LLM can later enrich
/// via `KmsWrite`. `--force` re-copies the source AND triggers a
/// cascade: any page whose frontmatter `sources:` includes this
/// alias gets a "stale" marker appended (BUG #10). User then runs
/// `/kms lint` or asks the agent to refresh affected pages.
pub fn ingest(
    kms: &KmsRef,
    source: &Path,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let meta = std::fs::metadata(source)
        .map_err(|e| Error::Tool(format!("cannot stat source '{}': {e}", source.display())))?;
    if !meta.is_file() {
        return Err(Error::Tool(format!(
            "source '{}' is not a regular file",
            source.display()
        )));
    }

    let ext_raw = source.extension().and_then(|e| e.to_str()).ok_or_else(|| {
        Error::Tool(format!(
            "'{}' has no extension — ingest requires one of: {}",
            source.display(),
            INGEST_EXTENSIONS.join(", "),
        ))
    })?;
    let ext = ext_raw.to_ascii_lowercase();
    if !INGEST_EXTENSIONS.iter().any(|e| *e == ext) {
        return Err(Error::Tool(format!(
            "extension '.{ext}' not supported — allowed: {} (or use the URL/PDF ingest variants)",
            INGEST_EXTENSIONS.join(", "),
        )));
    }

    let raw_alias = match alias {
        Some(a) => a.to_string(),
        None => source
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("page")
            .to_string(),
    };
    let alias = sanitize_alias(&raw_alias);
    if alias.is_empty() {
        return Err(Error::Tool(format!(
            "alias '{raw_alias}' sanitises to empty — use [A-Za-z0-9_-] characters"
        )));
    }
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(&alias))
    {
        return Err(Error::Tool(format!(
            "alias '{alias}' is reserved — pick another"
        )));
    }

    // Source path lives under sources/, page stub under pages/.
    std::fs::create_dir_all(kms.root.join("sources"))
        .map_err(|e| Error::Tool(format!("ensure sources dir: {e}")))?;
    let source_target = kms.root.join("sources").join(format!("{alias}.{ext}"));
    let page_target = kms.pages_dir().join(format!("{alias}.md"));
    let page_existed = page_target.exists();
    let source_existed = source_target.exists();
    if (page_existed || source_existed) && !force {
        return Err(Error::Tool(format!(
            "alias '{alias}' already exists ({}{}{}) — re-run with --force to overwrite",
            if source_existed { "source" } else { "" },
            if source_existed && page_existed {
                " + "
            } else {
                ""
            },
            if page_existed { "page" } else { "" },
        )));
    }

    std::fs::copy(source, &source_target).map_err(|e| {
        Error::Tool(format!(
            "copy {} → {} failed: {e}",
            source.display(),
            source_target.display()
        ))
    })?;
    let summary = first_summary_line(&source_target);

    // Write the page stub with frontmatter pointing at the source.
    let mut fm = std::collections::BTreeMap::new();
    let today = crate::usage::today_str();
    if !page_existed {
        fm.insert("created".into(), today.clone());
    }
    fm.insert("updated".into(), today.clone());
    fm.insert("category".into(), "uncategorized".into());
    fm.insert("sources".into(), alias.clone());
    let body = format!(
        "# {alias}\n\nStub page — raw source at `sources/{alias}.{ext}`. Summary line: {summary}\n\n\
         _Replace this stub with a curated summary, key takeaways, cross-references to other pages, etc._\n",
    );
    let serialized = write_frontmatter(&fm, &body);
    std::fs::write(&page_target, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write page {}: {e}", page_target.display())))?;

    update_index_for_write(kms, &alias, &summary, Some("uncategorized"), page_existed)?;
    append_log_header(
        kms,
        if page_existed {
            "re-ingested"
        } else {
            "ingested"
        },
        &alias,
    )?;

    // BUG #10: cascade on re-ingest. Pages whose frontmatter
    // `sources:` mentions this alias get a stale marker appended so
    // the next reader (human or agent) knows to refresh.
    let cascade_count = if page_existed && force {
        mark_dependent_pages_stale(kms, &alias).unwrap_or(0)
    } else {
        0
    };

    Ok(IngestResult {
        alias,
        target: page_target,
        summary,
        overwrote: page_existed,
        cascaded: cascade_count,
    })
}

/// M6.25 BUG #10: re-ingest cascade. Walk every page; if its
/// frontmatter `sources:` contains the changed alias (comma- or
/// space- separated list), append a stale-marker line at the bottom
/// of the page body (after frontmatter). Returns the count of pages
/// touched.
fn mark_dependent_pages_stale(kref: &KmsRef, changed_alias: &str) -> Result<usize> {
    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(0),
    };
    let today = crate::usage::today_str();
    let mut count = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem == changed_alias {
            // Don't mark the freshly-written page as stale.
            continue;
        }
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        let sources_field = match fm.get("sources") {
            Some(s) => s.clone(),
            None => continue,
        };
        let mentions = sources_field
            .split(|c: char| c == ',' || c.is_whitespace())
            .any(|s| s.trim() == changed_alias);
        if !mentions {
            continue;
        }
        fm.insert("updated".into(), today.clone());
        let mut new_body = body;
        if !new_body.ends_with('\n') {
            new_body.push('\n');
        }
        new_body.push_str(&format!(
            "\n> ⚠ STALE: source `{changed_alias}` was re-ingested on {today}. Refresh this page.\n"
        ));
        let serialized = write_frontmatter(&fm, &new_body);
        if std::fs::write(&path, serialized.as_bytes()).is_ok() {
            count += 1;
        }
    }
    Ok(count)
}

/// One stale marker found on a page. Multiple entries per page are possible
/// when a source has been re-ingested several times without the page being
/// refreshed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleEntry {
    pub page_stem: String,
    pub source_alias: String,
    pub date: String,
}

/// Pure-read inverse of `mark_dependent_pages_stale`: walks every page and
/// returns every `> ⚠ STALE: source \`<alias>\` was re-ingested on <date>.`
/// marker found in the body. Used by `/kms wrap-up` to surface refresh debt
/// so the user (or the agent) acts on it before the session closes.
pub fn scan_stale_markers(kref: &KmsRef) -> Result<Vec<StaleEntry>> {
    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(Vec::new()),
    };
    // Anchor on the marker prefix from `mark_dependent_pages_stale`. Date
    // format is `crate::usage::today_str()` (YYYY-MM-DD); regex stays loose
    // on the date so a future format change in one place doesn't silently
    // break detection in the other.
    let re =
        regex::Regex::new(r"> ⚠ STALE: source `([^`]+)` was re-ingested on ([^.\s]+)").unwrap();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        for cap in re.captures_iter(&body) {
            out.push(StaleEntry {
                page_stem: stem.clone(),
                source_alias: cap[1].to_string(),
                date: cap[2].to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        a.page_stem
            .cmp(&b.page_stem)
            .then(a.source_alias.cmp(&b.source_alias))
            .then(a.date.cmp(&b.date))
    });
    Ok(out)
}

/// M6.25 BUG #8: ingest a remote URL by fetching it via the existing
/// WebFetchTool then writing the response body to a temp file and
/// running `ingest()` against it. The HTML→markdown conversion is
/// out of scope — we save the raw response. Pages can be cleaned up
/// by the LLM via KmsWrite.
pub async fn ingest_url(
    kref: &KmsRef,
    url: &str,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        // Derive an alias from the last path segment.
        url.trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("page")
            .split('?')
            .next()
            .unwrap_or("page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "could not derive alias from URL '{url}' — pass --alias explicitly"
        )));
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Tool(format!("http client: {e}")))?;
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| Error::Tool(format!("fetch {url}: {e}")))?;
    if !resp.status().is_success() {
        return Err(Error::Tool(format!(
            "fetch {url}: HTTP {}",
            resp.status().as_u16()
        )));
    }
    let body = resp
        .text()
        .await
        .map_err(|e| Error::Tool(format!("read body: {e}")))?;

    // Stage to a tempfile with a markdown extension so the existing
    // ingest path accepts it.
    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-url-{alias_clean}.md"));
    let banner = format!(
        "<!-- fetched from {url} on {} -->\n",
        crate::usage::today_str()
    );
    std::fs::write(&tmp_path, format!("{banner}{body}").as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest(kref, &tmp_path, Some(&alias_clean), force);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// M6.25 BUG #8: ingest a PDF by extracting text via pdftotext
/// (the same path PdfReadTool uses). Output is markdown with a
/// short "extracted from PDF" banner. The agent can refine it
/// with KmsWrite.
pub async fn ingest_pdf(
    kref: &KmsRef,
    pdf_path: &Path,
    alias: Option<&str>,
    force: bool,
) -> Result<IngestResult> {
    let resolved_alias = alias.map(String::from).unwrap_or_else(|| {
        pdf_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("pdf-page")
            .to_string()
    });
    let alias_clean = sanitize_alias(&resolved_alias);
    if alias_clean.is_empty() {
        return Err(Error::Tool(format!(
            "alias derived from PDF is empty — pass --alias"
        )));
    }
    // Run pdftotext in a blocking task — same shape PdfReadTool uses.
    let pdf_owned = pdf_path.to_path_buf();
    let extracted = tokio::task::spawn_blocking(move || -> Result<String> {
        let output = std::process::Command::new("pdftotext")
            .args(["-layout", "-enc", "UTF-8"])
            .arg(&pdf_owned)
            .arg("-") // stdout
            .output()
            .map_err(|e| Error::Tool(format!("pdftotext (is poppler installed?): {e}")))?;
        if !output.status.success() {
            return Err(Error::Tool(format!(
                "pdftotext exited {}: {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .map_err(|e| Error::Tool(format!("pdftotext join: {e}")))??;

    let tmp_dir = std::env::temp_dir();
    let tmp_path = tmp_dir.join(format!("kms-pdf-{alias_clean}.md"));
    let banner = format!(
        "<!-- extracted from PDF '{}' on {} -->\n",
        pdf_path.display(),
        crate::usage::today_str(),
    );
    std::fs::write(&tmp_path, format!("{banner}{extracted}").as_bytes())
        .map_err(|e| Error::Tool(format!("stage {}: {e}", tmp_path.display())))?;
    let result = ingest(kref, &tmp_path, Some(&alias_clean), force);
    let _ = std::fs::remove_file(&tmp_path);
    result
}

/// Keep only `[A-Za-z0-9_-]`; collapse anything else to `_`. An empty
/// result returns empty so the caller can reject it with a useful
/// message rather than writing a page named "".
///
/// Made `pub` in M6.28 so the `/kms ingest <name> $` rewrite can
/// derive a slug from the active session's title (which may contain
/// spaces / punctuation) without re-implementing the sanitizer.
pub fn sanitize_alias(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    cleaned.trim_matches('_').to_string()
}

/// First non-empty line of the just-copied file, trimmed to 80 chars.
/// Leading markdown `#` / `-` / `*` / `>` markers are stripped so the
/// summary reads as a snippet, not as heading syntax inside the index
/// bullet. Returns "(empty)" for empty files.
fn first_summary_line(target: &Path) -> String {
    let text = match std::fs::read_to_string(target) {
        Ok(t) => t,
        Err(_) => return "(binary or unreadable)".into(),
    };
    for line in text.lines() {
        let stripped = line.trim_start_matches(|c: char| {
            c == '#' || c == '-' || c == '*' || c == '>' || c.is_whitespace()
        });
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            let mut s: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    "(empty)".into()
}

// `append_index_entry` + `append_log_entry` removed in M6.25 — the
// new `update_index_for_write` and `append_log_header` (defined
// below in the BUG #1 + #7 sections) replace them with the
// frontmatter-aware index update and the greppable `## [date] verb |
// alias` log format.

/// Render the concatenated active-KMS block to splice into a system
/// prompt. One section per KMS with: SCHEMA.md (M6.25 BUG #5), the
/// index (categorized when pages have YAML frontmatter `category:`,
/// flat otherwise — M6.25 BUG #6), and the read/write/append/search
/// tool affordances.
///
/// Empty string when no active KMS or when active names resolve to
/// nothing.
pub fn system_prompt_section(active: &[String]) -> String {
    let mut parts = Vec::new();
    for name in active {
        let Some(kref) = resolve(name) else { continue };

        // M6.25 BUG #5: pull SCHEMA.md into the prompt. Pre-fix the
        // schema sat on disk but the LLM never saw it, so the "wiki
        // maintainer" affordance had no instructions to follow. Cap
        // by line count to keep prompt bounded.
        let schema = read_text_capped(&kref.schema_path(), 100, 5000);
        // Categorized index — supersedes the raw index.md when pages
        // have frontmatter. Falls back to raw index.md for legacy
        // KMSes that haven't adopted frontmatter.
        let index_section = render_index_section(&kref);

        let mut block = format!("## KMS: {name} ({scope})\n", scope = kref.scope.as_str());
        if !schema.trim().is_empty() {
            block.push_str(&format!("\n### Schema\n{}\n", schema.trim()));
        }
        block.push_str(&format!("\n### Index\n{index_section}\n"));
        block.push_str(&format!(
            "\n### Tools\n\
             - `KmsRead(kms: \"{name}\", page: \"<page>\")` — read one page\n\
             - `KmsSearch(kms: \"{name}\", pattern: \"...\")` — grep across pages\n\
             - `KmsWrite(kms: \"{name}\", page: \"<page>\", content: \"...\")` — create or replace a page\n\
             - `KmsAppend(kms: \"{name}\", page: \"<page>\", content: \"...\")` — append to a page\n\
             - `KmsDelete(kms: \"{name}\", page: \"<page>\")` — remove a page (last resort; prefer KmsWrite to merge or supersede)\n\
             Pages may carry YAML frontmatter (`category:`, `tags:`, `sources:`, `created:`, `updated:`). \
             Follow the schema above when authoring."
        ));
        parts.push(block);
    }
    if parts.is_empty() {
        String::new()
    } else {
        // M6.39.5: strong-imperative wording. Pre-fix the prelude said
        // "consult them before answering when the user's question
        // overlaps" — soft enough that models routinely answered from
        // training data even when the index's per-page summaries
        // clearly matched the user's question. This rewrite uses
        // numbered MUST procedure + explicit "do not skip" + framing
        // skipped lookups as a correctness bug. Reader/maintainer
        // framing kept (still useful) but moved below the consultation
        // procedure so the directive lands first.
        format!(
            "# Active knowledge bases (CONSULT BEFORE ANSWERING)\n\n\
             The following KMS are attached to this conversation. They contain \
             research, notes, and entity pages curated specifically for this project.\n\n\
             **MANDATORY consultation procedure.** For ANY user message whose subject \
             could plausibly appear in the index below, your FIRST action MUST be \
             a tool call sequence — BEFORE composing any prose response:\n\n\
             1. Call `KmsSearch(kms: \"<name>\", pattern: \"<keyword>\")` with 1-3 keyword \
             stems from the user's message. KMS uses plain grep, so romanizations or \
             English keywords work for non-English questions (e.g. user asks in Thai \
             about \"llm-wiki\" → search `pattern: \"llm-wiki\"` or `\"llm wiki\"`).\n\
             2. For each matching page, call `KmsRead(kms: \"<name>\", page: \"<page-stem>\")` \
             to read full content.\n\
             3. ONLY THEN compose your answer, citing KMS pages inline as `(see KMS: <name>/<page>)`.\n\n\
             Do NOT skip steps 1-2 because the question seems familiar from training data. \
             KMS content is authoritative for any topic it covers — the user populated the KMS \
             specifically to override generic answers. Answering without KMS lookup when the \
             index suggests relevance is a correctness bug, not a shortcut.\n\n\
             If `KmsSearch` returns no hits AND the index lists nothing matching the user's \
             topic, fall back to training-data knowledge — but say so explicitly (\"the KMS \
             has nothing on this; answering from general knowledge\").\n\n\
             You are both reader AND maintainer: file new findings via `KmsWrite`, update \
             entity pages when sources contradict them, and run `/kms lint <name>` \
             periodically.\n\n{}",
            parts.join("\n\n")
        )
    }
}

/// Read a text file, cap by lines and bytes for prompt safety.
/// Returns "" when the file is missing or symlinked.
fn read_text_capped(path: &Path, max_lines: usize, max_bytes: usize) -> String {
    if let Ok(md) = std::fs::symlink_metadata(path) {
        if md.file_type().is_symlink() {
            return String::new();
        }
    }
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    if raw.is_empty() {
        return raw;
    }
    crate::memory::truncate_for_prompt(
        raw.trim(),
        max_lines,
        max_bytes,
        &path.display().to_string(),
    )
}

/// M6.25 BUG #6: render index as categorized markdown when pages have
/// frontmatter `category:`. Falls back to the raw index.md (capped)
/// when no frontmatter has been adopted yet — preserves backwards
/// compat with pre-M6.25 KMSes.
fn render_index_section(kref: &KmsRef) -> String {
    use std::collections::BTreeMap;

    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return raw_index_capped(kref),
    };

    let mut by_category: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut any_frontmatter = false;
    let mut total_pages = 0usize;
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        total_pages += 1;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        let (fm, rest) = parse_frontmatter(&body);
        let summary = first_meaningful_line(&rest);
        if let Some(cat) = fm.get("category").cloned() {
            any_frontmatter = true;
            by_category.entry(cat).or_default().push((stem, summary));
        } else {
            by_category
                .entry("uncategorized".into())
                .or_default()
                .push((stem, summary));
        }
    }

    if !any_frontmatter {
        return raw_index_capped(kref);
    }

    let mut out = String::new();
    let mut shown = 0usize;
    let cap = crate::memory::MEMORY_INDEX_MAX_LINES;
    for (cat, mut pages) in by_category {
        pages.sort();
        out.push_str(&format!("\n**{cat}**\n"));
        for (stem, summary) in pages {
            if shown >= cap {
                out.push_str(&format!(
                    "\n_… index truncated at {cap} entries (total: {total_pages})_\n"
                ));
                return out;
            }
            out.push_str(&format!("- [{stem}](pages/{stem}.md) — {summary}\n"));
            shown += 1;
        }
    }
    out
}

fn raw_index_capped(kref: &KmsRef) -> String {
    let index = kref.read_index();
    if index.trim().is_empty() {
        return "(empty index)".into();
    }
    crate::memory::truncate_for_prompt(
        index.trim(),
        crate::memory::MEMORY_INDEX_MAX_LINES,
        crate::memory::MEMORY_INDEX_MAX_BYTES,
        &format!("KMS index `{}`", kref.name),
    )
}

/// First non-empty line of body text, stripped of markdown markers,
/// trimmed to 80 chars. Used for index summaries.
fn first_meaningful_line(body: &str) -> String {
    for line in body.lines() {
        let stripped = line.trim_start_matches(|c: char| {
            c == '#' || c == '-' || c == '*' || c == '>' || c.is_whitespace()
        });
        let trimmed = stripped.trim();
        if !trimmed.is_empty() {
            let mut s: String = trimmed.chars().take(80).collect();
            if trimmed.chars().count() > 80 {
                s.push('…');
            }
            return s;
        }
    }
    "(empty)".into()
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #9: YAML frontmatter convention for KMS pages.
//
// Tiny, hand-rolled parser — we deliberately don't pull in `serde_yaml`
// for this. Pages either start with `---\n<key>: <value>\n...\n---\n`
// or they don't. Values are flat strings (single line), no nesting,
// no anchors, no multiline. That matches the documented convention
// (`category:`, `tags:`, `sources:`, `created:`, `updated:`) — anything
// fancier should live in the page body, not the metadata.

/// Parse `(frontmatter, body)` from a page. Frontmatter map preserves
/// insertion order via Vec under the hood (BTreeMap is fine — keys
/// are conventional and small). Returns `(empty, original)` when no
/// frontmatter delimiter present.
pub fn parse_frontmatter(s: &str) -> (std::collections::BTreeMap<String, String>, String) {
    let mut map = std::collections::BTreeMap::new();
    let trimmed = s.trim_start_matches('\u{FEFF}');
    let Some(after_open) = trimmed.strip_prefix("---\n") else {
        return (map, s.to_string());
    };
    // Find the closing `---\n` (or `---` at EOF) anchored to start-of-line.
    let close_idx = after_open.find("\n---\n").or_else(|| {
        if after_open.ends_with("\n---") {
            Some(after_open.len() - 4)
        } else {
            None
        }
    });
    let Some(close) = close_idx else {
        return (map, s.to_string());
    };
    let yaml = &after_open[..close];
    let body = if close + 5 <= after_open.len() {
        // skip "\n---\n"
        &after_open[close + 5..]
    } else {
        ""
    };
    for line in yaml.lines() {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().trim_matches('"').trim_matches('\'').to_string();
            if !key.is_empty() {
                map.insert(key, val);
            }
        }
    }
    (map, body.to_string())
}

/// Serialize a frontmatter map + body into a page string. Empty map →
/// just the body (no `---` block).
pub fn write_frontmatter(map: &std::collections::BTreeMap<String, String>, body: &str) -> String {
    if map.is_empty() {
        return body.to_string();
    }
    let mut out = String::from("---\n");
    for (k, v) in map {
        // YAML-safe values: if the value contains `:`, `#`, leading
        // whitespace, or quote chars, wrap in double quotes and
        // escape internal double quotes.
        let needs_quote = v.contains(':')
            || v.contains('#')
            || v.starts_with(' ')
            || v.contains('"')
            || v.contains('\n');
        if needs_quote {
            let escaped = v.replace('"', "\\\"");
            out.push_str(&format!("{k}: \"{escaped}\"\n"));
        } else {
            out.push_str(&format!("{k}: {v}\n"));
        }
    }
    out.push_str("---\n");
    out.push_str(body);
    out
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #1 + #4: write helpers for KMS pages.
//
// `KmsWrite` / `KmsAppend` tools and the `/kms file-answer` slash
// command bypass `Sandbox::check_write` to land inside the KMS root
// (project-scope `.thclaws/kms/.../pages/...` is otherwise blocked).
// Same pattern as TodoWrite's intentional `.thclaws/todos.md` carve-
// out: the path is computed from a validated KMS name + a validated
// page name (no `..`, no path separators, no symlinks, must resolve
// inside the KMS root via `KmsRef::page_path`-style canonicalization).
//
// We don't want the LLM passing an arbitrary file path here.

/// Resolve `page_name` to a writable path inside `kref.pages_dir()`.
/// Differs from `KmsRef::page_path` — that one requires the file to
/// EXIST so canonicalize works. This one is for create-or-replace, so
/// it canonicalizes the parent directory and ensures the candidate
/// resolves under it.
pub fn writable_page_path(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    if page_name.is_empty()
        || page_name.contains("..")
        || page_name.contains('/')
        || page_name.contains('\\')
        || page_name.contains('\0')
        || page_name.chars().any(|c| c.is_control())
        || Path::new(page_name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid page name '{page_name}' — no '..', path separators, or control chars"
        )));
    }
    let stem = page_name.trim_end_matches(".md");
    if RESERVED_PAGE_STEMS
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        return Err(Error::Tool(format!(
            "page name '{page_name}' is reserved — pick another stem"
        )));
    }
    let name = if page_name.ends_with(".md") {
        page_name.to_string()
    } else {
        format!("{page_name}.md")
    };

    let pages_dir = kref.pages_dir();
    std::fs::create_dir_all(&pages_dir)
        .map_err(|e| Error::Tool(format!("ensure pages dir for '{}': {e}", kref.name)))?;
    // Refuse if pages/ itself is a symlink (would let an attacker
    // redirect writes outside the KMS root).
    if let Ok(md) = std::fs::symlink_metadata(&pages_dir) {
        if md.file_type().is_symlink() {
            return Err(Error::Tool(format!(
                "kms '{}' has a symlinked pages/ directory — refusing to write",
                kref.name
            )));
        }
    }
    let canon_pages = std::fs::canonicalize(&pages_dir)
        .map_err(|e| Error::Tool(format!("canonicalize pages dir: {e}")))?;
    let candidate = canon_pages.join(&name);
    // The candidate may not exist yet (create case) — verify the
    // parent canonicalizes inside pages_dir, and that the file
    // (if it exists) is not a symlink to outside.
    if let Ok(canon_existing) = std::fs::canonicalize(&candidate) {
        if !canon_existing.starts_with(&canon_pages) {
            return Err(Error::Tool(format!(
                "page '{page_name}' resolves outside pages/ — symlink escape rejected"
            )));
        }
    }
    Ok(candidate)
}

/// Write (create-or-replace) a page. Bumps `updated:` frontmatter to
/// today, preserves existing other frontmatter when the body itself
/// includes a `---` block. Updates the index.md bullet under the
/// page's category. Appends a log entry.
pub fn write_page(kref: &KmsRef, page_name: &str, content: &str) -> Result<PathBuf> {
    let path = writable_page_path(kref, page_name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();

    // Merge user-supplied content's frontmatter with auto-stamped
    // `updated:` (and `created:` on new pages). User-supplied keys
    // win on conflict — they explicitly set them.
    let (mut fm, body) = parse_frontmatter(content);
    let today = crate::usage::today_str();
    fm.entry("updated".into()).or_insert_with(|| today.clone());
    if !existed {
        fm.entry("created".into()).or_insert(today.clone());
    }
    let serialized = write_frontmatter(&fm, &body);
    std::fs::write(&path, serialized.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;

    let summary = first_meaningful_line(&body);
    let category = fm.get("category").cloned();
    update_index_for_write(kref, &stem, &summary, category.as_deref(), existed)?;
    append_log_header(kref, if existed { "edited" } else { "wrote" }, &stem)?;
    Ok(path)
}

/// Append a chunk to a page. If the page doesn't exist, create it
/// (no frontmatter — the model can write a full page later via
/// `KmsWrite` to add metadata). Bumps `updated:` if frontmatter
/// already present.
pub fn append_to_page(kref: &KmsRef, page_name: &str, chunk: &str) -> Result<PathBuf> {
    use std::io::Write;
    let path = writable_page_path(kref, page_name)?;
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    let existed = path.exists();
    if existed {
        // Bump updated: in frontmatter if present, leave body alone,
        // append the new chunk after a newline.
        let raw = std::fs::read_to_string(&path).unwrap_or_default();
        let (mut fm, body) = parse_frontmatter(&raw);
        if !fm.is_empty() {
            fm.insert("updated".into(), crate::usage::today_str());
            let mut new_body = body;
            if !new_body.ends_with('\n') {
                new_body.push('\n');
            }
            new_body.push_str(chunk);
            let serialized = write_frontmatter(&fm, &new_body);
            std::fs::write(&path, serialized.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        } else {
            // No frontmatter — straight append.
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
            if !raw.ends_with('\n') {
                writeln!(f).ok();
            }
            f.write_all(chunk.as_bytes())
                .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        }
    } else {
        // Create with bare body (no frontmatter); subsequent
        // writes can add metadata.
        std::fs::write(&path, chunk.as_bytes())
            .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
        let summary = first_meaningful_line(chunk);
        update_index_for_write(kref, &stem, &summary, None, false)?;
    }
    append_log_header(kref, "appended", &stem)?;
    Ok(path)
}

/// Delete a KMS page. Validates the name via `writable_page_path`
/// (same path-safety carve-out as write/append), removes the file,
/// strips the matching bullet from `index.md`, and appends a
/// `## [YYYY-MM-DD] deleted | <stem>` entry to `log.md`.
pub fn delete_page(kref: &KmsRef, page_name: &str) -> Result<PathBuf> {
    let path = writable_page_path(kref, page_name)?;
    if !path.exists() {
        return Err(Error::Tool(format!("page not found: {}", path.display())));
    }
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("page")
        .to_string();
    std::fs::remove_file(&path)
        .map_err(|e| Error::Tool(format!("remove {}: {e}", path.display())))?;
    remove_index_bullet(kref, &stem)?;
    append_log_header(kref, "deleted", &stem)?;
    Ok(path)
}

/// M6.39.9: list every readable `*.md` file inside a KMS, split by
/// kind (`pages/` and `sources/`). Drives the right-edge KMS browser
/// panel — clicking the title of a KMS row in the sidebar opens this
/// listing, clicking a list entry opens the viewer overlay.
///
/// Filenames returned without the `.md` extension (so the frontend
/// can use them as page-name keys consistent with `KmsRead`).
/// Sorted alphabetically. Hidden files (`.foo`) skipped.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseFile {
    pub name: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowseListing {
    pub kms: String,
    pub pages: Vec<BrowseFile>,
    pub sources: Vec<BrowseFile>,
}

/// List browseable files for a KMS by name. Returns `None` if the
/// KMS isn't found. `pages/` and `sources/` are independent — a KMS
/// that predates M6.39.5 may have no `sources/` dir; that's fine,
/// returns empty list for that side.
pub fn browse(name: &str) -> Option<BrowseListing> {
    let kref = resolve(name)?;
    let pages = scan_dir_md(&kref.pages_dir());
    let sources = scan_dir_md(&kref.root.join("sources"));
    Some(BrowseListing {
        kms: name.to_string(),
        pages,
        sources,
    })
}

fn scan_dir_md(dir: &Path) -> Vec<BrowseFile> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut out: Vec<BrowseFile> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || !name.ends_with(".md") {
            continue;
        }
        let stem = name.trim_end_matches(".md").to_string();
        let bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        out.push(BrowseFile { name: stem, bytes });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// M6.39.13: build an Obsidian-style graph of one KMS — every page
/// is a node, every `[[slug]]` wikilink is a directed edge. Used by
/// the right-pane "Graph" view that mirrors Obsidian's visualization
/// of the same data.
///
/// Pages without outgoing OR incoming links are still emitted as
/// isolated nodes — the user wants to see them and decide whether
/// to link them.
///
/// Edge resolution: a `[[other-slug]]` in `karpathy.md` becomes an
/// edge `karpathy → other-slug` IF `other-slug.md` exists in the
/// same KMS. Dangling links (slug not present) are dropped silently
/// — the graph view shouldn't show ghost nodes for broken refs.
///
/// When `include_sources` is true, source files in `<root>/sources/`
/// are emitted as `kind: "source"` nodes and edges are added from
/// any page whose body cites them via `(../sources/<slug>.md)` (the
/// format produced by `linkify_citations` and the `## Sources`
/// section). Source nodes without any backlink are still listed —
/// orphan archives are useful to surface.
#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphNode {
    pub id: String, // page slug (filename stem); for sources we use `source:<stem>` to namespace
    pub label: String, // title from frontmatter, falls back to id
    pub size: u32,  // total link count (in + out) — sized in UI
    pub kind: GraphNodeKind,
}

#[derive(Debug, Clone, Copy, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum GraphNodeKind {
    Page,
    Source,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GraphData {
    pub kms: String,
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
}

/// Build the graph for `kms_name`. Returns `None` if the KMS isn't
/// found. Always succeeds for a valid KMS even if pages are empty.
///
/// `include_sources` toggles whether source archives in `<root>/sources/`
/// are emitted as nodes. When true, page → source citation edges are
/// also added (parsed from `(../sources/<slug>.md)` markdown links
/// inside page bodies — the format produced by `linkify_citations`
/// and the `## Sources` section).
///
/// Source node IDs are namespaced as `source:<stem>` so they can't
/// collide with page slugs and the frontend can route clicks back
/// to `read_browse_file(kind="source", name="<stem>.md")`.
pub fn graph(kms_name: &str, include_sources: bool) -> Option<GraphData> {
    let kref = resolve(kms_name)?;
    let pages_dir = kref.pages_dir();
    let pages_iter = std::fs::read_dir(&pages_dir).ok();

    // First pass: collect every page slug + its title. Skip
    // hidden / non-md / `_summary` (it's an index, not a real
    // research page) so the graph isn't dominated by it.
    let mut nodes: std::collections::BTreeMap<String, GraphNode> =
        std::collections::BTreeMap::new();
    let mut bodies: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    if let Some(entries) = pages_iter {
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_file() {
                continue;
            }
            let filename = entry.file_name().to_string_lossy().to_string();
            if filename.starts_with('.') || !filename.ends_with(".md") {
                continue;
            }
            let stem = filename.trim_end_matches(".md").to_string();
            if stem == "_summary" {
                continue;
            }
            let body = match std::fs::read_to_string(entry.path()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let (fm, _) = parse_frontmatter(&body);
            let label = fm
                .get("title")
                .map(|s| s.trim().trim_matches('"').to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| stem.clone());
            nodes.insert(
                stem.clone(),
                GraphNode {
                    id: stem.clone(),
                    label,
                    size: 0,
                    kind: GraphNodeKind::Page,
                },
            );
            bodies.insert(stem, body);
        }
    }

    // Optional: list sources/ as nodes (`source:<stem>` IDs) and
    // register their stems for citation-edge resolution. Title comes
    // from frontmatter if the source archive has it (HAL-fetched
    // markdown often does), else falls back to the bare stem.
    let mut source_stems: std::collections::HashSet<String> = std::collections::HashSet::new();
    if include_sources {
        let sources_dir = kref.root.join("sources");
        if let Ok(entries) = std::fs::read_dir(&sources_dir) {
            for entry in entries.flatten() {
                let Ok(ft) = entry.file_type() else { continue };
                if !ft.is_file() {
                    continue;
                }
                let filename = entry.file_name().to_string_lossy().to_string();
                if filename.starts_with('.') || !filename.ends_with(".md") {
                    continue;
                }
                let stem = filename.trim_end_matches(".md").to_string();
                let label = std::fs::read_to_string(entry.path())
                    .ok()
                    .and_then(|raw| {
                        let (fm, _) = parse_frontmatter(&raw);
                        fm.get("title")
                            .map(|s| s.trim().trim_matches('"').to_string())
                            .filter(|s| !s.is_empty())
                    })
                    .unwrap_or_else(|| stem.clone());
                let node_id = format!("source:{stem}");
                nodes.insert(
                    node_id.clone(),
                    GraphNode {
                        id: node_id,
                        label,
                        size: 0,
                        kind: GraphNodeKind::Source,
                    },
                );
                source_stems.insert(stem);
            }
        }
    }

    // Second pass: scan each body for `[[slug]]` wikilinks (page→page)
    // and `(../sources/<stem>.md)` markdown links (page→source) and
    // emit edges where the target exists in the node set.
    let mut edges: Vec<GraphEdge> = Vec::new();
    for (source, body) in &bodies {
        for target in extract_wikilink_targets(body) {
            if !nodes.contains_key(&target) {
                continue;
            }
            if &target == source {
                continue;
            }
            edges.push(GraphEdge {
                source: source.clone(),
                target,
            });
        }
        if include_sources {
            for stem in extract_source_link_targets(body) {
                if !source_stems.contains(&stem) {
                    continue;
                }
                edges.push(GraphEdge {
                    source: source.clone(),
                    target: format!("source:{stem}"),
                });
            }
        }
    }

    // Compute node `size` = total in + out degree, used by the
    // frontend to scale node radii.
    for e in &edges {
        if let Some(n) = nodes.get_mut(&e.source) {
            n.size += 1;
        }
        if let Some(n) = nodes.get_mut(&e.target) {
            n.size += 1;
        }
    }

    Some(GraphData {
        kms: kms_name.to_string(),
        nodes: nodes.into_values().collect(),
        edges,
    })
}

/// Extract source filenames from `](../sources/<stem>.md)` markdown
/// links — the canonical citation format produced by
/// `linkify_citations` + the auto-generated `## Sources` section.
/// Returns the bare stem (no path, no `.md`).
fn extract_source_link_targets(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "](../sources/";
    let mut search_from = 0;
    while let Some(rel) = body[search_from..].find(needle) {
        let abs = search_from + rel + needle.len();
        let rest = &body[abs..];
        let end = rest.find(')').unwrap_or(rest.len());
        let target = &rest[..end];
        // Strip optional `.md` suffix and any URL fragment / query.
        let cleaned = target
            .split(|c| c == '#' || c == '?')
            .next()
            .unwrap_or(target)
            .trim_end_matches(".md");
        if !cleaned.is_empty() && !cleaned.contains('/') && cleaned.len() <= 200 {
            out.push(cleaned.to_string());
        }
        search_from = abs + end;
    }
    out
}

/// Walk the markdown body, return every `[[slug]]` (or `[[slug|display]]`)
/// target as a list. Slug is the part before `|`; display is dropped
/// (we only need the link target). Multiline / oversized brackets
/// skipped to avoid pathological inputs.
fn extract_wikilink_targets(body: &str) -> Vec<String> {
    let bytes = body.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 1 < body.len() {
        if bytes[i] == b'[' && bytes[i + 1] == b'[' {
            if let Some(end_rel) = body[i + 2..].find("]]") {
                let inner = &body[i + 2..i + 2 + end_rel];
                if inner.len() <= 120 && !inner.contains('\n') {
                    let slug = inner
                        .split_once('|')
                        .map(|(s, _)| s.trim().to_string())
                        .unwrap_or_else(|| inner.trim().to_string());
                    if !slug.is_empty() {
                        out.push(slug);
                    }
                }
                i = i + 2 + end_rel + 2;
                continue;
            }
        }
        // Advance to next char boundary.
        let mut j = i + 1;
        while j < body.len() && !body.is_char_boundary(j) {
            j += 1;
        }
        i = j;
    }
    out
}

/// M6.39.9: read a file from a KMS's `pages/` or `sources/` dir
/// for the viewer overlay. `kind` is `"page"` or `"source"`; `name`
/// is the bare filename stem (no `.md`). Path-safety mirrors
/// [`writable_page_path`] — the viewer is read-only, but we still
/// don't want a crafted IPC reading `/etc/passwd` via traversal.
pub fn read_browse_file(kms_name: &str, kind: &str, name: &str) -> Result<String> {
    if name.is_empty()
        || name.contains("..")
        || name.contains('/')
        || name.contains('\\')
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
        || Path::new(name).is_absolute()
    {
        return Err(Error::Tool(format!(
            "invalid file name '{name}' — no path separators or traversal"
        )));
    }
    let kref =
        resolve(kms_name).ok_or_else(|| Error::Tool(format!("KMS '{kms_name}' not found")))?;
    let dir = match kind {
        "page" => kref.pages_dir(),
        "source" => kref.root.join("sources"),
        other => return Err(Error::Tool(format!("invalid kind '{other}'"))),
    };
    let stem = name.trim_end_matches(".md");
    let path = dir.join(format!("{stem}.md"));
    if !path.exists() {
        return Err(Error::Tool(format!("not found: {}", path.display())));
    }
    // Canonicalize both and confirm path lives inside dir — defense
    // in depth even though the bare-name validation above already
    // blocks `..`.
    let canon_dir = std::fs::canonicalize(&dir)
        .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", dir.display())))?;
    let canon_path = std::fs::canonicalize(&path)
        .map_err(|e| Error::Tool(format!("canonicalize {}: {e}", path.display())))?;
    if !canon_path.starts_with(&canon_dir) {
        return Err(Error::Tool(format!(
            "path '{}' escaped KMS root",
            path.display()
        )));
    }
    std::fs::read_to_string(&canon_path)
        .map_err(|e| Error::Tool(format!("read {}: {e}", canon_path.display())))
}

fn remove_index_bullet(kref: &KmsRef, stem: &str) -> Result<()> {
    let path = kref.index_path();
    let Ok(existing) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let needle = format!("(pages/{stem}.md)");
    let filtered: Vec<&str> = existing.lines().filter(|l| !l.contains(&needle)).collect();
    let mut new_body = filtered.join("\n");
    if !new_body.ends_with('\n') && !new_body.is_empty() {
        new_body.push('\n');
    }
    std::fs::write(&path, new_body.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// Update index.md to reflect a write. Adds a fresh bullet (or
/// replaces an existing one for the same page). Categorization is a
/// hint — the actual rendering for the system prompt is built from
/// per-page frontmatter at read time, so this is just so the on-disk
/// index.md stays human-readable.
fn update_index_for_write(
    kref: &KmsRef,
    stem: &str,
    summary: &str,
    _category: Option<&str>,
    existed: bool,
) -> Result<()> {
    use std::io::Write;
    let path = kref.index_path();
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    let needle = format!("(pages/{stem}.md)");
    if existed || existing.contains(&needle) {
        existing = existing
            .lines()
            .filter(|l| !l.contains(&needle))
            .collect::<Vec<_>>()
            .join("\n");
        if !existing.ends_with('\n') {
            existing.push('\n');
        }
    }
    if !existing.ends_with('\n') && !existing.is_empty() {
        existing.push('\n');
    }
    existing.push_str(&format!("- [{stem}](pages/{stem}.md) — {summary}\n"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(existing.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

/// M6.25 BUG #7: append a header-style log entry for greppability.
/// `## [YYYY-MM-DD] verb | alias`. Pre-fix `- date verb src → dest`
/// bullets weren't greppable as "give me the last 5 ingests".
fn append_log_header(kref: &KmsRef, verb: &str, alias: &str) -> Result<()> {
    use std::io::Write;
    let path = kref.log_path();
    let line = format!("## [{}] {verb} | {alias}\n", crate::usage::today_str());
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    f.write_all(line.as_bytes())
        .map_err(|e| Error::Tool(format!("write {}: {e}", path.display())))?;
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────
// M6.25 BUG #3: lint — pure-read health check.

/// What `lint()` found. Each list is a category of issue.
#[derive(Debug, Default)]
pub struct LintReport {
    pub orphan_pages: Vec<String>, // page exists but no inbound link from any other page
    pub broken_links: Vec<(String, String)>, // (page, target) where pages/<target>.md doesn't exist
    pub index_orphans: Vec<String>, // index entry but no underlying file
    pub missing_in_index: Vec<String>, // page file but no index entry
    pub missing_frontmatter: Vec<String>, // page has no `---` block
    /// (page_stem, source_key, missing_field) — `source_key` is `"global"`
    /// or the page's `category:` value, indicating which manifest rule the
    /// field came from. Empty when no manifest exists or the manifest's
    /// `frontmatter_required` map is empty.
    pub missing_required_fields: Vec<(String, String, String)>,
}

impl LintReport {
    pub fn total_issues(&self) -> usize {
        self.orphan_pages.len()
            + self.broken_links.len()
            + self.index_orphans.len()
            + self.missing_in_index.len()
            + self.missing_frontmatter.len()
            + self.missing_required_fields.len()
    }
}

/// Walk a KMS and report common health issues. Pure-read; doesn't
/// modify the wiki. Inbound-link detection is greedy: any markdown
/// link `[*](pages/<stem>.md)` counts.
pub fn lint(kref: &KmsRef) -> Result<LintReport> {
    use std::collections::HashSet;
    let mut report = LintReport::default();

    let pages_dir = kref.pages_dir();
    let entries = match std::fs::read_dir(&pages_dir) {
        Ok(e) => e,
        Err(_) => return Ok(report),
    };

    let mut all_stems: HashSet<String> = HashSet::new();
    let mut page_bodies: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if ft.is_symlink() || !ft.is_file() {
            continue;
        }
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        if stem.is_empty() {
            continue;
        }
        all_stems.insert(stem.clone());
        let body = std::fs::read_to_string(&path).unwrap_or_default();
        page_bodies.push((stem, body));
    }

    // Frontmatter audit + outbound link extraction.
    // Load the manifest's required-fields map once. Empty (or absent) skips
    // the per-page required-field check entirely — keeps legacy KMSes silent.
    let required_fields = kref
        .read_manifest()
        .map(|m| m.frontmatter_required)
        .unwrap_or_default();
    let link_re = regex::Regex::new(r"\(pages/([^)]+?)\.md\)").unwrap();
    let mut inbound_targets: HashSet<String> = HashSet::new();
    for (stem, body) in &page_bodies {
        let (fm, _rest) = parse_frontmatter(body);
        if fm.is_empty() {
            report.missing_frontmatter.push(stem.clone());
        } else if !required_fields.is_empty() {
            // Check global rules first, then any category-specific rules.
            // The same field listed under both keys is reported twice — by
            // design, so the user can see which rule fired and remove the
            // redundancy from their manifest.
            let category = fm.get("category").map(String::as_str).unwrap_or("");
            for source_key in ["global", category] {
                if source_key.is_empty() {
                    continue;
                }
                if let Some(fields) = required_fields.get(source_key) {
                    for field in fields {
                        if !fm.contains_key(field) {
                            report.missing_required_fields.push((
                                stem.clone(),
                                source_key.to_string(),
                                field.clone(),
                            ));
                        }
                    }
                }
            }
        }
        for cap in link_re.captures_iter(body) {
            let target = cap[1].to_string();
            inbound_targets.insert(target.clone());
            if !all_stems.contains(&target) {
                report.broken_links.push((stem.clone(), target));
            }
        }
    }

    // Orphan pages: exist on disk but no other page links to them.
    for (stem, _) in &page_bodies {
        if !inbound_targets.contains(stem) {
            report.orphan_pages.push(stem.clone());
        }
    }

    // Index <-> filesystem cross-check.
    let index = kref.read_index();
    let index_re = regex::Regex::new(r"\(pages/([^)]+?)\.md\)").unwrap();
    let mut indexed: HashSet<String> = HashSet::new();
    for cap in index_re.captures_iter(&index) {
        indexed.insert(cap[1].to_string());
    }
    for stem in &indexed {
        if !all_stems.contains(stem) {
            report.index_orphans.push(stem.clone());
        }
    }
    for stem in &all_stems {
        if !indexed.contains(stem) {
            report.missing_in_index.push(stem.clone());
        }
    }

    report.orphan_pages.sort();
    report.broken_links.sort();
    report.index_orphans.sort();
    report.missing_in_index.sort();
    report.missing_frontmatter.sort();
    report.missing_required_fields.sort();
    Ok(report)
}

// ────────────────────────────────────────────────────────────────────────
// Schema migrations — chained version upgrades anchored on KmsManifest.

/// Sentinel for any KMS that predates the manifest entirely. Treated as
/// "0.x" by the migration chain so legacy stores get bumped to 1.0 the
/// first time `/kms migrate` runs.
pub const LEGACY_SCHEMA_VERSION: &str = "0.x";

/// One step in the migration chain. `from`/`to` are the `schema_version`
/// strings as they appear in `manifest.json`. The `apply` function takes
/// a `dry_run` flag — in dry-run mode it must not touch the filesystem;
/// in live mode it returns descriptions of what was actually written.
pub struct Migration {
    pub from: &'static str,
    pub to: &'static str,
    pub apply: fn(&KmsRef, dry_run: bool) -> Result<Vec<String>>,
}

/// Registry of known migrations, in chain order. Add a new entry when
/// the schema changes; the resolver in `migrate()` walks `from → to`
/// until it reaches `KMS_SCHEMA_VERSION`.
pub fn migrations() -> Vec<Migration> {
    vec![Migration {
        from: LEGACY_SCHEMA_VERSION,
        to: "1.0",
        apply: migrate_0_to_1,
    }]
}

/// 0.x → 1.0: write the initial manifest with empty enforcement.
/// Pure additive change — no page bodies touched, no index changes.
/// Lint behaviour is identical before and after; the manifest just
/// anchors future migrations and gives users a place to declare
/// `frontmatter_required` rules.
fn migrate_0_to_1(kref: &KmsRef, dry_run: bool) -> Result<Vec<String>> {
    let manifest_path = kref.manifest_path();
    let actions = vec![format!(
        "write {} (schema_version: 1.0, frontmatter_required: empty)",
        manifest_path.display()
    )];
    if !dry_run {
        let manifest = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: std::collections::BTreeMap::new(),
        };
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap_or_else(|_| "{}".into()),
        )
        .map_err(|e| Error::Tool(format!("write {}: {e}", manifest_path.display())))?;
        append_log_header(kref, "migrated", "0.x → 1.0")?;
    }
    Ok(actions)
}

/// Detect the current schema version. Absent manifest, or manifest with
/// empty `schema_version`, is treated as legacy `0.x` — that's how every
/// KMS created before the manifest feature looks on disk.
pub fn detect_schema_version(kref: &KmsRef) -> String {
    match kref.read_manifest() {
        Some(m) if !m.schema_version.is_empty() => m.schema_version,
        _ => LEGACY_SCHEMA_VERSION.into(),
    }
}

#[derive(Debug)]
pub struct MigrationStep {
    pub from: String,
    pub to: String,
    pub actions: Vec<String>,
}

#[derive(Debug)]
pub struct MigrationReport {
    pub current_version: String,
    pub target_version: String,
    pub steps: Vec<MigrationStep>,
    pub dry_run: bool,
}

/// Walk the migration chain from the KMS's current schema_version up to
/// `KMS_SCHEMA_VERSION`. In dry-run mode, returns the plan without
/// writing. In live mode, applies each step and returns what happened.
///
/// Idempotent: a KMS already at the latest version returns a report
/// with no steps and `current_version == target_version`.
pub fn migrate(kref: &KmsRef, dry_run: bool) -> Result<MigrationReport> {
    let initial = detect_schema_version(kref);
    let target = KMS_SCHEMA_VERSION.to_string();
    let mut report = MigrationReport {
        current_version: initial.clone(),
        target_version: target.clone(),
        steps: Vec::new(),
        dry_run,
    };
    if initial == target {
        return Ok(report);
    }
    let table = migrations();
    let mut current = initial;
    // Bound the loop defensively — `table` is hand-edited, but a bad
    // edit (e.g. a cycle 1.0 → 1.0) shouldn't spin forever.
    for _ in 0..table.len() + 1 {
        if current == target {
            break;
        }
        let Some(m) = table.iter().find(|m| m.from == current) else {
            return Err(Error::Tool(format!(
                "no migration path from schema version '{current}' to '{target}'"
            )));
        };
        let actions = (m.apply)(kref, dry_run)?;
        report.steps.push(MigrationStep {
            from: m.from.to_string(),
            to: m.to.to_string(),
            actions,
        });
        current = m.to.to_string();
    }
    if current != target {
        return Err(Error::Tool(format!(
            "migration chain stalled at '{current}', target '{target}' (likely a cycle in migrations())"
        )));
    }
    Ok(report)
}

// ────────────────────────────────────────────────────────────────────────
// User-facing report formatters. Live here (not in shell_dispatch.rs)
// because the CLI binary `thclaws-cli` is built without the `gui`
// feature — and `shell_dispatch` is gated behind `gui`. Pure functions:
// `&LintReport` / `&MigrationReport` / `&[StaleEntry]` → `String`.
// (M6.38.3 audit fix.)

/// Render a `LintReport` as the user-facing summary block emitted by
/// `/kms lint <name>`. Six issue categories; clean state returns a
/// short "no issues found" line.
pub fn format_lint_report(name: &str, report: &LintReport) -> String {
    let total = report.total_issues();
    if total == 0 {
        return format!("KMS '{name}': clean — no issues found.");
    }
    let mut out = format!("KMS '{name}': {total} issue(s)\n");
    if !report.broken_links.is_empty() {
        out.push_str(&format!(
            "\nbroken links ({}):\n",
            report.broken_links.len()
        ));
        for (page, target) in &report.broken_links {
            out.push_str(&format!("  - {page} → pages/{target}.md (missing)\n"));
        }
    }
    if !report.index_orphans.is_empty() {
        out.push_str(&format!(
            "\nindex entries with no underlying file ({}):\n",
            report.index_orphans.len()
        ));
        for stem in &report.index_orphans {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_in_index.is_empty() {
        out.push_str(&format!(
            "\npages missing from index ({}):\n",
            report.missing_in_index.len()
        ));
        for stem in &report.missing_in_index {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.orphan_pages.is_empty() {
        out.push_str(&format!(
            "\norphan pages (no inbound links from other pages, {}):\n",
            report.orphan_pages.len()
        ));
        for stem in &report.orphan_pages {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_frontmatter.is_empty() {
        out.push_str(&format!(
            "\npages without YAML frontmatter ({}):\n",
            report.missing_frontmatter.len()
        ));
        for stem in &report.missing_frontmatter {
            out.push_str(&format!("  - {stem}\n"));
        }
    }
    if !report.missing_required_fields.is_empty() {
        out.push_str(&format!(
            "\nmissing required frontmatter fields ({}):\n",
            report.missing_required_fields.len()
        ));
        for (page, source_key, field) in &report.missing_required_fields {
            out.push_str(&format!(
                "  - {page}: '{field}' (required by {source_key})\n"
            ));
        }
    }
    out
}

/// Session-end review: lint output plus any STALE markers left behind
/// by re-ingest cascades. Both are pure-read; the user (or agent) acts
/// on them via KmsWrite. The "next step" hints surface what's most
/// actionable.
pub fn format_wrap_up_report(name: &str, lint: &LintReport, stale: &[StaleEntry]) -> String {
    let lint_total = lint.total_issues();
    let stale_count = stale.len();
    if lint_total == 0 && stale_count == 0 {
        return format!("KMS '{name}': clean — nothing to wrap up.");
    }
    let mut out = format!(
        "KMS '{name}': wrap-up — {lint_total} lint issue(s), {stale_count} stale marker(s)\n"
    );
    if lint_total > 0 {
        // Reuse the lint formatter so both surfaces stay consistent.
        let lint_body = format_lint_report(name, lint);
        // Drop the lint formatter's own header line; we already wrote one.
        if let Some((_, rest)) = lint_body.split_once('\n') {
            out.push_str(rest);
            if !out.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    if stale_count > 0 {
        out.push_str(&format!(
            "\nstale pages awaiting refresh ({stale_count}):\n"
        ));
        for entry in stale {
            out.push_str(&format!(
                "  - {}: source `{}` re-ingested on {} (page not yet refreshed)\n",
                entry.page_stem, entry.source_alias, entry.date
            ));
        }
    }
    out.push_str("\nnext steps: ask the agent to refresh stale pages and fix lint issues, or run `/kms lint <name>` again after edits.\n");
    out
}

/// Render a `MigrationReport` from `kms::migrate`. Three shapes —
/// empty steps (already at latest), dry-run preview, applied summary.
pub fn format_migration_report(name: &str, report: &MigrationReport) -> String {
    let mode = if report.dry_run { "plan" } else { "applied" };
    if report.steps.is_empty() {
        return format!(
            "KMS '{name}': already at schema version {} — nothing to migrate.",
            report.target_version
        );
    }
    let mut out = format!(
        "KMS '{name}': migration {mode} ({} → {}, {} step(s))\n",
        report.current_version,
        report.target_version,
        report.steps.len()
    );
    for step in &report.steps {
        out.push_str(&format!("\n{} → {}:\n", step.from, step.to));
        for action in &step.actions {
            out.push_str(&format!("  - {action}\n"));
        }
    }
    if report.dry_run {
        out.push_str("\nthis was a dry-run preview. re-run with `--apply` to execute.\n");
    } else {
        out.push_str("\nlogged to log.md. /kms lint to verify.\n");
    }
    out
}

/// Build the `kms_update` envelope the frontend's KMS sidebar
/// consumes. M6.36 SERVE9c — moved from `gui.rs` to an always-on
/// module so the WS transport's `kms_list` IPC arm can call it from
/// `crate::ipc::handle_ipc`. Same JSON shape both transports emit.
pub fn build_update_payload() -> serde_json::Value {
    let active: std::collections::HashSet<String> = crate::config::ProjectConfig::load()
        .and_then(|c| c.kms.map(|k| k.active))
        .unwrap_or_default()
        .into_iter()
        .collect();
    let kmss: Vec<serde_json::Value> = list_all()
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "name": k.name,
                "scope": k.scope.as_str(),
                "active": active.contains(&k.name),
            })
        })
        .collect();
    serde_json::json!({
        "type": "kms_update",
        "kmss": kmss,
    })
}

/// Test-only lock shared by every test in this module *and* in
/// `tools::kms` that mutates the process env (HOME, cwd). Without
/// this, parallel tests race on env — which can also break unrelated
/// tests (bash/grep) whose sandbox resolver reads cwd.
#[cfg(test)]
pub(crate) fn test_env_lock() -> std::sync::MutexGuard<'static, ()> {
    use std::sync::{Mutex, OnceLock};
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_userprofile: Option<String>,
        prev_cwd: std::path::PathBuf,
        _home_dir: tempfile::TempDir,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // Restore cwd first — set_current_dir against a dropped
            // tempdir would fail silently otherwise.
            let _ = std::env::set_current_dir(&self.prev_cwd);
            match &self.prev_home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
            match &self.prev_userprofile {
                Some(h) => std::env::set_var("USERPROFILE", h),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    /// Acquire exclusive access to the process env + cwd for this
    /// test, set HOME (+ USERPROFILE on Windows) to a fresh tempdir,
    /// leave cwd pointing at that tempdir. Dropped at end of test to
    /// restore.
    fn scoped_home() -> EnvGuard {
        let lock = test_env_lock();
        let prev_home = std::env::var("HOME").ok();
        let prev_userprofile = std::env::var("USERPROFILE").ok();
        let prev_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::set_var("USERPROFILE", dir.path());
        std::env::set_current_dir(dir.path()).unwrap();
        EnvGuard {
            _lock: lock,
            prev_home,
            prev_userprofile,
            prev_cwd,
            _home_dir: dir,
        }
    }

    #[test]
    fn create_seeds_starter_files() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::User).unwrap();
        assert!(k.index_path().exists());
        assert!(k.log_path().exists());
        assert!(k.schema_path().exists());
        assert!(k.pages_dir().is_dir());
    }

    #[test]
    fn create_is_idempotent() {
        let _home = scoped_home();
        let a = create("notes", KmsScope::User).unwrap();
        let b = create("notes", KmsScope::User).unwrap();
        assert_eq!(a.root, b.root);
    }

    #[test]
    fn create_rejects_path_traversal() {
        let _home = scoped_home();
        assert!(create("../evil", KmsScope::User).is_err());
        assert!(create("foo/bar", KmsScope::User).is_err());
    }

    #[test]
    fn resolve_prefers_project_over_user() {
        let _home = scoped_home();
        create("shared", KmsScope::User).unwrap();
        create("shared", KmsScope::Project).unwrap();
        let found = resolve("shared").unwrap();
        assert_eq!(found.scope, KmsScope::Project);
    }

    #[test]
    fn list_all_returns_project_then_user() {
        let _home = scoped_home();
        create("user-only", KmsScope::User).unwrap();
        create("proj-only", KmsScope::Project).unwrap();
        let all = list_all();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].scope, KmsScope::Project);
        assert_eq!(all[1].scope, KmsScope::User);
    }

    #[test]
    fn system_prompt_section_empty_when_no_active() {
        let _home = scoped_home();
        assert_eq!(system_prompt_section(&[]), "");
    }

    #[test]
    fn system_prompt_section_includes_index_text() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.index_path(), "# nb\n- [foo](pages/foo.md) — foo page\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("## KMS: nb"));
        assert!(out.contains("foo page"));
        assert!(out.contains("KmsRead"));
    }

    /// M6.39.5: pin the strong-imperative wording of the prelude.
    /// User reported via /system inspection that even when KMS was
    /// active and the index summary was descriptive, the LLM still
    /// answered from training data. Pre-fix prelude said "consult
    /// them before answering" — soft language. This test locks the
    /// directive form so a future "smooth out the wording" refactor
    /// can't regress it.
    #[test]
    fn system_prompt_section_uses_mandatory_consultation_directive() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.index_path(), "# nb\n- [foo](pages/foo.md) — foo\n").unwrap();
        let out = system_prompt_section(&["nb".into()]);
        // MUST include the strong imperative form
        assert!(
            out.contains("MANDATORY"),
            "prelude must use MANDATORY (got soft 'consult'-style wording)"
        );
        // MUST name the tool call sequence explicitly — `KmsSearch`
        // first, then `KmsRead`, then answer. This is the procedure
        // the model needs to follow.
        assert!(out.contains("KmsSearch"));
        assert!(out.contains("KmsRead"));
        // MUST forbid the shortcut (answering from training when KMS
        // could match). Without this the model rationalizes skipping
        // ("I already know the answer").
        let lower = out.to_ascii_lowercase();
        assert!(
            lower.contains("do not skip"),
            "prelude must forbid skipping the lookup steps"
        );
        // MUST acknowledge the no-match fallback so the model doesn't
        // feel boxed in when KMS genuinely has nothing.
        assert!(
            lower.contains("fall back to training-data knowledge"),
            "prelude must allow training-data fallback when KMS has no hits"
        );
    }

    #[test]
    fn system_prompt_section_skips_missing() {
        let _home = scoped_home();
        let out = system_prompt_section(&["does-not-exist".into()]);
        assert_eq!(out, "");
    }

    #[test]
    fn page_path_rejects_traversal() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(k.page_path("../../etc/passwd").is_err());
        assert!(k.page_path("/etc/passwd").is_err());
        assert!(k.page_path("foo/bar").is_err()); // path separator
        assert!(k.page_path("").is_err()); // empty name
        assert!(k.page_path("foo\0bar").is_err()); // null byte

        // The happy path: create the file first (page_path now requires
        // the file to exist so it can canonicalize + symlink-check).
        std::fs::write(k.pages_dir().join("ok-page.md"), "body").unwrap();
        assert!(k.page_path("ok-page").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn page_path_rejects_symlink_to_outside() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();

        // Attacker plants a symlink in pages/ to an outside target.
        let target_dir = tempfile::tempdir().unwrap();
        let outside_file = target_dir.path().join("secret.md");
        std::fs::write(&outside_file, "top secret").unwrap();
        let symlink_path = k.pages_dir().join("leaked.md");
        symlink(&outside_file, &symlink_path).unwrap();

        // Despite the file existing (via symlink), page_path rejects
        // because canonical candidate escapes the KMS root.
        let result = k.page_path("leaked");
        assert!(result.is_err(), "expected symlink to be rejected");
        let err_str = format!("{}", result.unwrap_err());
        assert!(
            err_str.contains("symlink escape") || err_str.contains("outside the KMS"),
            "unexpected error: {err_str}"
        );
    }

    /// M6.25 BUG #2: ingest now SPLITS source from page. Raw content
    /// lands in `sources/<alias>.<ext>`; a stub page with frontmatter
    /// lands in `pages/<alias>.md` pointing at it. Verifies the new
    /// shape end-to-end.
    #[test]
    fn ingest_splits_source_from_page() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("intro.md");
        std::fs::write(&src, "# Intro\n\nFirst real line of content.\n").unwrap();

        let result = ingest(&k, &src, None, false).unwrap();
        assert_eq!(result.alias, "intro");
        assert!(!result.overwrote);
        assert!(result.target.exists());
        // The target is the page stub, not the raw source.
        assert!(result.target.ends_with("pages/intro.md"));

        // Raw source lives under sources/ — verbatim.
        let source_copy = k.root.join("sources/intro.md");
        let raw = std::fs::read_to_string(&source_copy).unwrap();
        assert!(raw.contains("First real line"));

        // Page is a stub with frontmatter pointing back at the source.
        let page_body = std::fs::read_to_string(&result.target).unwrap();
        let (fm, body) = parse_frontmatter(&page_body);
        assert_eq!(fm.get("sources").map(String::as_str), Some("intro"));
        assert_eq!(
            fm.get("category").map(String::as_str),
            Some("uncategorized")
        );
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("Stub page"));
        assert!(body.contains("sources/intro.md"));

        // Index.md now has a bullet pointing at the page.
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(
            index.contains("- [intro](pages/intro.md)"),
            "index missing bullet, got:\n{index}"
        );

        // M6.25 BUG #7: log uses `## [date] verb | alias` header form.
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(
            log.contains("## [") && log.contains("] ingested | intro"),
            "log missing header-style entry, got:\n{log}"
        );
    }

    #[test]
    fn ingest_collides_without_force() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("page.md");
        std::fs::write(&src, "a").unwrap();

        ingest(&k, &src, Some("topic"), false).unwrap();
        let err = ingest(&k, &src, Some("topic"), false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("already exists"),
            "expected collision, got: {msg}"
        );

        // --force replaces, and is flagged as overwrote. The raw source
        // copy carries the new bytes; the page stub is regenerated.
        std::fs::write(&src, "b").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert!(r.overwrote);
        let raw = std::fs::read_to_string(k.root.join("sources/topic.md")).unwrap();
        assert_eq!(raw, "b");
    }

    #[test]
    fn ingest_rejects_unknown_extension() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("bin.xyz");
        std::fs::write(&src, "data").unwrap();
        let err = ingest(&k, &src, None, false).unwrap_err();
        assert!(format!("{err}").contains("not supported"));
    }

    #[test]
    fn ingest_rejects_reserved_alias() {
        let _home = scoped_home();
        let k = create("notes", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("file.md");
        std::fs::write(&src, "x").unwrap();
        let err = ingest(&k, &src, Some("index"), false).unwrap_err();
        assert!(format!("{err}").contains("reserved"));
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_kms_dir() {
        use std::os::unix::fs::symlink;
        let _home = scoped_home();

        // Attacker plants a symlink where a KMS dir should be.
        let target = tempfile::tempdir().unwrap();
        let kms_root = scope_root(KmsScope::User).unwrap();
        std::fs::create_dir_all(&kms_root).unwrap();
        symlink(target.path(), kms_root.join("evil")).unwrap();

        // resolve() should not return a KmsRef for a symlinked dir.
        assert!(
            resolve("evil").is_none(),
            "symlinked KMS dir should be rejected"
        );
    }

    // ─── M6.25: frontmatter (BUG #9) ──────────────────────────────────────

    // ─── M6.39.13: graph builder ──────────────────────────────────────────

    #[test]
    fn graph_extracts_wikilink_targets() {
        let body = "see [[alpha]] and [[beta|Beta Display]]\nrandom [text](http://x).\n[[gamma]]";
        let targets = extract_wikilink_targets(body);
        assert_eq!(targets, vec!["alpha", "beta", "gamma"]);
    }

    #[test]
    fn graph_skips_dangling_and_self_links() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "alpha",
            "---\ntitle: \"Alpha\"\n---\n\nlinks to [[beta]] and [[ghost]] and self [[alpha]]\n",
        )
        .unwrap();
        write_page(
            &k,
            "beta",
            "---\ntitle: \"Beta\"\n---\n\nback to [[alpha]]\n",
        )
        .unwrap();
        let g = graph("nb", false).expect("graph");
        let ids: Vec<_> = g.nodes.iter().map(|n| n.id.clone()).collect();
        assert!(ids.contains(&"alpha".to_string()));
        assert!(ids.contains(&"beta".to_string()));
        assert!(!ids.contains(&"ghost".to_string()));
        // alpha → beta + beta → alpha; alpha → ghost dropped (dangling);
        // alpha → alpha dropped (self-link).
        assert_eq!(g.edges.len(), 2);
        let alpha = g.nodes.iter().find(|n| n.id == "alpha").unwrap();
        assert_eq!(alpha.label, "Alpha");
        assert_eq!(alpha.kind, GraphNodeKind::Page);
    }

    #[test]
    fn graph_extracts_source_link_targets() {
        let body = "see [1](../sources/foo.md) and [2](../sources/bar) and [3](../sources/baz.md#x)\n[ignore](other/path.md)";
        let targets = extract_source_link_targets(body);
        assert_eq!(targets, vec!["foo", "bar", "baz"]);
    }

    #[test]
    fn graph_includes_sources_when_requested() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        write_page(
            &k,
            "alpha",
            "---\ntitle: \"Alpha\"\n---\n\nciting [1](../sources/example-com.md) and [2](../sources/ghost-source.md)\n",
        )
        .unwrap();
        // Create a sources/ archive that the page cites.
        let sources_dir = k.root.join("sources");
        std::fs::create_dir_all(&sources_dir).unwrap();
        std::fs::write(
            sources_dir.join("example-com.md"),
            "---\ntitle: \"Example Inc.\"\n---\n\nbody\n",
        )
        .unwrap();
        // Note: ghost-source.md does NOT exist on disk — should be dropped.

        // Without flag: only the page node, no source nodes/edges.
        let g_off = graph("nb", false).expect("graph");
        assert_eq!(g_off.nodes.len(), 1);
        assert!(g_off.edges.is_empty());

        // With flag: page node + 1 source node + 1 page→source edge
        // (the dangling ghost-source citation is dropped).
        let g_on = graph("nb", true).expect("graph");
        assert_eq!(g_on.nodes.len(), 2);
        let src = g_on
            .nodes
            .iter()
            .find(|n| n.kind == GraphNodeKind::Source)
            .expect("source node");
        assert_eq!(src.id, "source:example-com");
        assert_eq!(src.label, "Example Inc.");
        assert_eq!(g_on.edges.len(), 1);
        assert_eq!(g_on.edges[0].source, "alpha");
        assert_eq!(g_on.edges[0].target, "source:example-com");
    }

    #[test]
    fn parse_frontmatter_extracts_keys_and_strips_block() {
        let s = "---\ncategory: research\ntags: ai\nsources: paper-x\n---\n# Body\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert_eq!(fm.get("category").map(String::as_str), Some("research"));
        assert_eq!(fm.get("tags").map(String::as_str), Some("ai"));
        assert_eq!(fm.get("sources").map(String::as_str), Some("paper-x"));
        assert_eq!(body, "# Body\n\nHello.\n");
    }

    #[test]
    fn parse_frontmatter_no_block_returns_empty_and_original() {
        let s = "# No frontmatter\n\nHello.\n";
        let (fm, body) = parse_frontmatter(s);
        assert!(fm.is_empty());
        assert_eq!(body, s);
    }

    #[test]
    fn write_frontmatter_round_trips() {
        let mut fm = std::collections::BTreeMap::new();
        fm.insert("category".into(), "research".into());
        fm.insert("note".into(), "has: colon".into()); // forces quoting
        let serialized = write_frontmatter(&fm, "body text\n");
        let (parsed, body) = parse_frontmatter(&serialized);
        assert_eq!(parsed.get("category").map(String::as_str), Some("research"));
        assert_eq!(parsed.get("note").map(String::as_str), Some("has: colon"));
        assert_eq!(body, "body text\n");
    }

    // ─── M6.25: write_page + append_to_page (BUG #1) ──────────────────────

    #[test]
    fn write_page_creates_with_stamps_and_index_bullet() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "# Topic\n\nBody.\n").unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert!(fm.contains_key("created"), "created stamp missing");
        assert!(fm.contains_key("updated"), "updated stamp missing");
        assert!(body.contains("Body."));
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index.contains("- [topic](pages/topic.md)"));
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(log.contains("] wrote | topic"));
    }

    #[test]
    fn write_page_replace_preserves_created_bumps_updated() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let path = write_page(&k, "topic", "v1").unwrap();
        let raw1 = std::fs::read_to_string(&path).unwrap();
        let (fm1, _) = parse_frontmatter(&raw1);
        let created = fm1.get("created").cloned().unwrap();

        // Write again with explicit created override that should win.
        let _ = write_page(&k, "topic", "---\ncreated: 1999-01-01\n---\nv2").unwrap();
        let raw2 = std::fs::read_to_string(&path).unwrap();
        let (fm2, body2) = parse_frontmatter(&raw2);
        // User-supplied frontmatter wins on conflict.
        assert_eq!(fm2.get("created").map(String::as_str), Some("1999-01-01"));
        // updated still gets a stamp.
        assert!(fm2.contains_key("updated"));
        assert_eq!(body2, "v2");
        // Index has exactly one entry for `topic` (no duplicates).
        let index = std::fs::read_to_string(k.index_path()).unwrap();
        let count = index.matches("(pages/topic.md)").count();
        assert_eq!(count, 1, "expected one entry, got {count}\n{index}");
        // Sanity: original `created` was today, the override moved it.
        assert_ne!(created, "1999-01-01");
    }

    #[test]
    fn append_to_page_creates_then_appends_with_frontmatter_bump() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        // First call creates with bare body (no frontmatter).
        append_to_page(&k, "log-page", "first chunk\n").unwrap();
        // Now write a frontmatter version then append more.
        write_page(&k, "log-page", "---\ncategory: log\n---\noriginal\n").unwrap();
        append_to_page(&k, "log-page", "second chunk\n").unwrap();
        let path = k.pages_dir().join("log-page.md");
        let raw = std::fs::read_to_string(&path).unwrap();
        let (fm, body) = parse_frontmatter(&raw);
        assert_eq!(fm.get("category").map(String::as_str), Some("log"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("original"));
        assert!(body.contains("second chunk"));
    }

    #[test]
    fn writable_page_path_rejects_traversal_and_reserved() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        assert!(writable_page_path(&k, "../etc/passwd").is_err());
        assert!(writable_page_path(&k, "foo/bar").is_err());
        assert!(writable_page_path(&k, "").is_err());
        assert!(writable_page_path(&k, "index").is_err()); // reserved
        assert!(writable_page_path(&k, "log").is_err());
        assert!(writable_page_path(&k, "SCHEMA").is_err());
        assert!(writable_page_path(&k, "ok-page").is_ok());
    }

    // ─── M6.25: lint (BUG #3) ─────────────────────────────────────────────

    #[test]
    fn lint_finds_orphans_broken_links_and_missing_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Page A links to non-existent target → broken link.
        // Page B has no inbound links → orphan.
        // Page C has no frontmatter → flagged.
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink: [nope](pages/missing.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: y\n---\nIsland.\n",
        )
        .unwrap();
        std::fs::write(k.pages_dir().join("c.md"), "no frontmatter here\n").unwrap();

        let report = lint(&k).unwrap();
        assert!(report
            .broken_links
            .iter()
            .any(|(p, t)| p == "a" && t == "missing"));
        assert!(report.orphan_pages.contains(&"b".to_string()));
        assert!(report.missing_frontmatter.contains(&"c".to_string()));
        assert!(report.total_issues() >= 3);
    }

    #[test]
    fn lint_clean_kms_reports_no_issues() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink to [b](pages/b.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: x\n---\nLink to [a](pages/a.md)\n",
        )
        .unwrap();
        std::fs::write(k.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();
        let report = lint(&k).unwrap();
        assert_eq!(report.total_issues(), 0, "{report:?}");
    }

    // ─── M6.25: SCHEMA injection in system prompt (BUG #5) ────────────────

    #[test]
    fn system_prompt_includes_schema_when_present() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.schema_path(),
            "Pages must have category: in frontmatter.\n",
        )
        .unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("### Schema"));
        assert!(out.contains("Pages must have category"));
        assert!(out.contains("KmsWrite")); // tool affordance listed
        assert!(out.contains("KmsAppend"));
    }

    /// M6.38.2 audit fix (Bug B): KmsDelete is registered alongside the
    /// other write tools when a KMS is active. Before this fix the system
    /// prompt's Tools block omitted KmsDelete — the model had access to
    /// the tool via the registry but no narrative context for when to use
    /// it. Now it's listed with a "last resort" hint to bias the model
    /// toward KmsWrite for merge/supersede flows.
    #[test]
    fn system_prompt_tools_block_includes_kms_delete() {
        let _home = scoped_home();
        let _k = create("nb", KmsScope::User).unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(out.contains("### Tools"));
        assert!(out.contains("KmsRead"));
        assert!(out.contains("KmsSearch"));
        assert!(out.contains("KmsWrite"));
        assert!(out.contains("KmsAppend"));
        assert!(
            out.contains("KmsDelete"),
            "Tools block should list KmsDelete (M6.38.2 fix). Got:\n{out}"
        );
        // The "last resort" framing biases the model away from default
        // deletion behavior — locks the prompt's stance.
        assert!(
            out.contains("last resort") || out.contains("prefer KmsWrite"),
            "KmsDelete entry should bias model toward KmsWrite for merges. Got:\n{out}"
        );
    }

    // ─── M6.25: categorized index (BUG #6) ────────────────────────────────

    #[test]
    fn system_prompt_categorizes_index_by_frontmatter() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(
            k.pages_dir().join("paper-a.md"),
            "---\ncategory: research\n---\n# Paper A\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("api-x.md"),
            "---\ncategory: api\n---\n# API X\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("paper-b.md"),
            "---\ncategory: research\n---\n# Paper B\n",
        )
        .unwrap();
        let out = system_prompt_section(&["nb".into()]);
        assert!(
            out.contains("**research**"),
            "missing research section: {out}"
        );
        assert!(out.contains("**api**"), "missing api section: {out}");
        assert!(out.contains("paper-a"));
        assert!(out.contains("paper-b"));
        assert!(out.contains("api-x"));
    }

    // ─── M6.25: re-ingest cascade (BUG #10) ───────────────────────────────

    #[test]
    fn reingest_marks_dependent_pages_stale() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Ingest source `topic`.
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("topic.md");
        std::fs::write(&src, "v1").unwrap();
        ingest(&k, &src, Some("topic"), false).unwrap();

        // Write a derived page that mentions `topic` in `sources:`.
        write_page(
            &k,
            "summary",
            "---\ncategory: synthesis\nsources: topic\n---\n# Summary\n",
        )
        .unwrap();

        // Re-ingest topic with --force → cascade fires.
        std::fs::write(&src, "v2").unwrap();
        let r = ingest(&k, &src, Some("topic"), true).unwrap();
        assert_eq!(r.cascaded, 1, "expected 1 dependent page marked stale");

        let derived = std::fs::read_to_string(k.pages_dir().join("summary.md")).unwrap();
        assert!(derived.contains("STALE"), "stale marker missing: {derived}");
        assert!(derived.contains("source `topic`"));
    }

    // ─── manifest + schema-aware lint ─────────────────────────────────────

    #[test]
    fn create_seeds_manifest_with_empty_required() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let manifest = k.read_manifest().expect("manifest seeded by create()");
        assert_eq!(manifest.schema_version, KMS_SCHEMA_VERSION);
        assert!(
            manifest.frontmatter_required.is_empty(),
            "starter manifest must not enforce policy by default"
        );
    }

    #[test]
    fn read_manifest_returns_none_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert!(k.read_manifest().is_none());
    }

    #[test]
    fn read_manifest_returns_none_for_malformed_json() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.manifest_path(), "{ this is not json").unwrap();
        assert!(k.read_manifest().is_none());
    }

    #[test]
    fn read_manifest_round_trips_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into(), "tags".into()]);
        required.insert("research".into(), vec!["sources".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        let read = k.read_manifest().unwrap();
        assert_eq!(read.schema_version, "1.0");
        assert_eq!(
            read.frontmatter_required.get("global").unwrap(),
            &vec!["category".to_string(), "tags".to_string()]
        );
        assert_eq!(
            read.frontmatter_required.get("research").unwrap(),
            &vec!["sources".to_string()]
        );
    }

    #[test]
    fn lint_skips_required_check_when_manifest_has_empty_map() {
        // The starter manifest is present but enforcement is empty — must
        // behave identically to legacy KMSes for required-field reporting.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn lint_skips_required_check_when_manifest_absent() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn lint_finds_missing_global_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into(), "tags".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "---\ncategory: x\n---\nbody\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report
                .missing_required_fields
                .iter()
                .any(|(p, src, f)| p == "a" && src == "global" && f == "tags"),
            "expected missing 'tags' on page 'a': {:?}",
            report.missing_required_fields
        );
        // 'category' is present on the page so must NOT appear.
        assert!(!report
            .missing_required_fields
            .iter()
            .any(|(_, _, f)| f == "category"));
    }

    #[test]
    fn lint_finds_missing_per_category_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("research".into(), vec!["sources".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        // Research page without `sources:` → flagged.
        std::fs::write(
            k.pages_dir().join("paper.md"),
            "---\ncategory: research\n---\nbody\n",
        )
        .unwrap();
        // Non-research page without `sources:` → NOT flagged (rule is
        // category-scoped, not global).
        std::fs::write(
            k.pages_dir().join("note.md"),
            "---\ncategory: misc\n---\nbody\n",
        )
        .unwrap();
        let report = lint(&k).unwrap();
        assert!(
            report
                .missing_required_fields
                .iter()
                .any(|(p, src, f)| p == "paper" && src == "research" && f == "sources"),
            "expected research/sources flag on 'paper': {:?}",
            report.missing_required_fields
        );
        assert!(!report
            .missing_required_fields
            .iter()
            .any(|(p, _, _)| p == "note"));
    }

    #[test]
    fn lint_skips_required_check_for_pages_with_no_frontmatter() {
        // A page with no `---` block is already flagged via
        // `missing_frontmatter`. Don't double-report by also emitting
        // every required field as missing — the user fixes the
        // frontmatter once and both classes resolve.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["category".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        std::fs::write(k.pages_dir().join("bare.md"), "no frontmatter\n").unwrap();
        let report = lint(&k).unwrap();
        assert!(report.missing_frontmatter.contains(&"bare".to_string()));
        assert!(report.missing_required_fields.is_empty());
    }

    #[test]
    fn scan_stale_markers_finds_cascade_output() {
        // End-to-end: ingest a source, write a derived page that references
        // it, re-ingest with --force to trigger the cascade, then verify
        // scan_stale_markers picks up exactly what mark_dependent_pages_stale
        // wrote. Locks the producer/consumer marker contract.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let src_dir = tempfile::tempdir().unwrap();
        let src = src_dir.path().join("topic.md");
        std::fs::write(&src, "v1").unwrap();
        ingest(&k, &src, Some("topic"), false).unwrap();
        write_page(
            &k,
            "summary",
            "---\ncategory: synthesis\nsources: topic\n---\n# Summary\n",
        )
        .unwrap();
        std::fs::write(&src, "v2").unwrap();
        ingest(&k, &src, Some("topic"), true).unwrap();

        let stale = scan_stale_markers(&k).unwrap();
        assert_eq!(stale.len(), 1, "expected 1 stale marker: {stale:?}");
        assert_eq!(stale[0].page_stem, "summary");
        assert_eq!(stale[0].source_alias, "topic");
        assert!(!stale[0].date.is_empty(), "date must be captured");
    }

    #[test]
    fn scan_stale_markers_returns_empty_when_no_markers() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("clean.md"),
            "---\ncategory: x\n---\nNo markers here.\n",
        )
        .unwrap();
        assert!(scan_stale_markers(&k).unwrap().is_empty());
    }

    #[test]
    fn scan_stale_markers_collects_multiple_per_page() {
        // A page that has been left stale across two re-ingest waves
        // should surface both markers — refresh debt accumulates.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::write(
            k.pages_dir().join("debt.md"),
            "---\ncategory: synthesis\n---\nbody\n\n\
             > ⚠ STALE: source `alpha` was re-ingested on 2026-01-01. Refresh this page.\n\
             > ⚠ STALE: source `beta` was re-ingested on 2026-02-15. Refresh this page.\n",
        )
        .unwrap();
        let stale = scan_stale_markers(&k).unwrap();
        assert_eq!(stale.len(), 2);
        // Sorted by (stem, alias, date) — alpha before beta.
        assert_eq!(stale[0].source_alias, "alpha");
        assert_eq!(stale[0].date, "2026-01-01");
        assert_eq!(stale[1].source_alias, "beta");
        assert_eq!(stale[1].date, "2026-02-15");
    }

    // ─── schema migrations ────────────────────────────────────────────────

    #[test]
    fn detect_schema_version_returns_legacy_when_manifest_absent() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert_eq!(detect_schema_version(&k), LEGACY_SCHEMA_VERSION);
    }

    #[test]
    fn detect_schema_version_returns_legacy_when_version_field_empty() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Manifest exists but schema_version is empty — same legacy treatment.
        std::fs::write(
            k.manifest_path(),
            r#"{"schema_version": "", "frontmatter_required": {}}"#,
        )
        .unwrap();
        assert_eq!(detect_schema_version(&k), LEGACY_SCHEMA_VERSION);
    }

    #[test]
    fn detect_schema_version_reads_explicit_version() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Default seed is "1.0".
        assert_eq!(detect_schema_version(&k), "1.0");
    }

    #[test]
    fn migrate_is_noop_when_already_at_latest() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let report = migrate(&k, false).unwrap();
        assert_eq!(report.current_version, "1.0");
        assert_eq!(report.target_version, "1.0");
        assert!(report.steps.is_empty());
    }

    #[test]
    fn migrate_dry_run_writes_no_files_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        let log_before = std::fs::read_to_string(k.log_path()).unwrap();

        let report = migrate(&k, true).unwrap();
        assert!(report.dry_run);
        assert_eq!(report.current_version, LEGACY_SCHEMA_VERSION);
        assert_eq!(report.target_version, "1.0");
        assert_eq!(report.steps.len(), 1);
        assert_eq!(report.steps[0].from, LEGACY_SCHEMA_VERSION);
        assert_eq!(report.steps[0].to, "1.0");

        // No filesystem changes.
        assert!(!k.manifest_path().exists(), "dry-run wrote manifest");
        let log_after = std::fs::read_to_string(k.log_path()).unwrap();
        assert_eq!(log_before, log_after, "dry-run touched log.md");
    }

    #[test]
    fn migrate_apply_writes_manifest_for_legacy_kms() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        assert!(!k.manifest_path().exists());

        let report = migrate(&k, false).unwrap();
        assert!(!report.dry_run);
        assert_eq!(report.steps.len(), 1);

        // Manifest now exists at v1.0 with empty enforcement.
        let manifest = k.read_manifest().expect("manifest written");
        assert_eq!(manifest.schema_version, "1.0");
        assert!(manifest.frontmatter_required.is_empty());

        // Log entry was appended.
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(
            log.contains("migrated | 0.x → 1.0"),
            "log missing migration entry: {log}"
        );

        // Idempotent: a second migrate is a no-op.
        let report2 = migrate(&k, false).unwrap();
        assert!(report2.steps.is_empty());
        assert_eq!(report2.current_version, "1.0");
    }

    #[test]
    fn migrate_preserves_existing_pages() {
        // Migration must not touch page bodies — only the manifest changes.
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        std::fs::remove_file(k.manifest_path()).unwrap();
        let page_path = k.pages_dir().join("preserve.md");
        let original = "---\ncategory: x\n---\nimportant content\n";
        std::fs::write(&page_path, original).unwrap();

        migrate(&k, false).unwrap();

        let after = std::fs::read_to_string(&page_path).unwrap();
        assert_eq!(after, original, "page body modified by migration");
    }

    #[test]
    fn migrate_errors_on_unknown_schema_version() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // Plant a manifest with a version that has no migration path.
        std::fs::write(
            k.manifest_path(),
            r#"{"schema_version": "99.0", "frontmatter_required": {}}"#,
        )
        .unwrap();
        let err = migrate(&k, false).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("no migration path") && msg.contains("99.0"),
            "expected unknown-version error: {msg}"
        );
    }

    #[test]
    fn lint_total_issues_includes_missing_required_fields() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let mut required = std::collections::BTreeMap::new();
        required.insert("global".into(), vec!["tags".into()]);
        let m = KmsManifest {
            schema_version: "1.0".into(),
            frontmatter_required: required,
        };
        std::fs::write(k.manifest_path(), serde_json::to_string_pretty(&m).unwrap()).unwrap();
        // Self-linked pages so we don't trip orphan/broken-link checks.
        std::fs::write(
            k.pages_dir().join("a.md"),
            "---\ncategory: x\n---\nLink to [b](pages/b.md)\n",
        )
        .unwrap();
        std::fs::write(
            k.pages_dir().join("b.md"),
            "---\ncategory: x\n---\nLink to [a](pages/a.md)\n",
        )
        .unwrap();
        std::fs::write(k.index_path(), "- [a](pages/a.md)\n- [b](pages/b.md)\n").unwrap();
        let report = lint(&k).unwrap();
        // Both pages missing 'tags' → 2 missing-required-field issues.
        assert_eq!(report.missing_required_fields.len(), 2);
        assert_eq!(report.total_issues(), 2);
    }
}
