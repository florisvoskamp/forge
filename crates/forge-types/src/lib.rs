//! Shared, dependency-free domain types used across every Forge crate.
//!
//! This is a leaf crate (no internal dependencies) so the workspace graph stays acyclic:
//! provider, mesh, tools, store, core and tui all depend on it, it depends on none of them.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Who produced a message in a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// A single message in a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
        }
    }
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }
}

/// A request from a model to invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Arguments as a JSON object.
    pub args: serde_json::Value,
}

/// Token counts and computed cost for one provider call.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_usd: f64,
}

impl Usage {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// The Model Mesh's difficulty classification for a task (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskTier {
    Trivial,
    Standard,
    Complex,
}

impl TaskTier {
    pub fn as_str(self) -> &'static str {
        match self {
            TaskTier::Trivial => "trivial",
            TaskTier::Standard => "standard",
            TaskTier::Complex => "complex",
        }
    }
}

/// Session-level tool-safety posture (ADR-0008).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask before any side effect.
    #[default]
    Default,
    /// Auto-allow file writes/edits; still ask for shell.
    AcceptEdits,
    /// Auto-allow everything (explicit, dangerous opt-in).
    Bypass,
    /// Read-only: deny all side effects.
    Plan,
}

/// How "dangerous" a tool is — drives the permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SideEffect {
    /// No side effects (read/search/list) — never prompts.
    ReadOnly,
    /// Mutates files in the workspace.
    Write,
    /// Executes arbitrary shell commands.
    Shell,
}

/// Outcome of a permission check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionDecision {
    Allow,
    /// Must ask the user to confirm.
    Ask,
    Deny,
}

/// A new opaque identifier (UUID v4) as a string.
pub fn new_id() -> String {
    Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_totals() {
        let u = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cost_usd: 0.01,
        };
        assert_eq!(u.total_tokens(), 15);
    }

    #[test]
    fn permission_mode_default_is_safe() {
        assert_eq!(PermissionMode::default(), PermissionMode::Default);
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
