//! The `Tool` trait and Forge's core coding tools. Each tool declares its [`SideEffect`]
//! class, which the core's permission broker (ADR-0008) uses to decide whether to allow,
//! ask, or deny. Adding a tool is implementing this trait and registering it — no core
//! changes.

use std::collections::HashMap;

use async_trait::async_trait;
use forge_types::SideEffect;
use serde_json::Value;

mod core_tools;
pub use core_tools::{ReadFileTool, ShellTool, WriteFileTool};

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
        r.register(Box::new(ShellTool));
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
        assert!(r.get("read_file").is_some());
        assert!(r.get("write_file").is_some());
        assert!(r.get("shell").is_some());
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
    }
}
