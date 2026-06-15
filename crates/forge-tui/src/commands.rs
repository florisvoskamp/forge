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
        name: "resume",
        desc: "resume a session by id prefix",
        usage: "/resume <id>",
    },
    Command {
        name: "new",
        desc: "start a fresh session",
        usage: "/new",
    },
    Command {
        name: "undo",
        desc: "rewind the last turn (chat + file edits)",
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
        name: "clear",
        desc: "clear the screen (keep the session)",
        usage: "/clear",
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
    New,
    ClearScreen,
    /// Rewind the last turn (conversation + file edits).
    Undo,
    /// Save a checkpoint at the current point; `None` = an auto/unnamed checkpoint.
    Checkpoint(Option<String>),
    /// Open the checkpoint picker.
    ListCheckpoints,
    Quit,
    /// Not a known command — the binary shows `unknown command: X`.
    Unknown(String),
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
        "new" | "n" => CommandAction::New,
        "undo" | "u" => CommandAction::Undo,
        "checkpoint" | "cp" => CommandAction::Checkpoint((!arg.is_empty()).then_some(arg)),
        "checkpoints" => CommandAction::ListCheckpoints,
        "clear" | "cls" => CommandAction::ClearScreen,
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

/// Inline command-palette state. Opens when the input line starts with `/`. The binary's render
/// loop drives it via keystrokes and reads [`Palette::accepted`] / the filtered list.
#[derive(Debug, Clone, Default)]
pub struct Palette {
    pub open: bool,
    /// Text after the leading `/` (the filter query).
    pub query: String,
    pub selected: usize,
    /// Eases 0.0 → 1.0 on open for the reveal animation (advanced by the render tick).
    pub anim: f32,
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

    /// The currently-filtered commands.
    pub fn matches(&self) -> Vec<&'static Command> {
        filter_commands(&self.query)
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

    /// The selected command's name, for Tab-completion / Enter.
    pub fn selected_name(&self) -> Option<&'static str> {
        self.matches().get(self.selected).map(|c| c.name)
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
        assert_eq!(p.selected_name(), Some("help"));
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
