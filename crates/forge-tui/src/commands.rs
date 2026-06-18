//! Slash-command registry, parsing, and palette state (RFC session-management-and-commands,
//! PR1). Pure + UI-only: forge-tui defines *what* commands exist and how the palette filters/
//! navigates; the render loop in the binary interprets the resulting [`CommandAction`] (it owns
//! the `Session`, which this crate must not depend on).

/// One command's metadata, shown in the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Command {
    pub name: &'static str,
    pub desc: &'static str,
    pub usage: &'static str,
}

/// The static command registry. Order = display order in `/help`.
pub const COMMANDS: &[Command] = &[
    Command {
        name: "help",
        desc: "show available commands",
        usage: "/help",
    },
    Command {
        name: "sessions",
        desc: "browse & resume past sessions",
        usage: "/sessions",
    },
    Command {
        name: "replay",
        desc:
            "show a session transcript inline (/replay <id>) or diff two sessions (/replay <a> <b>)",
        usage: "/replay <id> [<id2>]",
    },
    Command {
        name: "resume",
        desc: "resume a session by id prefix",
        usage: "/resume <id>",
    },
    Command {
        name: "mode",
        desc: "switch the operating mode (temper)",
        usage: "/mode",
    },
    Command {
        name: "assay",
        desc: "analyze code quality (AI-slop, dead/unsafe/untested) — or full cleanup",
        usage:
            "/assay [--diff|--branch <b>|--since <ref>|<path>] [--only <lens,…>] [--skip <lens,…>]",
    },
    Command {
        name: "model",
        desc: "pin a specific model for this session (/model <id>), or clear the pin (/model)",
        usage: "/model [<id>]",
    },
    Command {
        name: "models",
        desc: "browse available models by provider (counts + frontier/free)",
        usage: "/models",
    },
    Command {
        name: "config",
        desc: "configure Forge — provider & search API keys, bridge plans",
        usage: "/config",
    },
    Command {
        name: "mcp",
        desc: "list connected MCP servers (or one server's tools: /mcp <server>)",
        usage: "/mcp [server]",
    },
    Command {
        name: "new",
        desc: "start a fresh session",
        usage: "/new",
    },
    Command {
        name: "undo",
        desc: "pick a past message to rewind to (chat + file edits)",
        usage: "/undo",
    },
    Command {
        name: "checkpoint",
        desc: "save a named checkpoint here",
        usage: "/checkpoint [name]",
    },
    Command {
        name: "checkpoints",
        desc: "browse & restore checkpoints",
        usage: "/checkpoints",
    },
    Command {
        name: "compact",
        desc: "summarize older messages to free up context",
        usage: "/compact",
    },
    Command {
        name: "lattice",
        desc: "show a symbol's code-intelligence subgraph (callers + provenance)",
        usage: "/lattice <symbol>",
    },
    Command {
        name: "goal",
        desc: "set a session goal and break it into a tracked task plan",
        usage: "/goal <objective>",
    },
    Command {
        name: "loop",
        desc: "re-run a task each turn until the model signals it's complete",
        usage: "/loop <task>",
    },
    Command {
        name: "clear",
        desc: "clear the screen (keep the session)",
        usage: "/clear",
    },
    Command {
        name: "usage",
        desc: "show API spend + token usage across providers",
        usage: "/usage",
    },
    Command {
        name: "mesh",
        desc: "inspect mesh routing — classification, scores, quota, conservation",
        usage: "/mesh [task prompt]",
    },
    Command {
        name: "remote",
        desc: "toggle remote control — drive this session from a phone/desktop browser",
        usage: "/remote [--lan | --local | --anywhere]",
    },
    Command {
        name: "quit",
        desc: "exit Forge",
        usage: "/quit",
    },
];

/// What the render loop must do when a command is accepted. forge-tui produces it; the binary
/// (which owns the `Session`) executes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Help,
    ListSessions,
    Resume(String),
    /// Pin a specific model for all subsequent turns in this session. `None` clears the pin
    /// and returns to mesh routing. `/model <id>` sets; `/model` clears.
    PinModel(Option<String>),
    /// Open the interactive model browser (`/models`).
    ListModels,
    /// Open the interactive config wizard (`/config`) — set provider/search keys + plans.
    Config,
    /// List connected MCP servers (`/mcp`), or one server's full tool list (`/mcp <server>`).
    Mcp(Option<String>),
    New,
    ClearScreen,
    /// Open the operating-mode (temper) picker.
    Mode,
    /// Enter Assay mode — pick analysis-only vs full cleanup, then run the critic crew.
    /// `only`/`skip` are lens name lists; `scope` is `""` (repo), a path, `"--diff"`,
    /// `"--branch <b>"`, or `"--since <ref>"`.
    Assay {
        only: Vec<String>,
        skip: Vec<String>,
        scope: String,
    },
    /// Rewind the last turn (conversation + file edits).
    Undo,
    /// Save a checkpoint at the current point; `None` = an auto/unnamed checkpoint.
    Checkpoint(Option<String>),
    /// Open the checkpoint picker.
    ListCheckpoints,
    /// Summarize older transcript messages to free up context (`/compact`).
    Compact,
    /// Show the code-intelligence subgraph for a symbol (`/lattice <symbol>`).
    Lattice(String),
    /// Set a session goal and decompose it into a tracked task plan (`/goal <objective>`).
    Goal(String),
    /// Re-run a task each turn until the model signals completion (`/loop <task>`).
    Loop(String),
    /// Show a session transcript inline, or diff two sessions (`/replay <id> [<id2>]`).
    Replay(String, Option<String>),
    /// Open the usage overlay showing API spend + token breakdown (`/usage`).
    Usage,
    /// Open the mesh routing inspector; optional prompt to trace (`/mesh [task]`).
    Mesh(Option<String>),
    /// Toggle remote control on/off. When turning on, `mode` selects how the server is exposed.
    /// The render loop prints the connect URL + QR code and lights the statusline indicator.
    Remote {
        mode: RemoteMode,
    },
    Quit,
    /// Not a known command — the binary shows `unknown command: X`.
    Unknown(String),
}

/// How `/remote` exposes the control server to a browser.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteMode {
    /// Bind `0.0.0.0` — reachable from the LAN (the default).
    Lan,
    /// Bind `127.0.0.1` only — control from this machine.
    Local,
    /// Bind loopback and pipe it through a public tunnel (cloudflared/ngrok/bore) so any browser,
    /// anywhere, can reach it — no manual router port-forwarding. The token gate is then the only
    /// thing standing between the public internet and the session.
    Anywhere,
}

/// A `/command` token detected somewhere in the input line, used to drive highlighting and
/// autocomplete. Slash commands are recognized at ANY whitespace-delimited position (not just
/// the first word), so `please run /orchestrate scan` highlights+completes `/orchestrate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashToken {
    /// Byte offset of the leading `/` in the input.
    pub start: usize,
    /// Byte offset just past the command name (where args/whitespace begin).
    pub end: usize,
    /// The command name without the leading slash (the autocomplete/highlight query).
    pub name: String,
}

/// Find the `/command` token to drive the palette for, given the cursor position. We scan every
/// whitespace-delimited word; a word qualifies when it begins with exactly one `/` (a `//literal`
/// escape is skipped). The token *under or just before* the cursor wins, so the palette tracks
/// the word being edited; otherwise the last slash-token on the line is used. Returns `None` when
/// no slash-command token is present.
pub fn slash_token_at(input: &str, cursor: usize) -> Option<SlashToken> {
    let mut best: Option<SlashToken> = None;
    let mut last: Option<SlashToken> = None;
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Advance to the start of a word (skip whitespace).
        if (bytes[i] as char).is_whitespace() {
            i += 1;
            continue;
        }
        let word_start = i;
        // Consume to the next whitespace (word end).
        while i < bytes.len() && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let word_end = i;
        let word = &input[word_start..word_end];
        // A slash command is a single leading `/` followed by a name; `//x` is a literal escape.
        if let Some(rest) = word.strip_prefix('/') {
            if rest.starts_with('/') {
                continue; // `//literal` escape — not a command token.
            }
            // The command name runs until the first whitespace (already the word) — but stop the
            // highlight at the name so `/cmd` in `/cmd arg` only spans the command word itself.
            let name = rest.to_string();
            let tok = SlashToken {
                start: word_start,
                end: word_end,
                name,
            };
            // Prefer the token the cursor is editing (cursor within [start, end]).
            if cursor >= tok.start && cursor <= tok.end {
                best = Some(tok.clone());
            }
            last = Some(tok);
        }
    }
    best.or(last)
}

/// An `@path` token found in the input line — drives the file-path autocomplete popup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AtToken {
    pub start: usize,
    pub end: usize,
    pub query: String,
}

/// Find the `@path` token to drive the file-path picker for, given the cursor position.
/// Scans every whitespace-delimited word; qualifies when it begins with `@` AND the cursor
/// is inside the token (cursor within [start, end]). Unlike the slash palette, there is no
/// last-token fallback — the picker closes as soon as the cursor moves off the `@` word.
pub fn at_token_at(input: &str, cursor: usize) -> Option<AtToken> {
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if (bytes[i] as char).is_whitespace() {
            i += 1;
            continue;
        }
        let word_start = i;
        while i < bytes.len() && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let word_end = i;
        let word = &input[word_start..word_end];
        if let Some(rest) = word.strip_prefix('@') {
            if cursor >= word_start && cursor <= word_end {
                return Some(AtToken {
                    start: word_start,
                    end: word_end,
                    query: rest.to_string(),
                });
            }
        }
    }
    None
}

/// Inline file-path picker state. Opens when the input line contains an `@path` token at cursor.
#[derive(Debug, Clone, Default)]
pub struct AtPathPicker {
    pub open: bool,
    pub query: String,
    pub selected: usize,
    pub anim: f32,
    files: Vec<String>,
}

impl AtPathPicker {
    pub fn open_with(&mut self, query: &str, files: Vec<String>) {
        self.open = true;
        self.query = query.to_string();
        self.files = files;
        self.selected = 0;
        self.anim = 0.0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
        self.anim = 0.0;
    }

    pub fn matches(&self) -> Vec<&String> {
        if self.query.is_empty() {
            return self.files.iter().collect();
        }
        let q = self.query.to_lowercase();
        self.files
            .iter()
            .filter(|f| f.to_lowercase().contains(&q))
            .collect()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        let n = self.matches().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    pub fn clamp(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    pub fn selected_path(&self) -> Option<String> {
        self.matches().into_iter().nth(self.selected).cloned()
    }

    pub fn tick_anim(&mut self) {
        if self.open && self.anim < 1.0 {
            self.anim = (self.anim + 0.34).min(1.0);
        }
    }
}

/// Extract a comma-separated lens list from `--flag <value>` in a raw arg string.
/// `/assay --only dead-weight,unsafe` → `extract_flag(arg, "--only")` → `["dead-weight", "unsafe"]`
fn extract_flag(arg: &str, flag: &str) -> Vec<String> {
    let tokens: Vec<&str> = arg.split_whitespace().collect();
    for (i, tok) in tokens.iter().enumerate() {
        if *tok == flag {
            if let Some(val) = tokens.get(i + 1) {
                if !val.starts_with('-') {
                    return val
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                }
            }
        }
    }
    Vec::new()
}

/// Check whether a boolean flag (no value) is present in `arg`.
fn has_flag(arg: &str, flag: &str) -> bool {
    arg.split_whitespace().any(|t| t == flag)
}

/// Parse a submitted command line (`"/resume ab12"`). The leading `/` is required; a `//`
/// prefix is NOT a command (it escapes to a literal prompt — handled by the caller).
pub fn parse_command(line: &str) -> CommandAction {
    let line = line.trim();
    let body = line.strip_prefix('/').unwrap_or(line).trim();
    let mut parts = body.splitn(2, char::is_whitespace);
    let name = parts.next().unwrap_or("").to_lowercase();
    let arg = parts.next().unwrap_or("").trim().to_string();
    match name.as_str() {
        "help" | "h" | "?" => CommandAction::Help,
        "sessions" | "ls" => CommandAction::ListSessions,
        "resume" | "r" => {
            if arg.is_empty() {
                CommandAction::ListSessions // /resume with no id → open the picker
            } else {
                CommandAction::Resume(arg)
            }
        }
        "model" => CommandAction::PinModel((!arg.is_empty()).then_some(arg)),
        "models" | "mc" => CommandAction::ListModels,
        "config" | "cfg" | "settings" => CommandAction::Config,
        "mcp" => CommandAction::Mcp((!arg.is_empty()).then_some(arg)),
        "new" | "n" => CommandAction::New,
        "mode" | "m" | "temper" => CommandAction::Mode,
        "assay" | "analyze" | "analyse" => {
            // `/assay [--diff|--branch <b>|--since <ref>|<path>] [--only <lens,…>] [--skip <lens,…>]`
            let only = extract_flag(&arg, "--only");
            let skip = extract_flag(&arg, "--skip");
            // Scope: --diff, --branch <b>, --since <ref>, a path, or empty (full repo).
            let scope = if has_flag(&arg, "--diff") {
                "--diff".to_string()
            } else if let Some(b) = extract_flag(&arg, "--branch").into_iter().next() {
                format!("--branch {b}")
            } else if let Some(r) = extract_flag(&arg, "--since").into_iter().next() {
                format!("--since {r}")
            } else {
                // Remaining tokens that aren't flags → treat as path.
                let path: String = arg
                    .split_whitespace()
                    .filter(|t| !t.starts_with("--"))
                    .collect::<Vec<_>>()
                    .join(" ");
                path
            };
            CommandAction::Assay { only, skip, scope }
        }
        "undo" | "u" => CommandAction::Undo,
        "checkpoint" | "cp" => CommandAction::Checkpoint((!arg.is_empty()).then_some(arg)),
        "checkpoints" => CommandAction::ListCheckpoints,
        "compact" => CommandAction::Compact,
        "lattice" | "lat" => CommandAction::Lattice(arg),
        "goal" | "objective" => CommandAction::Goal(arg),
        "loop" => CommandAction::Loop(arg),
        "replay" => {
            // `/replay <id>` or `/replay <a> <b>`
            let mut ids = arg.splitn(2, char::is_whitespace);
            let id_a = ids.next().unwrap_or("").trim().to_string();
            let id_b = ids
                .next()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            CommandAction::Replay(id_a, id_b)
        }
        "clear" | "cls" => CommandAction::ClearScreen,
        "usage" => CommandAction::Usage,
        "mesh" => CommandAction::Mesh((!arg.is_empty()).then_some(arg)),
        "remote" | "rc" => {
            // `/remote` toggles. `--anywhere`/`-a` pipes through a public tunnel (reachable from
            // any network, no port-forward); `--local` binds loopback only (this machine);
            // default `--lan` binds 0.0.0.0 so a phone on the same network can connect.
            let mode = if has_flag(&arg, "--anywhere") || has_flag(&arg, "-a") {
                RemoteMode::Anywhere
            } else if has_flag(&arg, "--local") {
                RemoteMode::Local
            } else {
                RemoteMode::Lan
            };
            CommandAction::Remote { mode }
        }
        "quit" | "exit" | "q" => CommandAction::Quit,
        other => CommandAction::Unknown(other.to_string()),
    }
}

/// Rank a command against a query: `Some(score)` if it matches (lower = better), `None` if not.
/// Prefix matches rank above subsequence (fuzzy) matches; an empty query matches everything.
fn match_score(name: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1000); // stable order preserved by the caller's enumerate tiebreak
    }
    let (name, query) = (name.to_lowercase(), query.to_lowercase());
    if name.starts_with(&query) {
        return Some(query.len() as u32); // exact-prefix: best, shorter query already typed = closer
    }
    // Subsequence fuzzy: every query char appears in order.
    let mut q = query.chars().peekable();
    for c in name.chars() {
        if q.peek() == Some(&c) {
            q.next();
        }
    }
    if q.peek().is_none() {
        Some(500 + name.len() as u32) // matched, but worse than any prefix hit
    } else {
        None
    }
}

/// Commands matching `query`, best-first (prefix before fuzzy, then registry order).
pub fn filter_commands(query: &str) -> Vec<&'static Command> {
    let mut scored: Vec<(u32, usize, &'static Command)> = COMMANDS
        .iter()
        .enumerate()
        .filter_map(|(i, c)| match_score(c.name, query).map(|s| (s, i, c)))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    scored.into_iter().map(|(_, _, c)| c).collect()
}

/// One palette row, owned so it can mix the static builtins with file-based commands/skills
/// (the binary populates [`Palette::extra`] from `forge_skills::Catalog`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaletteEntry {
    pub name: String,
    pub desc: String,
}

/// Inline command-palette state. Opens when the input line starts with `/`. The binary's render
/// loop drives it via keystrokes and reads the filtered list / selection.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub open: bool,
    /// Text after the leading `/` (the filter query).
    pub query: String,
    pub selected: usize,
    /// Eases 0.0 → 1.0 on open for the reveal animation (advanced by the render tick).
    pub anim: f32,
    /// File-based commands + skills discovered at startup, shown alongside the builtins. Set
    /// once by the binary; preserved across open/close.
    pub extra: Vec<PaletteEntry>,
}

impl Palette {
    pub fn open_with(&mut self, query: &str) {
        self.open = true;
        self.query = query.to_string();
        self.selected = 0;
        self.anim = 0.0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.query.clear();
        self.selected = 0;
        self.anim = 0.0;
    }

    /// The currently-filtered palette rows: builtins + any file-based `extra`, ranked best-first
    /// (prefix before fuzzy, then order), deduped by name (a builtin wins a name clash).
    pub fn matches(&self) -> Vec<PaletteEntry> {
        let mut all: Vec<PaletteEntry> = COMMANDS
            .iter()
            .map(|c| PaletteEntry {
                name: c.name.to_string(),
                desc: c.desc.to_string(),
            })
            .collect();
        all.extend(self.extra.iter().cloned());
        let mut scored: Vec<(u32, usize, PaletteEntry)> = all
            .into_iter()
            .enumerate()
            .filter_map(|(i, e)| match_score(&e.name, &self.query).map(|s| (s, i, e)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        let mut seen = std::collections::HashSet::new();
        scored
            .into_iter()
            .filter(|(_, _, e)| seen.insert(e.name.clone()))
            .map(|(_, _, e)| e)
            .collect()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        let n = self.matches().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    /// Clamp the selection after the list shrinks (query got more specific).
    pub fn clamp(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// The selected row's name, for Tab-completion / Enter.
    pub fn selected_name(&self) -> Option<String> {
        self.matches()
            .into_iter()
            .nth(self.selected)
            .map(|e| e.name)
    }

    /// Advance the reveal animation toward 1.0 (called per render tick while open).
    pub fn tick_anim(&mut self) {
        if self.open && self.anim < 1.0 {
            self.anim = (self.anim + 0.34).min(1.0);
        }
    }
}

/// What an open [`Picker`] is selecting, so the render loop knows what `Enter` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickerKind {
    /// Pick a past session to resume (`/sessions`, `/resume`).
    Sessions,
    /// Pick a checkpoint to rewind to (`/checkpoints`).
    Checkpoints,
    /// Pick the operating mode / temper (`/mode`).
    Tempers,
    /// Pick the Assay action: analysis-only vs full cleanup (`/assay`).
    AssayChoice,
    /// Browse available models (`/models`): a provider list that drills into per-provider models.
    Models,
}

/// One row in an interactive picker: an opaque `id` the loop acts on, plus two display strings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    /// What the loop resolves on Enter (a session id, or a checkpoint boundary seq as a string).
    pub id: String,
    pub title: String,
    pub subtitle: String,
}

/// A full-width, animated, filterable selection list (RFC session-management-and-commands).
/// Reused for `/sessions`, `/resume`, and `/checkpoints` — the loop populates `rows` from the
/// store, then drives it with the same keys as the command palette. Filter narrows as you type.
#[derive(Debug, Clone, Default)]
pub struct Picker {
    pub open: bool,
    pub kind: Option<PickerKind>,
    rows: Vec<PickerRow>,
    pub query: String,
    pub selected: usize,
    pub anim: f32,
    /// A one-line title shown above the list (e.g. "resume a session").
    pub heading: String,
}

impl Picker {
    pub fn open_with(&mut self, kind: PickerKind, heading: &str, rows: Vec<PickerRow>) {
        self.open = true;
        self.kind = Some(kind);
        self.rows = rows;
        self.heading = heading.to_string();
        self.query.clear();
        self.selected = 0;
        self.anim = 0.0;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.kind = None;
        self.rows.clear();
        self.query.clear();
        self.heading.clear();
        self.selected = 0;
        self.anim = 0.0;
    }

    /// Rows matching the current query (case-insensitive substring over id/title/subtitle).
    pub fn matches(&self) -> Vec<&PickerRow> {
        if self.query.is_empty() {
            return self.rows.iter().collect();
        }
        let q = self.query.to_lowercase();
        self.rows
            .iter()
            .filter(|r| {
                r.id.to_lowercase().contains(&q)
                    || r.title.to_lowercase().contains(&q)
                    || r.subtitle.to_lowercase().contains(&q)
            })
            .collect()
    }

    pub fn move_up(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    pub fn move_down(&mut self) {
        let n = self.matches().len();
        if n > 0 {
            self.selected = (self.selected + 1).min(n - 1);
        }
    }

    /// Re-clamp the selection after the filtered list shrinks.
    pub fn clamp(&mut self) {
        let n = self.matches().len();
        if n == 0 {
            self.selected = 0;
        } else if self.selected >= n {
            self.selected = n - 1;
        }
    }

    /// The selected row, if any (after filtering).
    pub fn selected_row(&self) -> Option<&PickerRow> {
        self.matches().into_iter().nth(self.selected)
    }

    pub fn tick_anim(&mut self) {
        if self.open && self.anim < 1.0 {
            self.anim = (self.anim + 0.34).min(1.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows() -> Vec<PickerRow> {
        vec![
            PickerRow {
                id: "aaa".into(),
                title: "aaa  $0.01  2 msgs".into(),
                subtitle: "fix the auth bug".into(),
            },
            PickerRow {
                id: "bbb".into(),
                title: "bbb  $0.02  5 msgs".into(),
                subtitle: "refactor the mesh".into(),
            },
        ]
    }

    #[test]
    fn parses_new_commands() {
        assert_eq!(parse_command("/mode"), CommandAction::Mode);
        assert_eq!(parse_command("/m"), CommandAction::Mode);
        assert_eq!(parse_command("/undo"), CommandAction::Undo);
        assert_eq!(
            parse_command("/checkpoints"),
            CommandAction::ListCheckpoints
        );
        assert_eq!(
            parse_command("/checkpoint"),
            CommandAction::Checkpoint(None)
        );
        assert_eq!(
            parse_command("/checkpoint before refactor"),
            CommandAction::Checkpoint(Some("before refactor".into()))
        );
        assert_eq!(parse_command("/config"), CommandAction::Config);
        assert_eq!(parse_command("/cfg"), CommandAction::Config);
        assert_eq!(parse_command("/settings"), CommandAction::Config);
        assert_eq!(parse_command("/mcp"), CommandAction::Mcp(None));
        assert_eq!(
            parse_command("/mcp gitlab"),
            CommandAction::Mcp(Some("gitlab".into()))
        );
        assert_eq!(
            parse_command("/goal ship the parser"),
            CommandAction::Goal("ship the parser".into())
        );
        assert_eq!(parse_command("/goal"), CommandAction::Goal(String::new()));
        assert_eq!(
            parse_command("/loop fix all warnings"),
            CommandAction::Loop("fix all warnings".into())
        );
    }

    #[test]
    fn parses_remote_command_and_alias() {
        // `/remote` (and alias `/rc`) toggle on with LAN binding by default.
        assert_eq!(
            parse_command("/remote"),
            CommandAction::Remote {
                mode: RemoteMode::Lan
            }
        );
        assert_eq!(
            parse_command("/rc"),
            CommandAction::Remote {
                mode: RemoteMode::Lan
            }
        );
        // `--local` binds loopback only; `--lan` is the explicit default.
        assert_eq!(
            parse_command("/remote --local"),
            CommandAction::Remote {
                mode: RemoteMode::Local
            }
        );
        assert_eq!(
            parse_command("/rc --lan"),
            CommandAction::Remote {
                mode: RemoteMode::Lan
            }
        );
        // `--anywhere` (and `-a`) pipe through a public tunnel.
        assert_eq!(
            parse_command("/remote --anywhere"),
            CommandAction::Remote {
                mode: RemoteMode::Anywhere
            }
        );
        assert_eq!(
            parse_command("/rc -a"),
            CommandAction::Remote {
                mode: RemoteMode::Anywhere
            }
        );
    }

    #[test]
    fn picker_filters_navigates_and_resolves_a_row() {
        let mut p = Picker::default();
        p.open_with(PickerKind::Sessions, "resume", rows());
        assert_eq!(p.matches().len(), 2);
        assert_eq!(p.selected_row().unwrap().id, "aaa");

        p.move_down();
        assert_eq!(p.selected_row().unwrap().id, "bbb");
        p.move_up();
        assert_eq!(p.selected_row().unwrap().id, "aaa");

        // Filter narrows the list; selection re-clamps.
        p.query = "mesh".into();
        p.clamp();
        let m = p.matches();
        assert_eq!(m.len(), 1, "only the mesh session matches");
        assert_eq!(p.selected_row().unwrap().id, "bbb");
    }

    #[test]
    fn closing_a_picker_clears_it() {
        let mut p = Picker::default();
        p.open_with(PickerKind::Checkpoints, "restore", rows());
        p.close();
        assert!(!p.open);
        assert!(p.kind.is_none());
        assert!(p.matches().is_empty());
    }

    #[test]
    fn parses_commands_and_args() {
        assert_eq!(parse_command("/help"), CommandAction::Help);
        assert_eq!(parse_command("/h"), CommandAction::Help);
        assert_eq!(
            parse_command("/resume ab12"),
            CommandAction::Resume("ab12".into())
        );
        assert_eq!(parse_command("/resume"), CommandAction::ListSessions);
        assert_eq!(parse_command("/new"), CommandAction::New);
        assert_eq!(parse_command("/clear"), CommandAction::ClearScreen);
        assert_eq!(parse_command("/quit"), CommandAction::Quit);
        assert_eq!(
            parse_command("/lattice Session"),
            CommandAction::Lattice("Session".into())
        );
        assert_eq!(
            parse_command("/lat foo"),
            CommandAction::Lattice("foo".into())
        );
        assert_eq!(
            parse_command("/bogus"),
            CommandAction::Unknown("bogus".into())
        );
    }

    #[test]
    fn resume_keeps_only_the_id_arg() {
        assert_eq!(
            parse_command("/resume   7f3a-9 "),
            CommandAction::Resume("7f3a-9".into())
        );
    }

    #[test]
    fn empty_query_lists_all_in_registry_order() {
        let all = filter_commands("");
        assert_eq!(all.len(), COMMANDS.len());
        assert_eq!(all[0].name, "help");
    }

    #[test]
    fn prefix_matches_rank_above_fuzzy() {
        // "s" is a prefix of "sessions"; it's a subsequence of "clear"? no. of "sessions" prefix.
        let m = filter_commands("se");
        assert_eq!(m[0].name, "sessions", "prefix hit first");
    }

    #[test]
    fn fuzzy_subsequence_matches() {
        // "clr" is a subsequence of "clear" but not a prefix.
        let m = filter_commands("clr");
        assert!(m.iter().any(|c| c.name == "clear"), "fuzzy matched clear");
    }

    #[test]
    fn no_match_returns_empty() {
        assert!(filter_commands("zzzzz").is_empty());
    }

    #[test]
    fn palette_navigation_clamps() {
        let mut p = Palette::default();
        p.open_with("");
        assert_eq!(p.selected, 0);
        p.move_up(); // can't go below 0
        assert_eq!(p.selected, 0);
        for _ in 0..100 {
            p.move_down();
        }
        assert_eq!(p.selected, COMMANDS.len() - 1, "clamped to last");
        // Narrow the query so the list shrinks; selection must re-clamp.
        p.query = "help".into();
        p.clamp();
        assert_eq!(p.selected, 0);
        assert_eq!(p.selected_name().as_deref(), Some("help"));
    }

    #[test]
    fn palette_merges_file_entries_and_builtin_wins_name_clash() {
        let mut p = Palette {
            extra: vec![
                PaletteEntry {
                    name: "review".into(),
                    desc: "file command".into(),
                },
                // Same name as a builtin: the builtin must win (no duplicate row).
                PaletteEntry {
                    name: "clear".into(),
                    desc: "shadowing attempt".into(),
                },
            ],
            ..Default::default()
        };
        p.open_with("");
        let names: Vec<String> = p.matches().into_iter().map(|e| e.name).collect();
        assert!(names.iter().any(|n| n == "review"), "file command shown");
        assert_eq!(
            names.iter().filter(|n| *n == "clear").count(),
            1,
            "no duplicate 'clear'"
        );
        let clear = p.matches().into_iter().find(|e| e.name == "clear").unwrap();
        assert_ne!(clear.desc, "shadowing attempt", "builtin clear wins");

        // Filtering still works over the merged set.
        p.query = "review".into();
        p.clamp();
        assert_eq!(p.selected_name().as_deref(), Some("review"));
    }

    #[test]
    fn slash_token_detected_at_leading_position() {
        let t = slash_token_at("/orchestrate scan repo", 0).unwrap();
        assert_eq!(t.name, "orchestrate");
        assert_eq!(t.start, 0);
        assert_eq!(t.end, "/orchestrate".len());
    }

    #[test]
    fn slash_token_detected_mid_line() {
        let input = "please run /orchestrate scan repo";
        let off = input.find("/orchestrate").unwrap();
        // Cursor anywhere on the line still finds the only slash-token.
        let t = slash_token_at(input, input.len()).unwrap();
        assert_eq!(t.name, "orchestrate");
        assert_eq!(t.start, off);
    }

    #[test]
    fn slash_token_under_cursor_wins_when_multiple() {
        // Two slash tokens; the one the cursor is editing is selected.
        let input = "/help and /clear";
        let clear_off = input.find("/clear").unwrap();
        // Cursor at the end of `/clear` → that token.
        let t = slash_token_at(input, input.len()).unwrap();
        assert_eq!(t.name, "clear");
        assert_eq!(t.start, clear_off);
        // Cursor on `/help` (offset 3, inside the first token) → first token.
        let t = slash_token_at(input, 3).unwrap();
        assert_eq!(t.name, "help");
        assert_eq!(t.start, 0);
    }

    #[test]
    fn double_slash_is_literal_not_a_command() {
        assert!(slash_token_at("//literal", 0).is_none());
        assert!(slash_token_at("run //escaped here", 18.min("run //escaped here".len())).is_none());
        // A real command alongside an escape is still detected.
        let t = slash_token_at("//lit and /help", 15).unwrap();
        assert_eq!(t.name, "help");
    }

    #[test]
    fn no_slash_token_returns_none() {
        assert!(slash_token_at("just plain text", 5).is_none());
        assert!(slash_token_at("", 0).is_none());
    }

    #[test]
    fn anim_eases_to_one() {
        let mut p = Palette::default();
        p.open_with("");
        for _ in 0..10 {
            p.tick_anim();
        }
        assert_eq!(p.anim, 1.0);
    }
}
