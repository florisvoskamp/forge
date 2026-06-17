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

// ---- Assay: AI-slop / quality analysis (docs/features/analysis-mode.md) ----

/// How serious a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Critical,
    High,
    Medium,
    Low,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Critical => "critical",
            Severity::High => "high",
            Severity::Medium => "medium",
            Severity::Low => "low",
        }
    }
    pub fn parse(s: &str) -> Option<Severity> {
        match s.trim().to_lowercase().as_str() {
            "critical" | "crit" => Some(Severity::Critical),
            "high" => Some(Severity::High),
            "medium" | "med" => Some(Severity::Medium),
            "low" => Some(Severity::Low),
            _ => None,
        }
    }
    /// Higher = more severe (for ranking, since the enum's declaration order would sort the
    /// other way).
    pub fn weight(self) -> u8 {
        match self {
            Severity::Critical => 3,
            Severity::High => 2,
            Severity::Medium => 1,
            Severity::Low => 0,
        }
    }
}

/// Post-verification confidence that a finding is real.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

impl Confidence {
    pub fn as_str(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
    pub fn parse(s: &str) -> Option<Confidence> {
        match s.trim().to_lowercase().as_str() {
            "high" => Some(Confidence::High),
            "medium" | "med" => Some(Confidence::Medium),
            "low" => Some(Confidence::Low),
            _ => None,
        }
    }
    pub fn weight(self) -> u8 {
        match self {
            Confidence::High => 2,
            Confidence::Medium => 1,
            Confidence::Low => 0,
        }
    }
}

/// The lens a critic applies. Mechanical lenses route to the cheap/local tier; judgment lenses
/// to the frontier tier (FR-4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FindingCategory {
    DeadWeight,
    Correctness,
    Unsafe,
    TestCoverage,
    Design,
    Architecture,
    DocumentationRot,
    OverEngineering,
}

impl FindingCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            FindingCategory::DeadWeight => "dead-weight",
            FindingCategory::Correctness => "correctness",
            FindingCategory::Unsafe => "unsafe",
            FindingCategory::TestCoverage => "test-coverage",
            FindingCategory::Design => "design",
            FindingCategory::Architecture => "architecture",
            FindingCategory::DocumentationRot => "documentation",
            FindingCategory::OverEngineering => "over-engineering",
        }
    }
    pub fn parse(s: &str) -> Option<FindingCategory> {
        match s.trim().to_lowercase().as_str() {
            "dead-weight" | "dead" | "deadweight" => Some(FindingCategory::DeadWeight),
            "correctness" | "bug" | "bugs" => Some(FindingCategory::Correctness),
            "unsafe" => Some(FindingCategory::Unsafe),
            "test-coverage" | "tests" | "test" => Some(FindingCategory::TestCoverage),
            "design" => Some(FindingCategory::Design),
            "architecture" | "arch" => Some(FindingCategory::Architecture),
            "documentation" | "docs" | "doc" => Some(FindingCategory::DocumentationRot),
            "over-engineering" | "over-eng" | "overeng" | "ai-slop" | "slop" => {
                Some(FindingCategory::OverEngineering)
            }
            _ => None,
        }
    }
    /// The intended Model-Mesh tier for this lens: mechanical scans are cheap, judgment is
    /// frontier (`docs/features/analysis-mode.md` §U4).
    pub fn tier(self) -> TaskTier {
        match self {
            FindingCategory::DeadWeight
            | FindingCategory::Unsafe
            | FindingCategory::TestCoverage => TaskTier::Trivial,
            _ => TaskTier::Complex,
        }
    }
    /// The v0.1 critic crew, in display order.
    pub fn crew() -> &'static [FindingCategory] {
        &[
            FindingCategory::DeadWeight,
            FindingCategory::Correctness,
            FindingCategory::Unsafe,
            FindingCategory::TestCoverage,
            FindingCategory::Design,
            FindingCategory::Architecture,
            FindingCategory::DocumentationRot,
            FindingCategory::OverEngineering,
        ]
    }
}

/// Rough fix effort for a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Trivial,
    #[default]
    Small,
    Medium,
    Large,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Trivial => "trivial",
            Effort::Small => "small",
            Effort::Medium => "medium",
            Effort::Large => "large",
        }
    }
    pub fn parse(s: &str) -> Option<Effort> {
        match s.trim().to_lowercase().as_str() {
            "trivial" => Some(Effort::Trivial),
            "small" => Some(Effort::Small),
            "medium" | "med" => Some(Effort::Medium),
            "large" => Some(Effort::Large),
            _ => None,
        }
    }
}

/// What part of the repo an assay run covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssayScope {
    Repo,
    Path(String),
    /// Uncommitted working-tree changes (git diff).
    Diff,
    /// Files changed between this branch and `base` (git diff <base>...).
    Branch(String),
    /// Files changed since a git ref (git diff <ref> --name-only).
    Since(String),
}

impl AssayScope {
    pub fn label(&self) -> String {
        match self {
            AssayScope::Repo => "repo".to_string(),
            AssayScope::Path(p) => format!("path {p}"),
            AssayScope::Diff => "diff (working tree)".to_string(),
            AssayScope::Branch(b) => format!("branch vs {b}"),
            AssayScope::Since(r) => format!("since {r}"),
        }
    }
}

/// One verified problem the crew surfaced.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub id: String,
    pub category: FindingCategory,
    pub severity: Severity,
    pub confidence: Confidence,
    pub file: String,
    pub line: Option<u32>,
    /// One-line "what's wrong".
    pub title: String,
    /// WHY it's a problem (the critic's reasoning).
    pub rationale: String,
    pub suggested_fix: String,
    pub effort: Effort,
    /// Which lens raised it.
    pub lens: String,
    /// Survived adversarial verification.
    pub verified: bool,
}

/// The full result of an assay run, findings pre-sorted by (severity, confidence).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssayReport {
    pub run_id: String,
    pub scope: AssayScope,
    pub findings: Vec<Finding>,
    pub cost_usd: f64,
    /// Lenses that errored out, with the reason — graceful degradation.
    pub skipped_lenses: Vec<(String, String)>,
}

impl AssayReport {
    /// Sort findings by severity (most severe first), then confidence, then category for a
    /// stable order. Mutates in place.
    pub fn rank(&mut self) {
        self.findings.sort_by(|a, b| {
            b.severity
                .weight()
                .cmp(&a.severity.weight())
                .then(b.confidence.weight().cmp(&a.confidence.weight()))
                .then(a.category.as_str().cmp(b.category.as_str()))
        });
    }

    /// Count of findings per severity, for the summary header.
    pub fn severity_counts(&self) -> [usize; 4] {
        let mut c = [0usize; 4];
        for f in &self.findings {
            c[match f.severity {
                Severity::Critical => 0,
                Severity::High => 1,
                Severity::Medium => 2,
                Severity::Low => 3,
            }] += 1;
        }
        c
    }
}

/// Live status of one critic lens during an assay run, for per-row TUI progress tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AssayCriticStatus {
    Queued,
    Done { candidates: usize },
    Skipped { reason: String },
}

/// One row in the live assay critics panel: the lens name + its current status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssayCriticRow {
    pub lens: String,
    pub status: AssayCriticStatus,
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

    /// One-line description of what this temper does, for the mode picker.
    pub fn description(self) -> &'static str {
        match self {
            PermissionMode::Plan => "analyze & plan only — no file edits or commands",
            PermissionMode::Default => "ask before every file edit and command",
            PermissionMode::AcceptEdits => "auto-apply file edits; still ask before shell commands",
            PermissionMode::Bypass => "auto-approve everything — dangerous, explicit opt-in",
        }
    }

    /// All tempers, safest → most permissive, for the mode picker (unlike the SHIFT+TAB cycle,
    /// the picker can reach `Full`/Bypass since it's an explicit, deliberate choice).
    pub fn all() -> &'static [PermissionMode] {
        &[
            PermissionMode::Plan,
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
        ]
    }

    /// Parse a temper from its UI label (or canonical/kebab key) — used to resolve a picker row.
    pub fn from_label(s: &str) -> Option<PermissionMode> {
        match s.trim().to_lowercase().as_str() {
            "read-only" | "readonly" | "plan" => Some(PermissionMode::Plan),
            "ask" | "default" => Some(PermissionMode::Default),
            "auto-edit" | "autoedit" | "acceptedits" => Some(PermissionMode::AcceptEdits),
            "full" | "bypass" => Some(PermissionMode::Bypass),
            _ => None,
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
    /// Reaches the network (web fetch/search) — distinct from a local read: egress can
    /// leak context or hit internal hosts, so it is gated separately from `ReadOnly`.
    Network,
    /// A call into an external MCP server (untrusted third-party tool). Gated like a side
    /// effect even when "read-shaped": the server is untrusted code and its result enters the
    /// agent loop, so MCP tool calls always go through the permission broker (mcp-client.md §6).
    External,
}

/// One line of `forge mcp` / `/mcp` server-status output. A dependency-free DTO so both
/// `forge-mcp` (which produces it) and `forge-tui` (which renders it) can share it without a
/// crate dependency between them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerLine {
    pub name: String,
    /// Human status word: connected / reconnecting / unauthorized / slow / failed / disabled.
    pub status: String,
    /// Transport label: "stdio" or "http".
    pub transport: String,
    pub tools: usize,
    pub resources: usize,
    pub prompts: usize,
    /// Extra detail for non-healthy states (the failure reason, retry attempt, latency, …).
    pub detail: Option<String>,
}

/// A single tracked task in the agent's todo list (the `update_tasks` tool). Mirrors the
/// TodoWrite pattern: the model maintains an ordered list and updates each item's status as it
/// works, giving the user a live view of multi-step progress.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub title: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    #[default]
    Pending,
    InProgress,
    Done,
}

impl TodoStatus {
    /// Parse a status from the model's free-form string (tolerant of synonyms/casing/spacing).
    pub fn parse_loose(s: &str) -> Self {
        match s
            .trim()
            .to_ascii_lowercase()
            .replace([' ', '-'], "_")
            .as_str()
        {
            "in_progress" | "active" | "doing" | "started" | "wip" => Self::InProgress,
            "done" | "completed" | "complete" | "finished" => Self::Done,
            _ => Self::Pending,
        }
    }

    /// A checkbox glyph for the TUI list.
    pub fn marker(&self) -> &'static str {
        match self {
            Self::Pending => "☐",
            Self::InProgress => "◐",
            Self::Done => "☑",
        }
    }
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

/// A snapshot of the models that are currently benched (rate-limited / unavailable / failed a
/// probe) and must not be routed to. Built by the store from the `model_health` table — only
/// models whose cooldown has not yet elapsed are included — and consulted by the mesh router.
/// Carries no clock or I/O: the time filtering happens where the snapshot is built.
#[derive(Debug, Default, Clone)]
pub struct ModelHealth {
    benched: std::collections::HashSet<String>,
}

impl ModelHealth {
    pub fn new(benched: std::collections::HashSet<String>) -> Self {
        Self { benched }
    }

    /// Whether `model` is currently benched and should be skipped by routing.
    pub fn is_benched(&self, model: &str) -> bool {
        self.benched.contains(model)
    }

    /// True when no model is benched (the common case — lets the router skip filtering).
    pub fn is_empty(&self) -> bool {
        self.benched.is_empty()
    }
}

/// How a subscription is sitting relative to its rolling usage window (quota-aware routing, L3,
/// provider-cost-routing.md). Ordered so the stricter wins with `.max()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum QuotaStatus {
    /// Comfortably within the window.
    #[default]
    Ok,
    /// Near the window limit (or using overage) — demote the subscription below alternatives.
    Warning,
    /// At/over the limit — skip the subscription entirely (route around it), like a benched model.
    Exhausted,
}

/// One observation of a CLI-bridge subscription's quota, surfaced by the bridge's event stream
/// (e.g. Claude Code's `rate_limit_event`) alongside a completion. Recorded by the store so the
/// router can avoid overrunning a near-limit plan.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaHint {
    /// Bridge provider prefix the quota belongs to (`claude-cli` / `codex-cli`).
    pub provider: String,
    /// The rolling-window kind the provider reported (`five_hour`, `weekly`, …); `""` if unknown.
    pub window: String,
    pub status: QuotaStatus,
    /// Epoch seconds when the window resets, if the provider told us.
    pub resets_at: Option<i64>,
    /// Fraction of the window consumed (0.0–1.0), if the provider told us.
    pub fraction_used: Option<f64>,
}

/// A snapshot of every subscription's current quota pressure, built by the store from the
/// `subscription_usage` table (rows whose window hasn't reset). Consulted by the mesh router to
/// demote or skip a pressured subscription. Carries no clock/I/O (filtering happens at build).
#[derive(Debug, Default, Clone)]
pub struct SubscriptionQuota {
    by_provider: std::collections::HashMap<String, QuotaStatus>,
}

impl SubscriptionQuota {
    pub fn new(by_provider: std::collections::HashMap<String, QuotaStatus>) -> Self {
        Self { by_provider }
    }

    /// The pressure for a provider prefix (defaults to `Ok` when unknown/unconstrained).
    pub fn status_for(&self, provider: &str) -> QuotaStatus {
        self.by_provider
            .get(provider)
            .copied()
            .unwrap_or(QuotaStatus::Ok)
    }

    /// At/over the limit — route around it.
    pub fn is_exhausted(&self, provider: &str) -> bool {
        self.status_for(provider) == QuotaStatus::Exhausted
    }

    /// Near or over the limit — usable but demoted below alternatives.
    pub fn is_pressured(&self, provider: &str) -> bool {
        self.status_for(provider) >= QuotaStatus::Warning
    }

    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_status_parses_loosely_and_defaults_to_pending() {
        assert_eq!(
            TodoStatus::parse_loose("in progress"),
            TodoStatus::InProgress
        );
        assert_eq!(
            TodoStatus::parse_loose("In-Progress"),
            TodoStatus::InProgress
        );
        assert_eq!(TodoStatus::parse_loose("DONE"), TodoStatus::Done);
        assert_eq!(TodoStatus::parse_loose("completed"), TodoStatus::Done);
        assert_eq!(TodoStatus::parse_loose("todo"), TodoStatus::Pending);
        assert_eq!(TodoStatus::parse_loose("garbage"), TodoStatus::Pending);
        assert_eq!(TodoStatus::default(), TodoStatus::Pending);
        assert_eq!(TodoStatus::Done.marker(), "☑");
    }

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

    fn finding(sev: Severity, conf: Confidence, cat: FindingCategory) -> Finding {
        Finding {
            id: new_id(),
            category: cat,
            severity: sev,
            confidence: conf,
            file: "x.rs".into(),
            line: None,
            title: "t".into(),
            rationale: "r".into(),
            suggested_fix: "f".into(),
            effort: Effort::Small,
            lens: cat.as_str().into(),
            verified: true,
        }
    }

    #[test]
    fn report_ranks_by_severity_then_confidence() {
        let mut report = AssayReport {
            run_id: "r".into(),
            scope: AssayScope::Repo,
            findings: vec![
                finding(Severity::Low, Confidence::High, FindingCategory::Design),
                finding(
                    Severity::Critical,
                    Confidence::Low,
                    FindingCategory::Correctness,
                ),
                finding(Severity::High, Confidence::Low, FindingCategory::Unsafe),
                finding(
                    Severity::High,
                    Confidence::High,
                    FindingCategory::DeadWeight,
                ),
            ],
            cost_usd: 0.0,
            skipped_lenses: vec![],
        };
        report.rank();
        let order: Vec<_> = report.findings.iter().map(|f| f.severity).collect();
        assert_eq!(
            order,
            vec![
                Severity::Critical,
                Severity::High,
                Severity::High,
                Severity::Low
            ]
        );
        // Within the two High findings, higher confidence ranks first.
        assert_eq!(report.findings[1].confidence, Confidence::High);
        assert_eq!(report.severity_counts(), [1, 2, 0, 1]);
    }

    #[test]
    fn mechanical_lenses_route_cheap_judgment_routes_frontier() {
        assert_eq!(FindingCategory::DeadWeight.tier(), TaskTier::Trivial);
        assert_eq!(FindingCategory::Unsafe.tier(), TaskTier::Trivial);
        assert_eq!(FindingCategory::Architecture.tier(), TaskTier::Complex);
        assert_eq!(FindingCategory::Correctness.tier(), TaskTier::Complex);
    }

    #[test]
    fn severity_and_category_parse_round_trip() {
        for s in [
            Severity::Critical,
            Severity::High,
            Severity::Medium,
            Severity::Low,
        ] {
            assert_eq!(Severity::parse(s.as_str()), Some(s));
        }
        for c in FindingCategory::crew() {
            assert_eq!(FindingCategory::parse(c.as_str()), Some(*c));
        }
    }
}
