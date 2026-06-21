//! The core coding tools shipped in v0.1.

use async_trait::async_trait;
use forge_types::{DiffKind, FileDiff, SideEffect};
use globset::{Glob, GlobMatcher};
use serde_json::{json, Value};

use crate::{str_arg, Tool, ToolError};

/// Map a file extension to a syntax-highlighting language token (best-effort; unknown
/// extensions pass through and fall back to plain highlighting downstream).
fn lang_from_path(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path).extension()?.to_str()?;
    let tok = match ext {
        "rs" => "rust",
        "py" => "python",
        "ts" | "tsx" => "typescript",
        "js" | "mjs" | "cjs" | "jsx" => "javascript",
        "go" => "go",
        "toml" => "toml",
        "json" => "json",
        "md" | "markdown" => "markdown",
        "sh" | "bash" => "bash",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" => "css",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        other => other,
    };
    Some(tok.to_string())
}

/// Read a UTF-8 text file. Supports optional line-range slicing.
pub struct ReadFileTool;

#[async_trait]
impl Tool for ReadFileTool {
    fn name(&self) -> &str {
        "read_file"
    }
    fn description(&self) -> &str {
        "Read the contents of a UTF-8 text file, returned verbatim (no line numbers — so the text \
         can be matched exactly by edit_file). Optionally slice to a line range with \
         `start_line`/`end_line` (both 1-indexed, inclusive). Very large files are truncated; pass \
         a line range to read a specific section. Always read a file before editing it."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "start_line": {
                    "type": "integer",
                    "description": "First line to read (1-indexed, inclusive). Default: 1."
                },
                "end_line": {
                    "type": "integer",
                    "description": "Last line to read (1-indexed, inclusive). Default: end of file."
                }
            },
            "required": ["path"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let start_line = args
            .get("start_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);
        let end_line = args
            .get("end_line")
            .and_then(Value::as_u64)
            .map(|n| n as usize);

        let content = tokio::fs::read_to_string(path).await?;
        let out = if start_line.is_none() && end_line.is_none() {
            content
        } else {
            let lines: Vec<&str> = content.lines().collect();
            let start = start_line.unwrap_or(1).saturating_sub(1); // 0-indexed
            let end = end_line.map(|e| e.min(lines.len())).unwrap_or(lines.len());
            lines[start.min(lines.len())..end].join("\n")
        };
        Ok(cap_read(out))
    }
}

/// Whitespace-insensitive fallback for `edit_file`: when `old` doesn't match the file byte-for-byte
/// (almost always a leading-indent / trailing-space difference), match it line-by-line ignoring each
/// line's surrounding whitespace. Returns the edited content ONLY when exactly one contiguous block
/// of lines matches (uniqueness preserved, so a near-miss can't hit the wrong place); otherwise
/// `None`. `new` is inserted verbatim, keeping the matched block's trailing newline.
fn flexible_replace(content: &str, old: &str, new: &str) -> Option<String> {
    let old_lines: Vec<&str> = old.lines().map(str::trim).collect();
    if old_lines.is_empty() {
        return None;
    }
    // Lines WITH their `\n` terminators, so byte offsets reconstruct exactly.
    let segs: Vec<&str> = content.split_inclusive('\n').collect();
    if old_lines.len() > segs.len() {
        return None;
    }
    let mut hits = Vec::new();
    for i in 0..=(segs.len() - old_lines.len()) {
        if (0..old_lines.len()).all(|j| segs[i + j].trim() == old_lines[j]) {
            hits.push(i);
        }
    }
    if hits.len() != 1 {
        return None; // not found, or ambiguous — caller errors
    }
    let i = hits[0];
    let start: usize = segs[..i].iter().map(|s| s.len()).sum();
    let end: usize = start
        + segs[i..i + old_lines.len()]
            .iter()
            .map(|s| s.len())
            .sum::<usize>();
    let mut replacement = new.to_string();
    if content[start..end].ends_with('\n') && !replacement.ends_with('\n') {
        replacement.push('\n');
    }
    let mut out = String::with_capacity(content.len() - (end - start) + replacement.len());
    out.push_str(&content[..start]);
    out.push_str(&replacement);
    out.push_str(&content[end..]);
    Some(out)
}

/// Hard cap on a single `read_file` result so one read can't flood the model's context. A whole
/// file over this is truncated (head kept — imports/signatures live there) with a marker telling
/// the model to request a specific line range instead.
const READ_MAX_BYTES: usize = 256 * 1024;

fn cap_read(s: String) -> String {
    if s.len() <= READ_MAX_BYTES {
        return s;
    }
    let mut end = READ_MAX_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[… file truncated at {} KiB — pass start_line/end_line to read a specific section …]",
        &s[..end],
        READ_MAX_BYTES / 1024
    )
}

/// Write (create/overwrite) a text file. Mutates the workspace.
pub struct WriteFileTool;

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &str {
        "write_file"
    }
    fn description(&self) -> &str {
        "Write content to a file at the given path, creating it or OVERWRITING it whole. For an \
         existing file, read it first and prefer edit_file for targeted changes — write_file \
         replaces the entire file, so any content you omit is lost. Best for new files or full \
         rewrites."
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

    async fn preview(&self, args: &Value) -> Option<FileDiff> {
        let path = str_arg(args, "path").ok()?;
        let content = str_arg(args, "content").ok()?;
        let old = tokio::fs::read_to_string(path).await.ok();
        let kind = if old.is_some() {
            DiffKind::Modified
        } else {
            DiffKind::Created
        };
        Some(FileDiff {
            path: path.to_string(),
            kind,
            old,
            new: Some(content.to_string()),
            lang: lang_from_path(path),
            binary: false,
        })
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
        "Replace text in a file: swaps the single, EXACT occurrence of `old` with `new`. `old` must \
         match the file byte-for-byte including indentation and whitespace, and must be UNIQUE — \
         include enough surrounding lines of context that it matches exactly once. It is an error \
         if `old` is absent or appears more than once (then add more context and retry). Read the \
         file first so your `old` matches. To insert, set `old` to a unique nearby anchor and put \
         that anchor plus the new lines in `new`. For new files or whole-file rewrites use \
         write_file instead."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit." },
                "old": {
                    "type": "string",
                    "description": "Exact text to replace — must occur exactly once; include \
                     surrounding context to disambiguate."
                },
                "new": { "type": "string", "description": "Replacement text." }
            },
            "required": ["path", "old", "new"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let old = str_arg(args, "old")?;
        let new = str_arg(args, "new")?;

        let content = tokio::fs::read_to_string(path).await?;
        let (updated, note) = apply_edit(&content, old, new)
            .map_err(|e| ToolError::Failed(format!("{e} (in {path})")))?;
        tokio::fs::write(path, &updated).await?;
        Ok(format!("edited {path} (1 replacement){note}"))
    }

    async fn preview(&self, args: &Value) -> Option<FileDiff> {
        let path = str_arg(args, "path").ok()?;
        let old = str_arg(args, "old").ok()?;
        let new = str_arg(args, "new").ok()?;
        let content = tokio::fs::read_to_string(path).await.ok()?;
        // Mirror run() (skip the diff and let run() surface the error when it can't apply).
        let (updated, _) = apply_edit(&content, old, new).ok()?;
        Some(FileDiff {
            path: path.to_string(),
            kind: DiffKind::Modified,
            old: Some(content),
            new: Some(updated),
            lang: lang_from_path(path),
            binary: false,
        })
    }
}

/// Apply one `old → new` replacement to `content`: an exact single match, else a UNIQUE
/// whitespace-insensitive fallback ([`flexible_replace`]). Returns `(updated, note)` or a
/// human-readable error. Shared by [`EditFileTool`] and [`MultiEditTool`].
fn apply_edit(content: &str, old: &str, new: &str) -> Result<(String, &'static str), String> {
    match content.matches(old).count() {
        1 => Ok((content.replacen(old, new, 1), "")),
        0 => flexible_replace(content, old, new)
            .map(|u| (u, " (matched ignoring whitespace)"))
            .ok_or_else(|| {
                "`old` not found (also tried a whitespace-insensitive match; \
                 add surrounding context so it matches exactly once)"
                    .to_string()
            }),
        n => Err(format!(
            "`old` is ambiguous: {n} occurrences — add surrounding context"
        )),
    }
}

/// Apply several `old → new` edits to ONE file in a single call. Mutates the workspace.
pub struct MultiEditTool;

#[async_trait]
impl Tool for MultiEditTool {
    fn name(&self) -> &str {
        "multi_edit"
    }
    fn description(&self) -> &str {
        "Apply several edits to ONE file in a single call, in order. Each edit is {old, new} with \
         exactly edit_file's rules (each `old` exact + unique, with a whitespace-insensitive \
         fallback). ATOMIC: applied in sequence to the in-memory file, and if ANY edit can't be \
         applied the file is left untouched and the failing edit is reported — so partial edits \
         never land. Prefer this over many edit_file calls when changing one file in several places."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File to edit." },
                "edits": {
                    "type": "array",
                    "description": "Edits applied in order; each is {old, new} (same rules as edit_file).",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": { "type": "string" },
                            "new": { "type": "string" }
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let edits = multi_edit_pairs(args)?;
        let original = tokio::fs::read_to_string(path).await?;
        let updated = apply_edits(&original, &edits)
            .map_err(|e| ToolError::Failed(format!("{e} (in {path}; no edits applied)")))?;
        tokio::fs::write(path, &updated).await?;
        Ok(format!("edited {path} ({} edits applied)", edits.len()))
    }

    async fn preview(&self, args: &Value) -> Option<FileDiff> {
        let path = str_arg(args, "path").ok()?;
        let edits = multi_edit_pairs(args).ok()?;
        let original = tokio::fs::read_to_string(path).await.ok()?;
        let updated = apply_edits(&original, &edits).ok()?;
        Some(FileDiff {
            path: path.to_string(),
            kind: DiffKind::Modified,
            old: Some(original),
            new: Some(updated),
            lang: lang_from_path(path),
            binary: false,
        })
    }
}

/// Extract the `(old, new)` pairs from a `multi_edit` call's `edits` array.
fn multi_edit_pairs(args: &Value) -> Result<Vec<(String, String)>, ToolError> {
    let arr = args
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Failed("`edits` must be an array of {old, new}".to_string()))?;
    if arr.is_empty() {
        return Err(ToolError::Failed("`edits` is empty".to_string()));
    }
    arr.iter()
        .map(|e| {
            let old = e.get("old").and_then(Value::as_str);
            let new = e.get("new").and_then(Value::as_str);
            match (old, new) {
                (Some(o), Some(n)) => Ok((o.to_string(), n.to_string())),
                _ => Err(ToolError::Failed(
                    "each edit needs string `old` and `new`".to_string(),
                )),
            }
        })
        .collect()
}

/// Fold the edits over `content` in order (each on the running result), all-or-nothing: the first
/// edit that can't apply aborts with `edit #k: <reason>` and the caller writes nothing.
fn apply_edits(content: &str, edits: &[(String, String)]) -> Result<String, String> {
    let mut cur = content.to_string();
    for (k, (old, new)) in edits.iter().enumerate() {
        let (next, _) = apply_edit(&cur, old, new).map_err(|e| format!("edit #{}: {e}", k + 1))?;
        cur = next;
    }
    Ok(cur)
}

/// Apply a unified diff to the workspace via `git apply`. Mutates the workspace.
pub struct ApplyPatchTool;

#[async_trait]
impl Tool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }
    fn description(&self) -> &str {
        "Apply a unified diff (git / `diff -u` format) to the workspace — best for multi-file or \
         large changes where a patch is cleaner than edit_file. The `--- a/path` / `+++ b/path` \
         headers name the files (a patch can also create or delete files). Applied with `git apply` \
         (line-number drift tolerated); if it doesn't apply cleanly the error is returned verbatim \
         so you can regenerate the patch. For small single-file edits prefer edit_file / multi_edit."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "patch": { "type": "string", "description": "A unified diff to apply." },
                "cwd": { "type": "string", "description": "Directory to apply in (default: current)." }
            },
            "required": ["patch"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        use tokio::io::AsyncWriteExt;
        let patch = str_arg(args, "patch")?;
        let cwd = args.get("cwd").and_then(Value::as_str).unwrap_or(".");
        let mut child = tokio::process::Command::new("git")
            // Apply byte-faithfully: a global core.autocrlf=true (the default on GitHub's
            // Windows runners) would otherwise rewrite the patched file's line endings.
            .args([
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.eol=lf",
                "apply",
                "--recount",
                "--whitespace=nowarn",
                "-",
            ])
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| ToolError::Failed(format!("spawning git apply: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(patch.as_bytes()).await;
            if !patch.ends_with('\n') {
                let _ = stdin.write_all(b"\n").await; // git apply wants a trailing newline
            }
            let _ = stdin.shutdown().await;
        }
        let out = child
            .wait_with_output()
            .await
            .map_err(|e| ToolError::Failed(format!("running git apply: {e}")))?;
        if out.status.success() {
            let files = patch
                .lines()
                .filter(|l| l.starts_with("+++ "))
                .count()
                .max(1);
            Ok(format!("applied patch ({files} file(s) changed)"))
        } else {
            Err(ToolError::Failed(format!(
                "git apply failed (regenerate the patch against the current file): {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }
}

/// Edit a Jupyter notebook (`.ipynb`) at the cell level: replace a cell's source, insert a new
/// cell, or delete one. Mutates the workspace.
pub struct NotebookEditTool;

#[derive(Clone, Copy, PartialEq)]
enum NotebookMode {
    Replace,
    Insert,
    Delete,
}

/// nbformat stores a cell's `source` as a list of lines, each retaining its trailing `\n`.
fn notebook_source_array(source: &str) -> Value {
    Value::Array(
        source
            .split_inclusive('\n')
            .map(|l| Value::String(l.to_string()))
            .collect(),
    )
}

/// Apply one cell-level edit to a notebook's JSON text. Pure (no disk) so it can be unit-tested;
/// returns the rewritten notebook JSON or a human-readable error. Editing a code cell's source
/// clears its stale `outputs`/`execution_count` (they no longer correspond to the new code).
fn apply_notebook_edit(
    content: &str,
    cell: usize,
    source: Option<&str>,
    cell_type: &str,
    mode: NotebookMode,
) -> Result<String, String> {
    let mut nb: Value =
        serde_json::from_str(content).map_err(|e| format!("not valid JSON: {e}"))?;
    let cells = nb
        .get_mut("cells")
        .and_then(Value::as_array_mut)
        .ok_or("not a Jupyter notebook (no `cells` array)")?;
    let n = cells.len();
    match mode {
        NotebookMode::Delete => {
            if cell >= n {
                return Err(format!("cell {cell} out of range (notebook has {n} cells)"));
            }
            cells.remove(cell);
        }
        NotebookMode::Insert => {
            let source = source.ok_or("`source` is required to insert a cell")?;
            if cell > n {
                return Err(format!(
                    "insert index {cell} out of range (notebook has {n} cells)"
                ));
            }
            let new_cell = if cell_type == "markdown" {
                json!({ "cell_type": "markdown", "metadata": {}, "source": notebook_source_array(source) })
            } else {
                json!({ "cell_type": "code", "metadata": {}, "execution_count": null, "outputs": [], "source": notebook_source_array(source) })
            };
            cells.insert(cell, new_cell);
        }
        NotebookMode::Replace => {
            let source = source.ok_or("`source` is required to replace a cell")?;
            if cell >= n {
                return Err(format!("cell {cell} out of range (notebook has {n} cells)"));
            }
            let c = &mut cells[cell];
            c["source"] = notebook_source_array(source);
            if c.get("cell_type").and_then(Value::as_str) == Some("code") {
                if let Some(obj) = c.as_object_mut() {
                    obj.insert("outputs".into(), Value::Array(Vec::new()));
                    obj.insert("execution_count".into(), Value::Null);
                }
            }
        }
    }
    let mut out = serde_json::to_string_pretty(&nb).map_err(|e| e.to_string())?;
    out.push('\n'); // .ipynb files conventionally end with a trailing newline
    Ok(out)
}

#[async_trait]
impl Tool for NotebookEditTool {
    fn name(&self) -> &str {
        "notebook_edit"
    }
    fn description(&self) -> &str {
        "Edit a Jupyter notebook (.ipynb) by cell index (0-based). mode=replace (default) swaps a \
         cell's source; mode=insert adds a new cell at the index; mode=delete removes it. \
         `source` is the full new cell text (required for replace/insert). For insert, `cell_type` \
         is \"code\" (default) or \"markdown\". Replacing a code cell clears its stale outputs and \
         execution count. Use this for .ipynb files — edit_file would corrupt the JSON structure."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Notebook (.ipynb) file." },
                "cell": { "type": "integer", "description": "0-based cell index.", "minimum": 0 },
                "mode": {
                    "type": "string",
                    "enum": ["replace", "insert", "delete"],
                    "description": "replace (default), insert, or delete."
                },
                "source": {
                    "type": "string",
                    "description": "New cell source (required for replace/insert)."
                },
                "cell_type": {
                    "type": "string",
                    "enum": ["code", "markdown"],
                    "description": "Cell type for insert (default code)."
                }
            },
            "required": ["path", "cell"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let path = str_arg(args, "path")?;
        let cell = args
            .get("cell")
            .and_then(Value::as_u64)
            .ok_or_else(|| ToolError::BadArgs("expected integer 'cell'".into()))?
            as usize;
        let mode = match args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("replace")
        {
            "replace" => NotebookMode::Replace,
            "insert" => NotebookMode::Insert,
            "delete" => NotebookMode::Delete,
            other => return Err(ToolError::BadArgs(format!("unknown mode '{other}'"))),
        };
        let source = args.get("source").and_then(Value::as_str);
        let cell_type = args
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code");

        let content = tokio::fs::read_to_string(path).await?;
        let updated = apply_notebook_edit(&content, cell, source, cell_type, mode)
            .map_err(|e| ToolError::Failed(format!("{e} (in {path})")))?;
        tokio::fs::write(path, &updated).await?;
        let verb = match mode {
            NotebookMode::Replace => "replaced",
            NotebookMode::Insert => "inserted",
            NotebookMode::Delete => "deleted",
        };
        Ok(format!("{verb} cell {cell} in {path}"))
    }

    async fn preview(&self, args: &Value) -> Option<FileDiff> {
        let path = str_arg(args, "path").ok()?;
        let cell = args.get("cell").and_then(Value::as_u64)? as usize;
        let mode = match args
            .get("mode")
            .and_then(Value::as_str)
            .unwrap_or("replace")
        {
            "insert" => NotebookMode::Insert,
            "delete" => NotebookMode::Delete,
            _ => NotebookMode::Replace,
        };
        let source = args.get("source").and_then(Value::as_str);
        let cell_type = args
            .get("cell_type")
            .and_then(Value::as_str)
            .unwrap_or("code");
        let content = tokio::fs::read_to_string(path).await.ok()?;
        let updated = apply_notebook_edit(&content, cell, source, cell_type, mode).ok()?;
        Some(FileDiff {
            path: path.to_string(),
            kind: DiffKind::Modified,
            old: Some(content),
            new: Some(updated),
            lang: Some("json".to_string()),
            binary: false,
        })
    }
}

/// Delete a file. Mutates the workspace.
pub struct DeleteFileTool;

#[async_trait]
impl Tool for DeleteFileTool {
    fn name(&self) -> &str {
        "delete_file"
    }
    fn description(&self) -> &str {
        "Delete a file at the given path."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Write
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
        tokio::fs::remove_file(path).await?;
        Ok(format!("deleted {path}"))
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

/// Recursively search text files for a pattern, returning `path:lineno: line` matches.
/// Supports substring (default) or full regex matching, and an optional file-path glob filter.
pub struct SearchTool;

const SEARCH_MATCH_CAP: usize = 200;

/// Directory names skipped by `search` and `glob` (in addition to all dot-dirs): heavy vendor /
/// build / dependency trees that bury real results and aren't part of the source the agent edits.
const SEARCH_SKIP_DIRS: &[&str] = &[
    "node_modules",
    "target",
    "dist",
    "build",
    "vendor",
    "__pycache__",
    "venv",
    ".venv",
];

#[async_trait]
impl Tool for SearchTool {
    fn name(&self) -> &str {
        "search"
    }
    fn description(&self) -> &str {
        "Recursively search text files under `path` for lines matching `query`. \
         Set `regex: true` for regex matching (default: substring). \
         Use `file_pattern` (glob) to restrict which files are searched, e.g. \"**/*.rs\"."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string" },
                "path": { "type": "string" },
                "regex": {
                    "type": "boolean",
                    "description": "Treat `query` as a regex. Default: false (substring match)."
                },
                "file_pattern": {
                    "type": "string",
                    "description": "Glob to filter which files are searched, e.g. \"**/*.rs\"."
                }
            },
            "required": ["query"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let query = str_arg(args, "query")?;
        let root = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let use_regex = args.get("regex").and_then(Value::as_bool).unwrap_or(false);
        let file_pattern = args.get("file_pattern").and_then(Value::as_str);

        let re: Option<regex::Regex> = if use_regex {
            Some(
                regex::Regex::new(query)
                    .map_err(|e| ToolError::Failed(format!("invalid regex: {e}")))?,
            )
        } else {
            None
        };

        let file_glob: Option<GlobMatcher> = if let Some(pat) = file_pattern {
            Some(
                Glob::new(pat)
                    .map_err(|e| ToolError::Failed(format!("invalid file_pattern: {e}")))?
                    .compile_matcher(),
            )
        } else {
            None
        };

        let mut matches: Vec<String> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue; // hidden files + dirs (.git, .venv, …)
                }
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    // Skip heavy vendor/build dirs so non-Rust repos (node_modules, venv, …) don't
                    // bury real results. (`target` is now skipped only as a directory.)
                    if SEARCH_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    if let Some(ref fg) = file_glob {
                        if !fg.is_match(rel) {
                            continue;
                        }
                    }
                    if let Ok(content) = std::fs::read_to_string(&path) {
                        let rel_display = rel.display();
                        for (i, line) in content.lines().enumerate() {
                            let hit = if let Some(ref re) = re {
                                re.is_match(line)
                            } else {
                                line.contains(query)
                            };
                            if hit {
                                matches.push(format!(
                                    "{rel_display}:{}: {}",
                                    i + 1,
                                    line.trim_end()
                                ));
                                if matches.len() >= SEARCH_MATCH_CAP {
                                    matches
                                        .push(format!("… (capped at {SEARCH_MATCH_CAP} matches)"));
                                    return Ok(matches.join("\n"));
                                }
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

/// List files matching a glob pattern, recursively. Skips hidden directories and `target/`.
pub struct GlobTool;

#[async_trait]
impl Tool for GlobTool {
    fn name(&self) -> &str {
        "glob"
    }
    fn description(&self) -> &str {
        "List files matching a glob pattern (e.g. \"**/*.rs\", \"src/**/*.toml\"). \
         Returns sorted relative paths. Skips hidden dirs and `target/`."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::ReadOnly
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Glob pattern, e.g. \"**/*.rs\" or \"src/**/*.toml\"."
                },
                "path": {
                    "type": "string",
                    "description": "Root directory to search from (default: \".\")."
                }
            },
            "required": ["pattern"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let pattern = str_arg(args, "pattern")?;
        let root = args.get("path").and_then(Value::as_str).unwrap_or(".");

        let matcher = Glob::new(pattern)
            .map_err(|e| ToolError::Failed(format!("invalid glob: {e}")))?
            .compile_matcher();

        let mut matches: Vec<String> = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue; // hidden files + dirs (.git, .venv, …)
                }
                let path = entry.path();
                let Ok(ft) = entry.file_type() else { continue };
                if ft.is_dir() {
                    // Skip heavy vendor/build dirs so non-Rust repos (node_modules, venv, …) don't
                    // bury real results. (`target` is now skipped only as a directory.)
                    if SEARCH_SKIP_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).unwrap_or(&path);
                    if matcher.is_match(rel) {
                        matches.push(rel.display().to_string());
                    }
                }
            }
        }

        if matches.is_empty() {
            Ok(format!("no files match '{pattern}'"))
        } else {
            matches.sort();
            Ok(matches.join("\n"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[tokio::test]
    async fn apply_patch_applies_a_unified_diff() {
        let dir = temp_dir("applypatch");
        std::fs::write(dir.join("f.txt"), "a\nb\nc\n").unwrap();
        let patch = "--- a/f.txt\n+++ b/f.txt\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n c\n";
        let out = ApplyPatchTool
            .run(&json!({ "patch": patch, "cwd": dir.to_str().unwrap() }))
            .await;
        assert!(out.is_ok(), "apply failed: {out:?}");
        assert_eq!(
            std::fs::read_to_string(dir.join("f.txt")).unwrap(),
            "a\nB\nc\n"
        );
        // A patch that doesn't match the file is reported as an error (not silently dropped).
        let bad = "--- a/f.txt\n+++ b/f.txt\n@@ -1 +1 @@\n-zzz\n+q\n";
        assert!(ApplyPatchTool
            .run(&json!({ "patch": bad, "cwd": dir.to_str().unwrap() }))
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    const NB: &str = r##"{"cells":[{"cell_type":"code","execution_count":3,"metadata":{},"outputs":[{"x":1}],"source":["print(1)\n"]},{"cell_type":"markdown","metadata":{},"source":["# title\n"]}],"metadata":{},"nbformat":4,"nbformat_minor":5}"##;

    #[test]
    fn notebook_replace_rewrites_source_and_clears_code_outputs() {
        let out = apply_notebook_edit(NB, 0, Some("print(2)\n"), "code", NotebookMode::Replace)
            .expect("replace");
        let nb: Value = serde_json::from_str(&out).unwrap();
        let c0 = &nb["cells"][0];
        assert_eq!(c0["source"], json!(["print(2)\n"]));
        // Stale execution artifacts are cleared on a source edit.
        assert_eq!(c0["outputs"], json!([]));
        assert_eq!(c0["execution_count"], Value::Null);
        // The other cell is untouched.
        assert_eq!(nb["cells"][1]["source"], json!(["# title\n"]));
    }

    #[test]
    fn notebook_insert_and_delete_change_cell_count() {
        let inserted =
            apply_notebook_edit(NB, 1, Some("import os\n"), "code", NotebookMode::Insert).unwrap();
        let nb: Value = serde_json::from_str(&inserted).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 3);
        assert_eq!(nb["cells"][1]["source"], json!(["import os\n"]));

        let deleted = apply_notebook_edit(NB, 0, None, "code", NotebookMode::Delete).unwrap();
        let nb: Value = serde_json::from_str(&deleted).unwrap();
        assert_eq!(nb["cells"].as_array().unwrap().len(), 1);
        assert_eq!(nb["cells"][0]["cell_type"], "markdown");
    }

    #[test]
    fn notebook_edit_rejects_bad_index_and_non_notebook() {
        assert!(apply_notebook_edit(NB, 9, Some("x"), "code", NotebookMode::Replace).is_err());
        assert!(
            apply_notebook_edit("{\"foo\":1}", 0, Some("x"), "code", NotebookMode::Replace)
                .is_err()
        );
        assert!(apply_notebook_edit("not json", 0, None, "code", NotebookMode::Delete).is_err());
    }

    #[test]
    fn apply_edits_is_atomic_and_ordered() {
        let content = "a = 1\nb = 2\nc = 3\n";
        // Both edits apply, in order.
        let out = apply_edits(
            content,
            &[
                ("a = 1".into(), "a = 10".into()),
                ("c = 3".into(), "c = 30".into()),
            ],
        )
        .unwrap();
        assert_eq!(out, "a = 10\nb = 2\nc = 30\n");
        // A failing edit aborts the whole batch (atomic) and names which one.
        let err = apply_edits(
            content,
            &[
                ("a = 1".into(), "a = 10".into()),
                ("nope".into(), "x".into()),
            ],
        )
        .unwrap_err();
        assert!(err.starts_with("edit #2:"), "names the failing edit: {err}");
    }

    #[test]
    fn flexible_replace_matches_ignoring_whitespace_when_unique() {
        let content = "fn a() {\n        let x = 1;\n}\n";
        // `old` has different indentation than the file — exact match would miss.
        let out = flexible_replace(content, "let x = 1;", "let x = 2;").unwrap();
        assert!(out.contains("let x = 2;"), "replaced: {out:?}");
        assert!(out.starts_with("fn a() {\n"), "rest preserved: {out:?}");
        assert!(out.ends_with("}\n"), "trailing preserved: {out:?}");
        // Ambiguous → None (two whitespace-equal matches).
        let dup = "  v\n    v\n";
        assert!(
            flexible_replace(dup, "v", "w").is_none(),
            "ambiguous → None"
        );
        // Genuinely absent → None.
        assert!(flexible_replace(content, "let y = 9;", "z").is_none());
    }

    #[test]
    fn cap_read_truncates_oversized_with_a_marker() {
        let small = "fn main() {}".to_string();
        assert_eq!(cap_read(small.clone()), small, "small content is untouched");
        let big = "x".repeat(READ_MAX_BYTES + 5_000);
        let capped = cap_read(big);
        assert!(
            capped.len() <= READ_MAX_BYTES + 200,
            "capped near the limit"
        );
        assert!(
            capped.contains("truncated"),
            "explains the cut + how to read more"
        );
    }

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
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "dup dup");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_file_removes_file() {
        let dir = temp_dir("delete");
        let path = dir.join("f.txt");
        std::fs::write(&path, "bye").unwrap();
        DeleteFileTool
            .run(&json!({ "path": path.to_str().unwrap() }))
            .await
            .unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn delete_file_errors_on_missing() {
        let err = DeleteFileTool
            .run(&json!({ "path": "/no/such/file/xyz.txt" }))
            .await
            .unwrap_err();
        assert!(matches!(err, ToolError::Io(_)));
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
        std::fs::create_dir(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/n.txt"), "find ME").unwrap();

        let out = SearchTool
            .run(&json!({ "query": "find ME", "path": dir.to_str().unwrap() }))
            .await
            .unwrap();

        assert!(out.contains("a.txt:2: find ME here"), "got:\n{out}");
        assert!(!out.contains("target"), "must skip target/:\n{out}");
        assert!(!out.contains("g.txt"), "must skip .git/:\n{out}");
        assert!(!out.contains("n.txt"), "must skip node_modules/:\n{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_regex_matches_pattern() {
        let dir = temp_dir("search-regex");
        std::fs::write(dir.join("a.txt"), "fn hello() {}\nfn world() {}").unwrap();

        let out = SearchTool
            .run(&json!({
                "query": r"fn \w+\(\)",
                "path": dir.to_str().unwrap(),
                "regex": true
            }))
            .await
            .unwrap();

        assert!(out.contains("a.txt:1:"), "got:\n{out}");
        assert!(out.contains("a.txt:2:"), "got:\n{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_file_pattern_filters_extension() {
        let dir = temp_dir("search-filepattern");
        std::fs::write(dir.join("a.rs"), "needle").unwrap();
        std::fs::write(dir.join("b.txt"), "needle").unwrap();

        let out = SearchTool
            .run(&json!({
                "query": "needle",
                "path": dir.to_str().unwrap(),
                "file_pattern": "**/*.rs"
            }))
            .await
            .unwrap();

        assert!(out.contains("a.rs"), "got:\n{out}");
        assert!(!out.contains("b.txt"), "must skip non-rs:\n{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_finds_files_by_pattern() {
        let dir = temp_dir("glob");
        std::fs::create_dir(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "").unwrap();
        std::fs::write(dir.join("README.md"), "").unwrap();

        let out = GlobTool
            .run(&json!({ "pattern": "**/*.rs", "path": dir.to_str().unwrap() }))
            .await
            .unwrap();

        assert!(out.contains("main.rs"), "got:\n{out}");
        assert!(out.contains("lib.rs"), "got:\n{out}");
        assert!(!out.contains("README.md"), "no md:\n{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_reads_workspace_manifest() {
        let out = ReadFileTool
            .run(&json!({ "path": "Cargo.toml" }))
            .await
            .unwrap();
        assert!(out.contains("forge-tools"));
    }

    #[tokio::test]
    async fn read_file_line_range() {
        let dir = temp_dir("read-range");
        let path = dir.join("f.txt");
        std::fs::write(&path, "line1\nline2\nline3\nline4\nline5").unwrap();

        let out = ReadFileTool
            .run(&json!({ "path": path.to_str().unwrap(), "start_line": 2, "end_line": 4 }))
            .await
            .unwrap();

        assert_eq!(out, "line2\nline3\nline4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_file_requires_path() {
        let err = ReadFileTool.run(&json!({})).await.unwrap_err();
        assert!(matches!(err, ToolError::BadArgs(_)));
    }
}
