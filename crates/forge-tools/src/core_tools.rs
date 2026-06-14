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

#[cfg(test)]
mod tests {
    use super::*;

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
