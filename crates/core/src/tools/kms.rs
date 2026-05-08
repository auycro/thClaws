//! KMS read, search, write, and append tools — pair with [`crate::kms`]
//! to let the model both consult AND maintain wiki pages without
//! embeddings.
//!
//! All tools resolve the `kms` argument via `kms::resolve`, which
//! prefers a project-scope KMS over a user-scope one on name collision.
//!
//! M6.25 BUG #1: `KmsWrite` + `KmsAppend` deliberately bypass
//! `Sandbox::check_write` to land inside the KMS root (project-scope
//! `.thclaws/kms/.../pages/...` is otherwise blocked by the sandbox).
//! Path safety is enforced at a finer grain via `kms::writable_page_path`,
//! which validates the page name and canonicalizes it inside the
//! resolved KMS pages dir. Same intentional carve-out pattern as
//! TodoWrite's `.thclaws/todos.md` write.

use super::{req_str, Tool};
use crate::error::{Error, Result};
use async_trait::async_trait;
use regex::Regex;
use serde_json::{json, Value};

pub struct KmsReadTool;

#[async_trait]
impl Tool for KmsReadTool {
    fn name(&self) -> &'static str {
        "KmsRead"
    }

    fn description(&self) -> &'static str {
        "Read a single page from an attached knowledge base. Use after \
         spotting a relevant entry in the KMS index that the user's \
         question touches on."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":  {"type": "string", "description": "KMS name (from the active list)"},
                "page": {"type": "string", "description": "Page name (with or without .md)"}
            },
            "required": ["kms", "page"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = kref.page_path(page)?;
        std::fs::read_to_string(&path)
            .map_err(|e| Error::Tool(format!("read {}: {e}", path.display())))
    }
}

pub struct KmsSearchTool;

#[async_trait]
impl Tool for KmsSearchTool {
    fn name(&self) -> &'static str {
        "KmsSearch"
    }

    fn description(&self) -> &'static str {
        "Grep across all pages in one knowledge base. Returns matching \
         lines as `page:line:text`. Use to locate a fact before reading \
         a whole page."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name"},
                "pattern": {"type": "string", "description": "Regex pattern"}
            },
            "required": ["kms", "pattern"]
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let pattern = req_str(&input, "pattern")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let re = Regex::new(pattern).map_err(|e| Error::Tool(format!("regex: {e}")))?;

        let pages_dir = kref.pages_dir();
        // Refuse to walk if `pages/` itself is a symlink. Entry-level
        // symlink filtering below can't save us from a `pages -> /etc`
        // symlink because /etc's contents aren't themselves symlinks.
        if let Ok(md) = std::fs::symlink_metadata(&pages_dir) {
            if md.file_type().is_symlink() {
                return Err(Error::Tool(format!(
                    "kms '{kms_name}' has a symlinked pages/ directory — refusing to read"
                )));
            }
        }
        let Ok(entries) = std::fs::read_dir(&pages_dir) else {
            return Ok(String::new());
        };

        let mut results: Vec<String> = Vec::new();
        for entry in entries.flatten() {
            // Skip symlinks to prevent `ln -s ~/.ssh/id_rsa pages/leak.md`
            // style exfiltration via grep.
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            let path = entry.path();
            if !path.extension().map(|e| e == "md").unwrap_or(false) {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(&path) else {
                continue;
            };
            let page_name = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            for (i, line) in contents.lines().enumerate() {
                if re.is_match(line) {
                    results.push(format!("{}:{}:{}", page_name, i + 1, line));
                }
            }
        }
        results.sort();
        Ok(results.join("\n"))
    }
}

/// M6.25 BUG #1: write a KMS page. Create-or-replace; if the content
/// includes YAML frontmatter (`---\n...\n---\n`), it's preserved and
/// `updated:` is bumped to today. New pages get `created:` stamped.
/// Updates the index.md bullet and appends a `## [date] wrote | alias`
/// log entry. Bypasses `Sandbox::check_write` for the KMS pages dir
/// (validated by `kms::writable_page_path`).
pub struct KmsWriteTool;

#[async_trait]
impl Tool for KmsWriteTool {
    fn name(&self) -> &'static str {
        "KmsWrite"
    }

    fn description(&self) -> &'static str {
        "Create or replace a page in an attached knowledge base. Content \
         may begin with YAML frontmatter (---\\n category: ... \\n---\\n) — \
         it's preserved and `updated:` is bumped to today. Use for \
         wiki maintenance: filing curated summaries, entity pages, \
         cross-referenced concept pages."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name (from the active list)"},
                "page":    {"type": "string", "description": "Page name (with or without .md). No path separators."},
                "content": {"type": "string", "description": "Full page content; may include YAML frontmatter."}
            },
            "required": ["kms", "page", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let content = req_str(&input, "content")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = crate::kms::write_page(&kref, page, content)?;
        Ok(format!(
            "wrote {} ({} bytes)",
            path.display(),
            content.len()
        ))
    }
}

/// M6.25 BUG #1: append to a KMS page. If the page exists with
/// frontmatter, only the body grows and `updated:` bumps. If no
/// frontmatter, plain append. If the page doesn't exist, creates it
/// with the given content (no frontmatter — model can rewrite via
/// KmsWrite to add metadata).
pub struct KmsAppendTool;

#[async_trait]
impl Tool for KmsAppendTool {
    fn name(&self) -> &'static str {
        "KmsAppend"
    }

    fn description(&self) -> &'static str {
        "Append content to a page in an attached knowledge base. \
         Faster than KmsWrite for incremental updates (logs, journal \
         entries, accumulating notes). Bumps `updated:` if the page \
         already has frontmatter."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":     {"type": "string", "description": "KMS name"},
                "page":    {"type": "string", "description": "Page name (with or without .md)"},
                "content": {"type": "string", "description": "Text chunk to append"}
            },
            "required": ["kms", "page", "content"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let content = req_str(&input, "content")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = crate::kms::append_to_page(&kref, page, content)?;
        Ok(format!(
            "appended {} bytes to {}",
            content.len(),
            path.display()
        ))
    }
}

/// Delete a single page from a KMS. Removes the file, strips its
/// bullet from `index.md`, and appends a `deleted | <stem>` log line.
/// Used during consolidation (`/dream`) to retire duplicates or stale
/// entries — gated on approval since it's destructive.
pub struct KmsDeleteTool;

#[async_trait]
impl Tool for KmsDeleteTool {
    fn name(&self) -> &'static str {
        "KmsDelete"
    }

    fn description(&self) -> &'static str {
        "Delete a single page from an attached knowledge base. \
         Removes the file, prunes the index.md bullet, and logs the \
         removal. Use during consolidation to retire duplicates or \
         stale entries — never as a casual cleanup."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "kms":  {"type": "string", "description": "KMS name (from the active list)"},
                "page": {"type": "string", "description": "Page name (with or without .md). No path separators."}
            },
            "required": ["kms", "page"]
        })
    }

    fn requires_approval(&self, _input: &Value) -> bool {
        true
    }

    async fn call(&self, input: Value) -> Result<String> {
        let kms_name = req_str(&input, "kms")?;
        let page = req_str(&input, "page")?;
        let Some(kref) = crate::kms::resolve(kms_name) else {
            return Err(Error::Tool(format!(
                "no KMS named '{kms_name}' (check /kms list)"
            )));
        };
        let path = crate::kms::delete_page(&kref, page)?;
        Ok(format!("deleted {}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kms::{create, KmsScope};

    struct EnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        prev_home: Option<String>,
        prev_userprofile: Option<String>,
        prev_cwd: std::path::PathBuf,
        _home_dir: tempfile::TempDir,
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
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

    fn scoped_home() -> EnvGuard {
        let lock = crate::kms::test_env_lock();
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

    #[tokio::test]
    async fn read_returns_page_contents() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("hello.md"), "hi from kms").unwrap();
        let out = KmsReadTool
            .call(json!({"kms": "nb", "page": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "hi from kms");
    }

    #[tokio::test]
    async fn read_resolves_missing_extension() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("x.md"), "x body").unwrap();
        let with = KmsReadTool
            .call(json!({"kms": "nb", "page": "x.md"}))
            .await
            .unwrap();
        let without = KmsReadTool
            .call(json!({"kms": "nb", "page": "x"}))
            .await
            .unwrap();
        assert_eq!(with, without);
    }

    #[tokio::test]
    async fn read_unknown_kms_errors() {
        let _home = scoped_home();
        let err = KmsReadTool
            .call(json!({"kms": "nope", "page": "x"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no KMS"));
    }

    #[tokio::test]
    async fn search_returns_page_line_matches() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::User).unwrap();
        std::fs::write(k.pages_dir().join("a.md"), "alpha\nbeta\nhello world\n").unwrap();
        std::fs::write(k.pages_dir().join("b.md"), "nothing here\n").unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "pattern": "hello"}))
            .await
            .unwrap();
        assert_eq!(out, "a:3:hello world");
    }

    #[tokio::test]
    async fn search_returns_empty_for_no_matches() {
        let _home = scoped_home();
        create("nb", KmsScope::User).unwrap();
        let out = KmsSearchTool
            .call(json!({"kms": "nb", "pattern": "absent"}))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    // ─── M6.25 BUG #1: write/append tools ─────────────────────────────────

    #[tokio::test]
    async fn write_tool_creates_page_with_stamps() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        let result = KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "topic",
                "content": "# Topic\n\nFresh page.\n"
            }))
            .await
            .unwrap();
        assert!(result.contains("wrote"));
        let raw = std::fs::read_to_string(k.pages_dir().join("topic.md")).unwrap();
        let (fm, body) = crate::kms::parse_frontmatter(&raw);
        assert!(fm.contains_key("created"));
        assert!(fm.contains_key("updated"));
        assert!(body.contains("Fresh page."));
    }

    #[tokio::test]
    async fn write_tool_unknown_kms_errors() {
        let _home = scoped_home();
        let err = KmsWriteTool
            .call(json!({"kms": "nope", "page": "x", "content": "y"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("no KMS"));
    }

    #[tokio::test]
    async fn write_tool_rejects_traversal() {
        let _home = scoped_home();
        create("nb", KmsScope::Project).unwrap();
        let err = KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "../escape",
                "content": "evil"
            }))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("invalid page name") || format!("{err}").contains(".."));
    }

    #[tokio::test]
    async fn append_tool_creates_then_extends() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        // First call creates with bare body.
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "log", "content": "line one\n"}))
            .await
            .unwrap_err(); // "log" is reserved
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "journal", "content": "line one\n"}))
            .await
            .unwrap();
        KmsAppendTool
            .call(json!({"kms": "nb", "page": "journal", "content": "line two\n"}))
            .await
            .unwrap();
        let raw = std::fs::read_to_string(k.pages_dir().join("journal.md")).unwrap();
        assert!(raw.contains("line one"));
        assert!(raw.contains("line two"));
    }

    #[tokio::test]
    async fn write_and_append_require_approval() {
        let _home = scoped_home();
        // Approval defaults are read off the trait; write tools must
        // require approval (they mutate disk) — same posture as Write.
        assert!(KmsWriteTool.requires_approval(&json!({})));
        assert!(KmsAppendTool.requires_approval(&json!({})));
        assert!(KmsDeleteTool.requires_approval(&json!({})));
    }

    #[tokio::test]
    async fn delete_removes_page_and_index_bullet() {
        let _home = scoped_home();
        let k = create("nb", KmsScope::Project).unwrap();
        KmsWriteTool
            .call(json!({
                "kms": "nb",
                "page": "doomed",
                "content": "to be deleted\n"
            }))
            .await
            .unwrap();
        let page_path = k.pages_dir().join("doomed.md");
        assert!(page_path.exists());
        let index_before = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(index_before.contains("(pages/doomed.md)"));
        let out = KmsDeleteTool
            .call(json!({"kms": "nb", "page": "doomed"}))
            .await
            .unwrap();
        assert!(out.starts_with("deleted"));
        assert!(!page_path.exists());
        let index_after = std::fs::read_to_string(k.index_path()).unwrap();
        assert!(!index_after.contains("(pages/doomed.md)"));
        let log = std::fs::read_to_string(k.log_path()).unwrap();
        assert!(log.contains("deleted | doomed"));
    }

    #[tokio::test]
    async fn delete_missing_page_errors() {
        let _home = scoped_home();
        let _ = create("nb", KmsScope::Project).unwrap();
        let err = KmsDeleteTool
            .call(json!({"kms": "nb", "page": "ghost"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("not found"));
    }

    #[tokio::test]
    async fn delete_rejects_reserved_names() {
        let _home = scoped_home();
        let _ = create("nb", KmsScope::Project).unwrap();
        // Same path-safety carve-out: index/log/SCHEMA cannot be deleted
        // through the tool.
        assert!(KmsDeleteTool
            .call(json!({"kms": "nb", "page": "index"}))
            .await
            .is_err());
        assert!(KmsDeleteTool
            .call(json!({"kms": "nb", "page": "log"}))
            .await
            .is_err());
    }
}
