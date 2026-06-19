//! The `Tool` trait and Forge's core coding tools. Each tool declares its [`SideEffect`]
//! class, which the core's permission broker (ADR-0008) uses to decide whether to allow,
//! ask, or deny. Adding a tool is implementing this trait and registering it — no core
//! changes.

use std::collections::HashMap;

use async_trait::async_trait;
use forge_types::{FileDiff, SideEffect};
use serde_json::Value;

mod core_tools;
mod lattice_tool;
mod sandbox;
mod shell;
mod web;
pub use core_tools::{
    DeleteFileTool, EditFileTool, GlobTool, ListDirTool, ReadFileTool, SearchTool, WriteFileTool,
};
pub use lattice_tool::LatticeTool;
pub use sandbox::{ApplyResult, SandboxPolicy};
pub use shell::ShellTool;
pub use web::{BraveSearch, DuckDuckGo, SearchBackend, SearchResult, WebFetchTool, WebSearchTool};

/// Run a shell command without a sandbox (for use by the autofix loop and other internal
/// callers that don't need filesystem confinement). Never returns `Err`.
pub async fn run_shell_command(command: &str, cwd: &str, timeout_secs: u64) -> String {
    shell::run_command(command, cwd, timeout_secs, &SandboxPolicy::default()).await
}

#[derive(Debug, thiserror::Error)]
pub enum ToolError {
    #[error("missing or invalid argument: {0}")]
    BadArgs(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("tool execution failed: {0}")]
    Failed(String),
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn side_effect(&self) -> SideEffect;
    /// JSON Schema for the arguments object (advertised to the model).
    fn schema(&self) -> Value;
    async fn run(&self, args: &Value) -> Result<String, ToolError>;

    /// Compute the proposed change *without touching disk*, for diff-review before the write
    /// is confirmed. Returns `None` for tools that don't mutate files, or when a preview
    /// can't be produced (the real error then surfaces from `run`). Default: no preview.
    async fn preview(&self, _args: &Value) -> Option<FileDiff> {
        None
    }
}

/// Holds the available tools, looked up by name during the agent loop.
#[derive(Default)]
pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register all core coding tools.
    pub fn with_core_tools() -> Self {
        let mut r = Self::new();
        r.register(Box::new(ReadFileTool));
        r.register(Box::new(WriteFileTool));
        r.register(Box::new(EditFileTool));
        r.register(Box::new(DeleteFileTool));
        r.register(Box::new(ShellTool::default()));
        r.register(Box::new(ListDirTool));
        r.register(Box::new(SearchTool));
        r.register(Box::new(GlobTool));
        r.register(Box::new(WebFetchTool));
        r.register(Box::new(WebSearchTool::new()));
        r
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|b| b.as_ref())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.keys().map(String::as_str)
    }
}

/// Extract a required string argument from a JSON args object.
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::BadArgs(format!("expected string '{key}'")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_has_core_tools() {
        let r = ToolRegistry::with_core_tools();
        for name in [
            "read_file",
            "write_file",
            "edit_file",
            "delete_file",
            "shell",
            "list_dir",
            "search",
            "glob",
            "web_fetch",
            "web_search",
        ] {
            assert!(r.get(name).is_some(), "missing tool: {name}");
        }
    }

    #[tokio::test]
    async fn write_file_preview_new_path_is_created_kind() {
        let path = std::env::temp_dir().join(format!("forge-prev-{}.txt", forge_types::new_id()));
        let args = serde_json::json!({ "path": path.to_str().unwrap(), "content": "hi there" });
        let diff = WriteFileTool
            .preview(&args)
            .await
            .expect("preview for a write");
        assert_eq!(diff.kind, forge_types::DiffKind::Created);
        assert!(diff.old.is_none(), "no prior content for a new file");
        assert_eq!(diff.new.as_deref(), Some("hi there"));
        // preview must NOT create the file.
        assert!(!path.exists(), "preview is side-effect-free");
    }

    #[tokio::test]
    async fn read_only_tool_has_no_preview() {
        assert!(ReadFileTool
            .preview(&serde_json::json!({"path":"x"}))
            .await
            .is_none());
    }

    #[test]
    fn side_effect_classes_are_correct() {
        let r = ToolRegistry::with_core_tools();
        assert_eq!(
            r.get("read_file").unwrap().side_effect(),
            SideEffect::ReadOnly
        );
        assert_eq!(
            r.get("write_file").unwrap().side_effect(),
            SideEffect::Write
        );
        assert_eq!(r.get("shell").unwrap().side_effect(), SideEffect::Shell);
        assert_eq!(r.get("edit_file").unwrap().side_effect(), SideEffect::Write);
        assert_eq!(
            r.get("list_dir").unwrap().side_effect(),
            SideEffect::ReadOnly
        );
        assert_eq!(r.get("search").unwrap().side_effect(), SideEffect::ReadOnly);
        assert_eq!(r.get("glob").unwrap().side_effect(), SideEffect::ReadOnly);
        assert_eq!(
            r.get("delete_file").unwrap().side_effect(),
            SideEffect::Write
        );
        assert_eq!(
            r.get("web_fetch").unwrap().side_effect(),
            SideEffect::Network
        );
        assert_eq!(
            r.get("web_search").unwrap().side_effect(),
            SideEffect::Network
        );
    }
}
