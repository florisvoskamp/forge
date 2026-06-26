use clap::{Parser, Subcommand, ValueEnum};

use crate::bench;

#[derive(Parser)]
#[command(
    name = "forge",
    version,
    about = "Fast, model-agnostic AI coding harness."
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum AssayFormat {
    Human,
    Markdown,
    Json,
    Sarif,
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum FailOnSeverity {
    Low,
    Medium,
    High,
    Critical,
}

impl FailOnSeverity {
    pub(crate) fn matches(self, sev: forge_types::Severity) -> bool {
        let sev_ord = match sev {
            forge_types::Severity::Low => FailOnSeverity::Low,
            forge_types::Severity::Medium => FailOnSeverity::Medium,
            forge_types::Severity::High => FailOnSeverity::High,
            forge_types::Severity::Critical => FailOnSeverity::Critical,
        };
        sev_ord >= self
    }
}

#[derive(Subcommand)]
pub(crate) enum BenchCmd {
    /// Generate SWE-bench predictions: run Forge on each instance and write predictions.jsonl
    /// (score it with the official `swebench` evaluator — see docs/benchmarks/swe-bench.md).
    Swe {
        /// SWE-bench dataset file (JSONL or a JSON array of instances).
        #[arg(long)]
        dataset: std::path::PathBuf,
        /// Where to write predictions.jsonl.
        #[arg(long, default_value = "predictions.jsonl")]
        out: std::path::PathBuf,
        /// Only run the first N instances (smoke runs).
        #[arg(long)]
        limit: Option<usize>,
        /// Pin a specific model. For --agent forge a Forge model id (provider::model); for
        /// claude-code / codex the CLI's own model name (e.g. `opus`, `gpt-5-codex`).
        #[arg(long)]
        model: Option<String>,
        /// Directory to clone/reuse instance repos under.
        #[arg(long, default_value = ".forge/swe-bench")]
        workdir: std::path::PathBuf,
        /// Which agent solves each instance — compare Forge against another CLI's own harness.
        #[arg(long, value_enum, default_value_t = bench::Agent::Forge)]
        agent: bench::Agent,
        /// Per-instance wall-clock budget for an external agent (seconds).
        #[arg(long, default_value_t = 1200)]
        timeout_secs: u64,
        /// Run each instance this many times (separate seeds) for pass@k / best-of-k. >1 writes
        /// one predictions file per seed (`<out>.seed1.jsonl`, …); aggregate with `bench passk`.
        #[arg(long, default_value_t = 1)]
        attempts: usize,
    },
    /// Aggregate pass@k from several swebench evaluation reports (one per seed).
    Passk {
        /// The `*.json` reports written by `run_evaluation`, one per seed.
        #[arg(required = true)]
        reports: Vec<std::path::PathBuf>,
    },
    /// Join per-instance metrics with the official eval report(s) and print resolve rate AND
    /// tokens-per-success per agent — the headline efficiency comparison (e.g. Forge-bridging-X
    /// vs X's own CLI).
    Report {
        /// One or more `<out>.metrics.jsonl` sidecars from `bench swe` (typically one per agent).
        #[arg(long = "metrics", required = true)]
        metrics: Vec<std::path::PathBuf>,
        /// Official `run_evaluation` `*.json` report(s); omit to print token/patch stats only.
        #[arg(long = "eval")]
        evals: Vec<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
pub(crate) enum LocalCmd {
    /// Detect this machine's specs and print the recommended local models.
    Detect,
    /// Install a local model (pulls via Ollama, offering to install Ollama first if missing).
    /// With no KEY, installs the recommended model for this machine.
    Install {
        /// Catalog key (e.g. `gemma4-12b`); omit to use the recommended pick.
        key: Option<String>,
    },
    /// List local models already pulled.
    List,
    /// Start the local runtime (and ensure a model is available). With no KEY, uses the configured
    /// `[local] model` or the recommended pick.
    Start { key: Option<String> },
    /// Show local-runtime status: Ollama installed/serving, installed models, autostart config.
    Status,
}

#[derive(Subcommand)]
pub(crate) enum AssayCmd {
    /// List past assay runs (newest first).
    List,
    /// Compare two assay runs by id prefix: shows new, fixed, and still-open findings.
    Compare {
        /// First run id (or prefix).
        a: String,
        /// Second run id (or prefix).
        b: String,
    },
    /// Run the Assay critic crew headlessly (CI-friendly). Exits 0 on success, 2 when
    /// findings meet the --fail-on threshold, 1 on hard error.
    Run {
        /// What to analyse: `diff` (uncommitted changes), `repo` (whole repo),
        /// `branch`, `since`, or `path` (see --branch / --since / --path).
        #[arg(long, default_value = "diff")]
        scope: String,
        /// For `--scope branch`: the base ref to diff against (e.g. `main`).
        #[arg(long)]
        branch: Option<String>,
        /// For `--scope since`: a git ref (commit / tag) to diff from.
        #[arg(long)]
        since: Option<String>,
        /// For `--scope path`: the file or directory to analyse.
        #[arg(long)]
        path: Option<String>,
        /// Output format.
        #[arg(long, default_value = "human")]
        format: AssayFormat,
        /// Exit 2 if any finding's severity is >= this threshold.
        #[arg(long)]
        fail_on: Option<FailOnSeverity>,
        /// Comma-separated subset of lenses to run (default: full crew).
        /// Valid names: dead-weight, correctness, unsafe, test-coverage,
        ///              design, architecture, documentation, over-engineering
        #[arg(long)]
        lenses: Option<String>,
        /// Override the model for all critics (bypasses mesh tier selection).
        #[arg(long)]
        model: Option<String>,
        /// Abort (exit 3) when the pre-run cost estimate exceeds this USD value.
        /// Without this flag the estimate is printed but never blocks the run.
        #[arg(long, value_name = "USD")]
        max_cost: Option<f64>,
        /// Skip the --max-cost guard and run even if the estimate exceeds the cap.
        #[arg(long, conflicts_with = "max_cost")]
        yes: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum Command {
    /// Run a single agent turn against your prompt.
    Run {
        /// The prompt / task for the agent.
        prompt: Vec<String>,
        /// Use the offline deterministic mock provider (no API keys, no network).
        #[arg(long)]
        mock: bool,
        /// Override the permission mode for this run.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        /// Render the interactive ratatui TUI instead of plain line output.
        #[arg(long)]
        tui: bool,
        /// Resume an existing session by id instead of starting a new one.
        #[arg(long)]
        resume: Option<String>,
        /// Pin a specific model (e.g. `openai::gpt-4o`), bypassing mesh classification.
        #[arg(long)]
        model: Option<String>,
    },
    /// Start an interactive multi-turn chat session.
    Chat {
        /// Use the offline deterministic mock provider.
        #[arg(long)]
        mock: bool,
        /// Override the permission mode.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
        /// Reattach the most-recent session (cannot be combined with --resume).
        #[arg(long, conflicts_with = "resume")]
        r#continue: bool,
        /// Reattach a session by id prefix, or open an interactive picker when no id is given.
        /// Cannot be combined with --continue.
        #[arg(long, num_args = 0..=1, value_name = "ID")]
        resume: Option<Option<String>>,
        /// Force plain line output instead of the interactive TUI.
        #[arg(long)]
        plain: bool,
        /// Run the TUI inline in the terminal's native scrollback instead of full-screen.
        /// Overrides the `[tui] fullscreen` config for this invocation.
        #[arg(long, conflicts_with = "fullscreen")]
        inline: bool,
        /// Force the full-screen (alternate-screen) TUI, overriding the config. This is the
        /// default; use `--inline` to opt out.
        #[arg(long)]
        fullscreen: bool,
        /// Pin a specific model (e.g. `openai::gpt-4o`), bypassing mesh classification.
        #[arg(long)]
        model: Option<String>,
    },
    /// List past sessions (newest first).
    Sessions,
    /// Replay a past session from the record: one id prints its turn-by-turn transcript
    /// (model, tokens, cost, time per turn); two ids diff their summaries.
    Replay {
        /// One session id to reconstruct, or two to diff (git-style prefixes accepted).
        ids: Vec<String>,
        /// Emit the transcript as machine-readable JSON instead of the human-readable format.
        /// Only valid with a single session id.
        #[arg(long)]
        json: bool,
        /// Re-execute the session's user prompts on the CURRENT model/mesh in a fresh session,
        /// then diff the new run against the original (reproducibility audit). Tools run under
        /// your normal permission mode, exactly as `forge run` would. Single id only.
        #[arg(long)]
        rerun: bool,
    },
    /// Inspect past assay runs stored in the database.
    Assay {
        #[command(subcommand)]
        sub: AssayCmd,
    },
    /// Run Forge against an evaluation benchmark (e.g. SWE-bench) and emit predictions.
    Bench {
        #[command(subcommand)]
        sub: BenchCmd,
    },
    /// List discovered slash commands + skills (project and user scope) with their descriptions.
    Commands,
    /// Show the auto-discovered model catalog and the mesh's best pick per tier.
    Models {
        /// Re-ping the currently benched/excluded models and persist the result (clear the ones
        /// that recovered, re-bench the still-dead). Cheap: only touches benched models, not the
        /// whole catalog. Use `--probe --all` to ping every discovered model (costs real money on
        /// paid providers).
        #[arg(long)]
        probe: bool,
        /// With `--probe`, ping EVERY discovered model, not just the benched ones. This calls each
        /// paid model once and can cost a few dollars across a large catalog.
        #[arg(long)]
        all: bool,
        /// Clear all stale model benches (forget every rate-limited/unavailable mark) and exit.
        #[arg(long)]
        clear: bool,
    },
    /// Explain how the mesh routes — classification, scored candidates, quota pressure, the
    /// conservation roll, and the final pick. With a PROMPT, explains that prompt; without one,
    /// shows the per-tier picks + subscription quota overview. `--json` for machine output.
    Mesh {
        /// The task prompt to explain (quote it). Omit for the per-tier / quota overview.
        #[arg(trailing_var_arg = true)]
        prompt: Vec<String>,
        /// Emit the explanation as JSON instead of the formatted view.
        #[arg(long)]
        json: bool,
    },
    /// Show measured model benchmark scores (Artificial Analysis, ADR-0011) and how well they
    /// cover the discovered catalog. `--refresh` forces a re-fetch (needs ARTIFICIALANALYSIS_API_KEY
    /// or `forge auth artificialanalysis`).
    Benchmarks {
        #[arg(long)]
        refresh: bool,
    },
    /// Manage local LLMs (Ollama): detect specs, install/run a Gemma model that fits, list/start.
    Local {
        #[command(subcommand)]
        sub: Option<LocalCmd>,
    },
    /// Diagnose your setup — config, providers/keys, CLI bridges, Ollama, git, terminal — with
    /// actionable fixes. Paste its output into bug reports. Exits non-zero if anything is broken.
    Doctor,
    /// Update Forge to the latest release. A standalone binary install (curl/zip) is replaced in
    /// place; Homebrew/cargo installs print the right upgrade command. `--check` only reports.
    Update {
        /// Only check whether a newer release exists; download/replace nothing.
        #[arg(long)]
        check: bool,
    },
    /// Internal: run Forge's tool registry as an MCP server on stdio (spawned by the CLI
    /// bridge so claude/codex use Forge's tools under Forge's permission gate). Not for direct use.
    #[command(hide = true)]
    McpServe,
    /// Store a provider API key securely in the OS keyring (reads the key from stdin), or remove
    /// it with `--remove`.
    Auth {
        /// Provider: anthropic, openai, gemini, xai, deepseek, or openrouter.
        provider: String,
        /// Delete the stored key for this provider instead of setting one.
        #[arg(long)]
        remove: bool,
    },
    /// Guided first-run setup: enable providers (enter API keys), declare which subscription plan
    /// backs each installed CLI bridge, and optionally install a local LLM that fits this machine.
    Setup,
    /// Alias for `setup` (kept for muscle memory) — runs the same guided setup.
    Init,
    /// Connect to the configured MCP servers and show their status (or one server's tools, or
    /// import a Claude-Code `.mcp.json`).
    Mcp {
        #[command(subcommand)]
        cmd: Option<McpCmd>,
    },
    /// Lattice — native code-intelligence graph (tree-sitter + SQLite). Build it, then query.
    Lattice {
        #[command(subcommand)]
        op: LatticeOp,
    },
    /// Import commands + skills from another AI CLI into Forge's scopes.
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
    /// Git integration helpers (co-author hook installation, etc.).
    Git {
        #[command(subcommand)]
        cmd: GitCmd,
    },
    /// Natural-language shell: describe what you want to know in plain English and Forge
    /// runs the right shell commands, then explains the results.
    ///
    /// Examples:
    ///   forge nl "what changed performance-wise since last week"
    ///   forge nl "which tests are slowest"
    ///   forge nl "show disk usage by directory"
    Nl {
        /// Your question in plain English.
        query: Vec<String>,
        /// Override the permission mode for this run.
        #[arg(long, value_enum)]
        mode: Option<Mode>,
    },
    /// Manage Forge skills.
    Skill {
        #[command(subcommand)]
        sub: SkillCmd,
    },
    /// Copy a full Forge install (config, skills, MCP, model metadata; optionally keys + history)
    /// to another machine. The bundle is a directory — move it with scp -r / rsync / USB.
    Migrate {
        #[command(subcommand)]
        cmd: MigrateCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum MigrateCmd {
    /// Write the install into a bundle directory DEST (config + skills + MCP + model metadata).
    ///
    /// Examples:
    ///   forge migrate export ./forge-bundle
    ///   forge migrate export ./forge-bundle --include-keys --include-sessions
    Export {
        /// Destination directory for the bundle (created if missing).
        dest: std::path::PathBuf,
        /// Also bundle API keys — WRITTEN IN PLAINTEXT. Move + delete the bundle carefully.
        #[arg(long)]
        include_keys: bool,
        /// Also bundle full session history + usage (the whole db). Off by default (private/large).
        #[arg(long)]
        include_sessions: bool,
    },
    /// Restore an install from a bundle directory SRC produced by `export`.
    ///
    /// Example:  forge migrate import ./forge-bundle
    Import {
        /// Bundle directory to restore from.
        src: std::path::PathBuf,
        /// Replace an existing session db instead of preserving it (default keeps your history).
        #[arg(long)]
        force: bool,
    },
    /// Export then copy to TARGET over SSH and run the remote import (forge must be on the remote).
    ///
    /// Example:  forge migrate push me@server --include-keys
    Push {
        /// SSH target, e.g. `user@host`.
        target: String,
        /// Include API keys (PLAINTEXT in transit/temp). See `export --include-keys`.
        #[arg(long)]
        include_keys: bool,
        /// Include full session history + usage.
        #[arg(long)]
        include_sessions: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum SkillCmd {
    /// Distil a past session's transcript into a reusable Forge skill.
    ///
    /// Reads the persisted transcript of SESSION_ID, calls a cheap model to synthesise a
    /// SKILL.md (a generalised, reusable methodology), and writes it where Forge discovers skills.
    /// The new skill is immediately invokable in future sessions.
    ///
    /// Examples:
    ///   forge skill from-session abc123
    ///   forge skill from-session abc123 --name refactor-workflow --scope project
    FromSession {
        /// Session id (or unambiguous prefix) — see `forge sessions`.
        session_id: String,
        /// Override the skill slug (kebab-case, e.g. `refactor-workflow`).
        /// Derived from the session's first user prompt when absent.
        #[arg(long)]
        name: Option<String>,
        /// Where to write the skill: `user` (default) or `project` (`.forge/skills/`).
        #[arg(long, default_value = "user")]
        scope: SkillScope,
    },
}

#[derive(Clone, Copy, ValueEnum, PartialEq, Eq)]
pub(crate) enum SkillScope {
    User,
    Project,
}

#[derive(Subcommand)]
pub(crate) enum GitCmd {
    /// Install the `prepare-commit-msg` git hook that strips Claude/Codex co-author lines and
    /// adds `Co-Authored-By: Forge <noreply@forge.dev>`. Requires `[git] coauthor = true` in
    /// `.forge/config.toml` (or pass `--force` to install regardless).
    Setup {
        /// Install the hook even if `[git] coauthor` is not set in config.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum ImportSource {
    /// Copy `~/.claude/commands/*.md` and `~/.claude/skills/*/` into Forge (user scope by
    /// default). Existing definitions are kept; malformed files are skipped.
    Claude {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
    /// Copy Codex CLI custom prompts (`~/.codex/prompts/*.md`) into Forge as commands (user
    /// scope by default). Existing definitions are kept; malformed files are skipped.
    Codex {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
    /// Import Cursor AI rules (`~/.cursor/rules/*.mdc`) into Forge as commands. Each `.mdc`
    /// rule becomes one command; `globs`/`alwaysApply` fields are dropped — only `description`
    /// and the rule body are kept. Existing commands are not overwritten.
    Cursor {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
    /// Import Aider AI convention files into Forge as commands. Looks for
    /// `CONVENTIONS.md` / `.aider.md` / `.aider.conventions.md` in `~` and then `$PWD`.
    /// Each file becomes one command named after the file. Existing commands are not overwritten.
    Aider {
        /// Import into the project (`./.forge`) instead of the user config dir.
        #[arg(long)]
        project: bool,
    },
}

#[derive(Subcommand)]
pub(crate) enum LatticeOp {
    /// (Re)index the working directory into the graph — incremental by file content hash.
    Update {
        /// Root to index (default: current directory).
        path: Option<String>,
    },
    /// Find symbols by name (case-insensitive); prints kind, location, and signature.
    Query {
        /// Symbol name or fragment.
        query: String,
    },
    /// Blast radius: everything that references a symbol (transitively) — "what breaks if I change X".
    Impact {
        /// Symbol name.
        symbol: String,
    },
    /// Shortest call/reference chain of symbol names from A to B.
    Path {
        /// Source symbol name.
        from: String,
        /// Target symbol name.
        to: String,
    },
    /// Decision provenance: who last changed a symbol's definition, when, and in which commit.
    Why {
        /// Symbol name.
        symbol: String,
    },
    /// Compute + store embeddings for nodes lacking them (semantic retrieval). Uses the
    /// `[lattice.embeddings]` backend (ollama by default). Runs automatically on startup when
    /// embeddings are enabled — this is the manual trigger / one-off.
    Embed,
    /// Show index counts (files, symbols, edges, refs).
    Status,
    /// Print a compact, importance-ranked overview of the repo's key definitions, grouped by file
    /// (aider-style repo-map). Selection is by PageRank so high-centrality symbols appear first;
    /// within each file symbols are shown in source order. Output is token-budgeted.
    Map {
        /// Token budget for the map (default: 2000). Larger = more symbols shown.
        #[arg(long, short = 'b')]
        budget: Option<usize>,
    },
}

#[derive(Subcommand)]
pub(crate) enum McpCmd {
    /// Show the full discovered tool list for one connected server.
    Tools {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
    /// Import a Claude-Code-style `.mcp.json` into `.forge/mcp.toml` (secrets are NOT copied).
    Import {
        /// Path to the `.mcp.json` (default: `./.mcp.json`).
        path: Option<String>,
    },
    /// Obtain OAuth tokens for an OAuth-protected HTTP MCP server (browser-based flow).
    Login {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
    /// Remove stored OAuth tokens for a server (`forge mcp logout <server>`).
    Logout {
        /// Server name (as declared in `.forge/mcp.toml`).
        server: String,
    },
}

#[derive(Clone, Copy, ValueEnum)]
pub(crate) enum Mode {
    #[value(alias = "ask")]
    Default,
    #[value(alias = "auto-edit", alias = "autoedit")]
    AcceptEdits,
    #[value(alias = "full")]
    Bypass,
    #[value(alias = "read-only", alias = "readonly")]
    Plan,
}

impl From<Mode> for forge_types::PermissionMode {
    fn from(m: Mode) -> Self {
        match m {
            Mode::Default => forge_types::PermissionMode::Default,
            Mode::AcceptEdits => forge_types::PermissionMode::AcceptEdits,
            Mode::Bypass => forge_types::PermissionMode::Bypass,
            Mode::Plan => forge_types::PermissionMode::Plan,
        }
    }
}
