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

    /// Parse the stored string form back into a `Role`.
    pub fn parse(s: &str) -> Option<Role> {
        match s {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

/// A single message in a session transcript.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: String,
    /// Tool calls the assistant requested in this turn (empty otherwise). Carried so the
    /// transcript can be replayed to a provider as a faithful tool-calling round-trip.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// For a `Tool` message, the id of the call this result answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
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
    /// An assistant turn that requested tool calls.
    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }
    /// A tool result answering a specific call.
    pub fn tool_result(call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
        }
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
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

/// Session-level tool-safety posture (ADR-0008). Exposed in the UI as the **temper** (the
/// forge/metallurgy framing for the agent's disposition); see `docs/features/temper-modes.md`.
/// Serde accepts both the canonical kebab key and the temper-label alias.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionMode {
    /// Ask before any side effect. Temper: **Ask**.
    #[default]
    #[serde(alias = "ask")]
    Default,
    /// Auto-allow file writes/edits; still ask for shell. Temper: **Auto-edit**.
    #[serde(alias = "auto-edit", alias = "autoedit")]
    AcceptEdits,
    /// Auto-allow everything (explicit, dangerous opt-in). Temper: **Full**.
    #[serde(alias = "full")]
    Bypass,
    /// Read-only: deny all side effects. Temper: **Read-only**.
    #[serde(alias = "read-only", alias = "readonly")]
    Plan,
}

impl PermissionMode {
    /// The temper label shown in the UI — names the permission plainly so the active posture
    /// is obvious at a glance (the dimension is themed "temper"; the values are descriptive).
    pub fn label(self) -> &'static str {
        match self {
            PermissionMode::Plan => "Read-only",
            PermissionMode::Default => "Ask",
            PermissionMode::AcceptEdits => "Auto-edit",
            PermissionMode::Bypass => "Full",
        }
    }

    /// The next temper in the SHIFT+TAB cycle. The cycle covers the three everyday tempers and
    /// **wraps** — `Bypass`/Unfettered is intentionally excluded (reachable only via explicit
    /// `--mode unfettered`/config, never by tapping a key). From Unfettered, cycling re-enters
    /// the safe loop at Guarded.
    pub fn cycle_next(self) -> PermissionMode {
        match self {
            PermissionMode::Default => PermissionMode::AcceptEdits, // Guarded → Smith
            PermissionMode::AcceptEdits => PermissionMode::Plan,    // Smith → Survey
            PermissionMode::Plan => PermissionMode::Default,        // Survey → Guarded (wrap)
            PermissionMode::Bypass => PermissionMode::Default,      // leave the unsafe temper
        }
    }
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

/// Where a permission rule came from. Drives precedence: a `Builtin` deny is a safety floor
/// that no configured rule and no permission mode (not even `Bypass`) can override.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    /// Shipped safety default (e.g. `rm -rf /`, secret reads) — unoverridable.
    Builtin,
    /// From a user or project `config.toml`.
    Configured,
}

/// One fine-grained allow/ask/deny rule (FR-10), matching a tool call by name + argument
/// pattern. The decision composes with the global [`PermissionMode`] in the broker.
#[derive(Debug, Clone)]
pub struct PermissionRule {
    /// Tool name to match, or `"*"` for any tool.
    pub tool: String,
    /// Glob patterns over the relevant argument (the effective shell command, or a file
    /// path). Empty means "match any arguments for this tool".
    pub patterns: Vec<String>,
    pub decision: PermissionDecision,
    pub source: RuleSource,
    /// Optional human reason, surfaced when the rule drives the decision.
    pub reason: Option<String>,
}

/// What kind of change a [`FileDiff`] represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffKind {
    Created,
    Modified,
    Deleted,
}

/// A proposed file change, computed *before* a write tool runs so the human can review it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub kind: DiffKind,
    /// Prior on-disk content (`None` for a created file).
    pub old: Option<String>,
    /// Proposed new content (`None` for a deleted file).
    pub new: Option<String>,
    /// Language token inferred from the extension; drives diff-body highlighting.
    pub lang: Option<String>,
    /// True → don't attempt a textual diff (non-UTF-8 target).
    pub binary: bool,
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
    fn temper_labels_name_the_permission_plainly() {
        assert_eq!(PermissionMode::Plan.label(), "Read-only");
        assert_eq!(PermissionMode::Default.label(), "Ask");
        assert_eq!(PermissionMode::AcceptEdits.label(), "Auto-edit");
        assert_eq!(PermissionMode::Bypass.label(), "Full");
    }

    #[test]
    fn temper_cycle_wraps_through_the_safe_three_and_excludes_bypass() {
        let mut m = PermissionMode::Default;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(m);
            m = m.cycle_next();
        }
        assert_eq!(m, PermissionMode::Default, "cycle wraps after three");
        assert_eq!(
            seen,
            vec![
                PermissionMode::Default,
                PermissionMode::AcceptEdits,
                PermissionMode::Plan
            ]
        );
        // The dangerous temper is never produced by cycling, and cycling off it is safe.
        assert!(!seen.contains(&PermissionMode::Bypass));
        assert_eq!(PermissionMode::Bypass.cycle_next(), PermissionMode::Default);
    }

    #[test]
    fn temper_labels_deserialize_as_aliases() {
        let m: PermissionMode = serde_json::from_str("\"read-only\"").unwrap();
        assert_eq!(m, PermissionMode::Plan);
        let m: PermissionMode = serde_json::from_str("\"auto-edit\"").unwrap();
        assert_eq!(m, PermissionMode::AcceptEdits);
        let m: PermissionMode = serde_json::from_str("\"full\"").unwrap();
        assert_eq!(m, PermissionMode::Bypass);
        // Canonical keys still work.
        let m: PermissionMode = serde_json::from_str("\"accept-edits\"").unwrap();
        assert_eq!(m, PermissionMode::AcceptEdits);
    }

    #[test]
    fn ids_are_unique() {
        assert_ne!(new_id(), new_id());
    }
}
