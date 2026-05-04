use super::Tool;
use crate::error::{Error, Result};
use async_trait::async_trait;
use serde_json::{json, Value};

/// M6.23 BUG LT1: hard cap on directory entries returned. Pre-fix `Ls`
/// built an unbounded `Vec<String>` from `std::fs::read_dir`. A
/// directory with hundreds of thousands of entries (large `node_modules`
/// trees flattened, build caches, mailbox dirs) would OOM the worker.
/// 1000 is generous enough for any human-navigable directory; users
/// who need to find a needle should use `Glob` with a pattern.
const MAX_LS_ENTRIES: usize = 1000;

pub struct LsTool;

#[async_trait]
impl Tool for LsTool {
    fn name(&self) -> &'static str {
        "Ls"
    }

    fn description(&self) -> &'static str {
        "List the immediate contents of a directory (files and subdirectories, \
         non-recursive). Use this for `list files` requests and general \
         directory exploration. For recursive pattern matching use `Glob` \
         instead. Directories are suffixed with `/`."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Directory path (default: current working directory)"
                }
            }
        })
    }

    async fn call(&self, input: Value) -> Result<String> {
        let raw = input.get("path").and_then(Value::as_str).unwrap_or(".");
        let path = crate::sandbox::Sandbox::check(raw)?;
        let entries = std::fs::read_dir(&path)
            .map_err(|e| Error::Tool(format!("ls {}: {e}", path.display())))?;

        // M6.23 BUG LT1: bound the in-memory entry collection so a
        // huge directory can't OOM the worker. Walk lazily and bail
        // when we hit the cap; surface the truncation so the user
        // knows to use Glob with a pattern instead.
        let mut items: Vec<String> = Vec::with_capacity(MAX_LS_ENTRIES);
        let mut total_seen = 0usize;
        for entry in entries.flatten() {
            total_seen += 1;
            if items.len() >= MAX_LS_ENTRIES {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            items.push(if is_dir { format!("{name}/") } else { name });
        }
        items.sort();
        let mut out = items.join("\n");
        if total_seen > MAX_LS_ENTRIES {
            out.push_str(&format!(
                "\n... ({} more entries, showing first {}; use Glob with a pattern for filtering)",
                total_seen - MAX_LS_ENTRIES,
                MAX_LS_ENTRIES,
            ));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn lists_immediate_contents_non_recursive() {
        let dir = tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/deep.txt"), "").unwrap();

        let out = LsTool
            .call(json!({"path": dir.path().to_string_lossy()}))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines, vec!["a.txt", "b.txt", "sub/"], "got: {out}");
    }

    #[tokio::test]
    async fn empty_directory_returns_empty_string() {
        let dir = tempdir().unwrap();
        let out = LsTool
            .call(json!({"path": dir.path().to_string_lossy()}))
            .await
            .unwrap();
        assert_eq!(out, "");
    }

    #[tokio::test]
    async fn nonexistent_path_errors() {
        let err = LsTool
            .call(json!({"path": "/nope/does/not/exist"}))
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("ls"));
    }

    #[tokio::test]
    async fn marks_subdirectories_with_trailing_slash() {
        let dir = tempdir().unwrap();
        std::fs::create_dir(dir.path().join("subdir")).unwrap();
        std::fs::write(dir.path().join("file"), "").unwrap();
        let out = LsTool
            .call(json!({"path": dir.path().to_string_lossy()}))
            .await
            .unwrap();
        assert!(out.contains("subdir/"));
        assert!(out.contains("file"));
        assert!(!out.contains("file/"));
    }

    /// M6.23 BUG LT1: huge directories must cap at MAX_LS_ENTRIES and
    /// surface a truncation notice with a hint to use Glob.
    #[tokio::test]
    async fn truncates_at_max_entries_with_notice() {
        let dir = tempdir().unwrap();
        // Create more entries than the cap so we exercise the truncation path.
        let n = MAX_LS_ENTRIES + 50;
        for i in 0..n {
            std::fs::write(dir.path().join(format!("f{i:05}.txt")), "").unwrap();
        }
        let out = LsTool
            .call(json!({"path": dir.path().to_string_lossy()}))
            .await
            .unwrap();
        let lines: Vec<&str> = out.lines().collect();
        // First MAX_LS_ENTRIES lines + one trailing notice line
        assert_eq!(lines.len(), MAX_LS_ENTRIES + 1, "expected cap + 1 notice");
        assert!(
            lines[MAX_LS_ENTRIES].contains("more entries"),
            "last line should announce truncation"
        );
        assert!(
            lines[MAX_LS_ENTRIES].contains("Glob"),
            "should suggest Glob for filtering"
        );
    }
}
