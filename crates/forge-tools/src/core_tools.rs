//! The core coding tools shipped in v0.1.

use async_trait::async_trait;
use forge_types::SideEffect;
use serde_json::{json, Value};

use crate::{str_arg, Tool, ToolError};

/// Read a UTF-8 text file. Read-only — never prompts for permission.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a UTF-8 text file at the given path."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } },
            "required": ["path"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        Ok(tokio::fs::read_to_string(path).await?)
    }
}

/// Write (create/overwrite) a text file. Mutates the workspace.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file at the given path, creating or overwriting it."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        tokio::fs::write(path, content).await?;
        Ok(format!("wrote {} bytes to {path}", content.len()))
    }
}

/// Execute a shell command and capture its output. Highest-risk tool.
pub struct ShellTool;

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a shell command and return its combined stdout/stderr and exit status."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Shell
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "command": { "type": "string" } },
            "required": ["command"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let command = str_arg(args, "command")?;
        let output = tokio::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .output()
            .await?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        Ok(format!(
            "exit={}\n{stdout}{stderr}",
            output.status.code().unwrap_or(-1)
        ))
    }
}

/// Replace exactly one occurrence of `old` with `new` in a file. Mutates the workspace.
pub struct EditFileTool;

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &str {
        "edit_file"
    }
    fn description(&self) -> &str {
        "Replace exactly one occurrence of `old` with `new` in the file at `path`."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "old": { "type": "string" },
                "new": { "type": "string" }
            },
            "required": ["path", "old", "new"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let old = str_arg(args, "old")?;
        let new = str_arg(args, "new")?;

        let content = tokio::fs::read_to_string(path).await?;
        let occurrences = content.matches(old).count();
        match occurrences {
            0 => return Err(ToolError::Failed(format!("`old` not found in {path}"))),
            1 => {}
            n => {
                return Err(ToolError::Failed(format!(
                    "`old` is ambiguous: {n} occurrences in {path}"
                )))
            }
        }

        let updated = content.replacen(old, new, 1);
        tokio::fs::write(path, &updated).await?;
        Ok(format!("edited {path} (1 replacement)"))
    }
}

/// List the entries of a directory, sorted, directories marked with a trailing `/`.
pub struct ListDirTool;

#[async_trait]
impl Tool for ListDirTool {
    fn name(&self) -> &str {
        "list_dir"
    }
    fn description(&self) -> &str {
        "List the entries of a directory (directories marked with a trailing /)."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string" } }
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let meta = std::fs::metadata(path)?;
        if !meta.is_dir() {
            return Err(ToolError::Failed(format!("{path} is not a directory")));
        }
        let mut entries: Vec<String> = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_dir() {
                entries.push(format!("{name}/"));
            } else {
                entries.push(name);
            }
        }
        entries.sort();
        Ok(entries.join("\n"))
    }
}

/// Recursively search text files for a substring, returning `path:lineno: line` matches.
pub struct SearchTool;

const SEARCH_MATCH_CAP: usize = 50;

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Recursively search text files under `path` for lines containing `query`."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "path": { "type": "string" }
            },
            "required": ["query"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let query = str_arg(args, "query")?;
        let root = args.get("path").and_then(Value::as_str).unwrap_or(".");

        let mut matches: Vec<String> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                // Skip hidden dirs/files (incl .git) and build output dirs.
                if name.starts_with('.') || name == "target" {
                    continue;
                }
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    stack.push(path);
                } else if let Ok(content) = std::fs::read_to_string(&path) {
                    let rel = path.strip_prefix(root).unwrap_or(&path).display();
                    for (i, line) in content.lines().enumerate() {
                        if line.contains(query) {
                            matches.push(format!("{rel}:{}: {}", i + 1, line.trim_end()));
                            if matches.len() >= SEARCH_MATCH_CAP {
                                matches.push(format!("… (capped at {SEARCH_MATCH_CAP} matches)"));
                                return Ok(matches.join("\n"));
                            }
                        }
                    }
                }
            }
        }
        if matches.is_empty() {
            Ok(format!("no matches for '{query}'"))
        } else {
            Ok(matches.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("forge-tools-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn edit_file_replaces_a_unique_occurrence() {
        let dir = temp_dir("edit-unique");
        let path = dir.join("f.txt");
        std::fs::write(&path, "alpha BETA gamma").unwrap();

        EditFileTool
            .run(&json!({ "path": path.to_str().unwrap(), "old": "BETA", "new": "delta" }))
            .await
            .unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha delta gamma");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_file_errors_when_old_is_missing() {
        let dir = temp_dir("edit-missing");
        let path = dir.join("f.txt");
        std::fs::write(&path, "nothing here").unwrap();
        let err = EditFileTool
            .run(&json!({ "path": path.to_str().unwrap(), "old": "ZZZ", "new": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn edit_file_errors_when_old_is_ambiguous() {
        let dir = temp_dir("edit-ambiguous");
        let path = dir.join("f.txt");
        std::fs::write(&path, "dup dup").unwrap();
        let err = EditFileTool
            .run(&json!({ "path": path.to_str().unwrap(), "old": "dup", "new": "x" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
        // File must be unchanged on error.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dup dup");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_lists_sorted_with_dir_markers() {
        let dir = temp_dir("listdir");
        std::fs::write(dir.join("file.txt"), "x").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();

        let out = ListDirTool
            .run(&json!({ "path": dir.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(out, "file.txt\nsub/");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_dir_errors_on_non_directory() {
        let err = ListDirTool
            .run(&json!({ "path": "Cargo.toml" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Failed(_)));
    }

    #[tokio::test]
    async fn search_finds_matches_and_skips_target_and_git() {
        let dir = temp_dir("search");
        std::fs::write(dir.join("a.txt"), "hello\nfind ME here\nbye").unwrap();
        std::fs::create_dir(dir.join("target")).unwrap();
        std::fs::write(dir.join("target/t.txt"), "find ME").unwrap();
        std::fs::create_dir(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/g.txt"), "find ME").unwrap();

        let out = SearchTool
            .run(&json!({ "query": "find ME", "path": dir.to_str().unwrap() }))
            .await
            .unwrap();

        assert!(out.contains("a.txt:2: find ME here"), "got:\n{out}");
        assert!(!out.contains("target"), "must skip target/:\n{out}");
        assert!(!out.contains("g.txt"), "must skip .git/:\n{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_reads_workspace_manifest() {
        // The crate's own Cargo.toml is always present at test time.
        let out = ReadFileTool
            .run(&json!({ "path": "Cargo.toml" }))
            .await
            .unwrap();
        assert!(out.contains("forge-tools"));
    }

    #[tokio::test]
    async fn read_file_requires_path() {
        let err = ReadFileTool.run(&json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs(_)));
    }
}
