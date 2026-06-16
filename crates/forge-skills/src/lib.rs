//! Slash-command + skill catalog (docs/features/command-skill-system.md).
//!
//! Pure and synchronous: this crate defines *what* a user can invoke (named prompt templates
//! and reusable methodologies) and resolves a typed `/line` into an expanded prompt + optional
//! system guidance. It does not touch the network, the provider, or the agent loop — the
//! binary feeds the resolved invocation into `Session::run_turn_with`.
//!
//! Format-compatible with Claude Code: `~/.claude/commands/*.md` and
//! `~/.claude/skills/<name>/SKILL.md` parse without modification (lenient frontmatter, the same
//! `$1`/`$ARGUMENTS` substitution). Reading those files is this crate's job; *migrating* them
//! into Forge scopes is a separate import layer.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use forge_types::TaskTier;

mod frontmatter;
mod template;

pub use template::expand;

/// Where a definition came from. Precedence: `Project` > `User` > `Builtin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Builtin,
    User,
    Project,
}

impl Scope {
    pub fn label(self) -> &'static str {
        match self {
            Scope::Builtin => "builtin",
            Scope::User => "user",
            Scope::Project => "project",
        }
    }
}

/// A directory to discover definitions in, tagged with the scope it contributes.
#[derive(Debug, Clone)]
pub struct ScopedDir {
    pub scope: Scope,
    pub path: PathBuf,
}

/// The filesystem sources a [`Catalog`] is built from (ordered low → high precedence is not
/// required; precedence is decided by [`Scope`] on collision).
#[derive(Debug, Clone, Default)]
pub struct Sources {
    pub commands: Vec<ScopedDir>,
    pub skills: Vec<ScopedDir>,
}

/// A declared command argument. `required` args missing at invoke time short-circuit before any
/// model call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub required: bool,
}

/// A slash command: a markdown file whose body is a prompt template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub name: String,
    pub description: String,
    pub args: Vec<ArgSpec>,
    pub tier: Option<TaskTier>,
    pub model: Option<String>,
    pub body: String,
    pub scope: Scope,
    pub path: PathBuf,
}

impl Command {
    /// A one-line `<a> [b]` hint of the declared args (empty when none).
    pub fn arg_hint(&self) -> String {
        self.args
            .iter()
            .map(|a| {
                if a.required {
                    format!("<{}>", a.name)
                } else {
                    format!("[{}]", a.name)
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Skill metadata — cheap, loaded at discovery (progressive disclosure: the body and resources
/// are NOT read until the skill is invoked, see [`Skill::load`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub tier: Option<TaskTier>,
    pub resources: Vec<String>,
    pub dir: PathBuf,
    pub scope: Scope,
}

/// A fully-loaded skill: the methodology body plus any readable bundled resources. Built by
/// [`Skill::load`] at invoke time only.
#[derive(Debug, Clone)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
    /// `(name, contents)` for each resource that loaded; missing ones are skipped + warned.
    pub resources: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

impl Skill {
    /// Read the skill body + resources from disk (progressive disclosure). Never fails: a
    /// missing/unreadable resource is recorded as a warning and the skill still runs.
    pub fn load(meta: &SkillMeta) -> Skill {
        let mut warnings = Vec::new();
        let body = match std::fs::read_to_string(meta.dir.join("SKILL.md")) {
            Ok(raw) => frontmatter::split(&raw).1.trim().to_string(),
            Err(e) => {
                warnings.push(format!("skill {}: cannot read SKILL.md ({e})", meta.name));
                String::new()
            }
        };
        let mut resources = Vec::new();
        for res in &meta.resources {
            match std::fs::read_to_string(meta.dir.join(res)) {
                Ok(c) => resources.push((res.clone(), c)),
                Err(_) => warnings.push(format!("skill {}: missing resource {res}", meta.name)),
            }
        }
        Skill {
            meta: meta.clone(),
            body,
            resources,
            warnings,
        }
    }

    /// The full guidance block injected into the turn: the methodology body followed by each
    /// loaded resource under a header.
    pub fn guidance(&self) -> String {
        let mut out = self.body.clone();
        for (name, contents) in &self.resources {
            out.push_str(&format!("\n\n--- resource: {name} ---\n{contents}"));
        }
        out
    }
}

/// The outcome of resolving a submitted line against the catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A command expanded into a ready-to-run prompt (plus any system guidance to prepend).
    Command {
        cmd: Command,
        prompt: String,
        guidance: Vec<String>,
    },
    /// A skill invocation: the binary loads the body lazily, then runs `prompt` (may be empty).
    Skill { meta: SkillMeta, prompt: String },
    /// A `/name` that matches nothing.
    Unknown(String),
    /// A command invoked without its required args; no model call should be made.
    MissingArgs { name: String, missing: Vec<String> },
    /// Not a slash invocation (or an escaped `//literal`): pass straight to `run_turn`.
    Plain(String),
}

/// One palette / listing entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub description: String,
    pub scope: Scope,
    pub is_skill: bool,
    /// True when this definition shadows a lower-scope one of the same name.
    pub shadows: bool,
}

/// The merged, precedence-resolved index, built once per session. Construction never fails;
/// malformed files are skipped and collected in [`Catalog::warnings`].
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    commands: BTreeMap<String, Command>,
    skills: BTreeMap<String, SkillMeta>,
    shadowed: Vec<(String, Scope)>,
    warnings: Vec<String>,
}

impl Catalog {
    /// Discover all commands + skills across `sources`, applying scope precedence
    /// (`Project` > `User` > `Builtin`) on name collision.
    pub fn load(sources: &Sources) -> Catalog {
        let mut cat = Catalog::default();
        for dir in &sources.commands {
            cat.load_commands_dir(dir);
        }
        for dir in &sources.skills {
            cat.load_skills_dir(dir);
        }
        cat
    }

    fn load_commands_dir(&mut self, dir: &ScopedDir) {
        let entries = match std::fs::read_dir(&dir.path) {
            Ok(e) => e,
            Err(_) => return, // a non-existent scope dir is normal, not an error
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let stem = match path.file_stem().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let raw = match std::fs::read_to_string(&path) {
                Ok(r) => r,
                Err(e) => {
                    self.warnings.push(format!("{}: {e}", path.display()));
                    continue;
                }
            };
            match parse_command(&raw, &stem, dir.scope, &path) {
                Ok(cmd) => self.insert_command(cmd),
                Err(e) => self
                    .warnings
                    .push(format!("{}: skipped — {e}", path.display())),
            }
        }
    }

    fn load_skills_dir(&mut self, dir: &ScopedDir) {
        let entries = match std::fs::read_dir(&dir.path) {
            Ok(e) => e,
            Err(_) => return,
        };
        for entry in entries.flatten() {
            let sub = entry.path();
            if !sub.is_dir() {
                continue;
            }
            let skill_md = sub.join("SKILL.md");
            if !skill_md.exists() {
                continue;
            }
            let stem = match sub.file_name().and_then(|s| s.to_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let raw = match std::fs::read_to_string(&skill_md) {
                Ok(r) => r,
                Err(e) => {
                    self.warnings.push(format!("{}: {e}", skill_md.display()));
                    continue;
                }
            };
            match parse_skill_meta(&raw, &stem, dir.scope, &sub) {
                Ok(meta) => self.insert_skill(meta),
                Err(e) => self
                    .warnings
                    .push(format!("{}: skipped — {e}", skill_md.display())),
            }
        }
    }

    fn insert_command(&mut self, cmd: Command) {
        match self.commands.get(&cmd.name) {
            Some(existing) if existing.scope >= cmd.scope => {
                self.shadowed.push((cmd.name.clone(), cmd.scope));
            }
            Some(existing) => {
                self.shadowed.push((existing.name.clone(), existing.scope));
                self.commands.insert(cmd.name.clone(), cmd);
            }
            None => {
                self.commands.insert(cmd.name.clone(), cmd);
            }
        }
    }

    fn insert_skill(&mut self, meta: SkillMeta) {
        match self.skills.get(&meta.name) {
            Some(existing) if existing.scope >= meta.scope => {
                self.shadowed.push((meta.name.clone(), meta.scope));
            }
            Some(existing) => {
                self.shadowed.push((existing.name.clone(), existing.scope));
                self.skills.insert(meta.name.clone(), meta);
            }
            None => {
                self.skills.insert(meta.name.clone(), meta);
            }
        }
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn command(&self, name: &str) -> Option<&Command> {
        self.commands.get(name)
    }

    pub fn skill(&self, name: &str) -> Option<&SkillMeta> {
        self.skills.get(name)
    }

    pub fn is_empty(&self) -> bool {
        self.commands.is_empty() && self.skills.is_empty()
    }

    /// Every resolved command (winning definition per name). For tooling like import that needs
    /// the underlying file paths.
    pub fn all_commands(&self) -> Vec<&Command> {
        self.commands.values().collect()
    }

    /// Every resolved skill metadata (winning definition per name).
    pub fn all_skills(&self) -> Vec<&SkillMeta> {
        self.skills.values().collect()
    }

    /// All entries (commands then any skill not shadowed by a same-named command), sorted by
    /// name — the source for `forge commands` and `/help`.
    pub fn entries(&self) -> Vec<Entry> {
        let mut out: Vec<Entry> = Vec::new();
        for cmd in self.commands.values() {
            out.push(Entry {
                name: cmd.name.clone(),
                description: self.display_description(cmd),
                scope: cmd.scope,
                is_skill: false,
                shadows: self.shadows(&cmd.name, cmd.scope),
            });
        }
        for meta in self.skills.values() {
            if self.commands.contains_key(&meta.name) {
                continue; // command wins the bare /name; the skill is still reachable via /skill
            }
            out.push(Entry {
                name: meta.name.clone(),
                description: meta.description.clone(),
                scope: meta.scope,
                is_skill: true,
                shadows: self.shadows(&meta.name, meta.scope),
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    fn shadows(&self, name: &str, scope: Scope) -> bool {
        self.shadowed.iter().any(|(n, s)| n == name && *s < scope)
    }

    /// The description to show for a command. A Claude-Code wrapper whose own "description" is just
    /// the delegation sentence ("Use the **X** skill …") is unhelpful in the palette, so when the
    /// command delegates to a known skill, show that skill's description instead.
    fn display_description(&self, cmd: &Command) -> String {
        if cmd.description.starts_with("Use the ") {
            if let Some(meta) = bold_tokens(&cmd.description)
                .into_iter()
                .find_map(|t| self.skills.get(&t))
            {
                return meta.description.clone();
            }
        }
        cmd.description.clone()
    }

    /// Fuzzy-rank entries against `query` (prefix beats subsequence), best-first, capped at
    /// `limit`. An empty query returns all entries in name order.
    pub fn fuzzy(&self, query: &str, limit: usize) -> Vec<Entry> {
        let mut scored: Vec<(u32, Entry)> = self
            .entries()
            .into_iter()
            .filter_map(|e| match_score(&e.name, query).map(|s| (s, e)))
            .collect();
        scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name.cmp(&b.1.name)));
        scored.into_iter().take(limit).map(|(_, e)| e).collect()
    }

    /// Resolve a submitted line into an actionable [`Resolved`].
    pub fn resolve(&self, line: &str) -> Resolved {
        let line = line.trim();
        if !line.starts_with('/') {
            return Resolved::Plain(line.to_string());
        }
        if let Some(lit) = line.strip_prefix("//") {
            return Resolved::Plain(lit.to_string()); // `//foo` escapes to a literal prompt
        }
        let body = &line[1..];
        let mut parts = body.splitn(2, char::is_whitespace);
        let name = parts.next().unwrap_or("").to_string();
        let rest = parts.next().unwrap_or("").trim().to_string();

        if name == "skill" {
            let mut sp = rest.splitn(2, char::is_whitespace);
            let sname = sp.next().unwrap_or("").to_string();
            let sprompt = sp.next().unwrap_or("").trim().to_string();
            return match self.skills.get(&sname) {
                Some(meta) => Resolved::Skill {
                    meta: meta.clone(),
                    prompt: sprompt,
                },
                None => Resolved::Unknown(format!("skill {sname}").trim().to_string()),
            };
        }

        if let Some(cmd) = self.commands.get(&name) {
            let positional: Vec<&str> = if rest.is_empty() {
                Vec::new()
            } else {
                rest.split_whitespace().collect()
            };
            let missing: Vec<String> = cmd
                .args
                .iter()
                .enumerate()
                .filter(|(i, a)| {
                    a.required && positional.get(*i).map(|s| s.is_empty()) != Some(false)
                })
                .map(|(_, a)| a.name.clone())
                .collect();
            if !missing.is_empty() {
                return Resolved::MissingArgs {
                    name: cmd.name.clone(),
                    missing,
                };
            }
            // Every declared arg maps to its positional value, or empty if absent (optional
            // tokens expand to nothing rather than staying literal).
            let named: Vec<(String, String)> = cmd
                .args
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    (
                        a.name.clone(),
                        positional.get(i).map(|v| v.to_string()).unwrap_or_default(),
                    )
                })
                .collect();
            let mut prompt = template::expand(&cmd.body, &positional, &named, &rest);
            // A command that delegates to a skill ("Use the **debugging** skill …", the
            // Claude-Code wrapper pattern) must actually load that skill's methodology, not just
            // name it — otherwise invoking the command does nothing useful.
            let guidance = self.referenced_skill_guidance(&cmd.body);
            // Don't silently drop a typed task when the body has no $ARGUMENTS/$N to receive it.
            if !rest.is_empty() && !template::uses_args(&cmd.body, &named) {
                let trimmed = prompt.trim_end().to_string();
                prompt = if trimmed.is_empty() {
                    rest.clone()
                } else {
                    format!("{trimmed}\n\n{rest}")
                };
            }
            return Resolved::Command {
                cmd: cmd.clone(),
                prompt,
                guidance,
            };
        }

        if let Some(meta) = self.skills.get(&name) {
            return Resolved::Skill {
                meta: meta.clone(),
                prompt: rest,
            };
        }

        Resolved::Unknown(name)
    }

    /// `(name, description)` of every skill in the catalog, sorted by name — for advertising the
    /// `use_skill` tool's available-skills list to the model.
    pub fn skill_listing(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = self
            .skills
            .values()
            .map(|m| (m.name.clone(), m.description.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Load a skill by name and render its full methodology guidance (body + resources), or
    /// `None` if no skill has that name. Used by the `use_skill` virtual tool on both the direct
    /// and CLI-bridge paths.
    pub fn skill_guidance(&self, name: &str) -> Option<String> {
        self.skills.get(name).map(|m| Skill::load(m).guidance())
    }

    /// Methodology guidance for every known skill that `body` references as a markdown-bold token
    /// (`**skill-name**`) — the Claude-Code "Use the **X** skill …" wrapper pattern. Each matched
    /// skill is loaded once (deduped). A command and the skill it delegates to commonly share a
    /// name (`/orchestrate` → `orchestrate` skill); that is the intended case, not recursion —
    /// skill guidance is inert text, never re-dispatched — so same-named delegation IS injected.
    /// Empty when the body references no known skill.
    fn referenced_skill_guidance(&self, body: &str) -> Vec<String> {
        let mut seen = std::collections::BTreeSet::new();
        let mut out = Vec::new();
        for tok in bold_tokens(body) {
            if let Some(meta) = self.skills.get(&tok) {
                if seen.insert(tok) {
                    out.push(Skill::load(meta).guidance());
                }
            }
        }
        out
    }
}

/// Extract the text inside each `**…**` markdown-bold span in `text`. Inner text is trimmed and
/// empty spans are skipped; used to spot `**skill-name**` references in a command body.
fn bold_tokens(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find("**") {
        let after = &rest[open + 2..];
        if let Some(close) = after.find("**") {
            let inner = after[..close].trim();
            if !inner.is_empty() && !inner.contains('\n') {
                out.push(inner.to_string());
            }
            rest = &after[close + 2..];
        } else {
            break;
        }
    }
    out
}

/// Rank `name` against `query`: `Some(score)` (lower = better) or `None`. Prefix matches beat
/// subsequence (fuzzy) matches; an empty query matches everything.
fn match_score(name: &str, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(1000);
    }
    let (name, query) = (name.to_lowercase(), query.to_lowercase());
    if name.starts_with(&query) {
        return Some(query.len() as u32);
    }
    let mut q = query.chars().peekable();
    for c in name.chars() {
        if q.peek() == Some(&c) {
            q.next();
        }
    }
    if q.peek().is_none() {
        Some(500 + name.len() as u32)
    } else {
        None
    }
}

/// Parse `trivial|standard|complex` (case-insensitive) into a [`TaskTier`].
pub fn parse_tier(s: &str) -> Option<TaskTier> {
    match s.trim().to_lowercase().as_str() {
        "trivial" => Some(TaskTier::Trivial),
        "standard" => Some(TaskTier::Standard),
        "complex" => Some(TaskTier::Complex),
        _ => None,
    }
}

fn parse_command(raw: &str, stem: &str, scope: Scope, path: &Path) -> Result<Command, String> {
    let (fm_text, body) = frontmatter::split(raw);
    let fm = frontmatter::parse(fm_text.unwrap_or(""))?;
    let body = body.trim().to_string();
    if body.is_empty() {
        return Err("empty command body".into());
    }
    let name = fm.scalar("name").unwrap_or_else(|| stem.to_string());
    let description = fm
        .scalar("description")
        .unwrap_or_else(|| first_line(&body, 60));
    let args = fm
        .list("args")
        .into_iter()
        .map(|a| {
            let required = !a.ends_with('?');
            ArgSpec {
                name: a.trim_end_matches('?').to_string(),
                required,
            }
        })
        .collect();
    let tier = fm.scalar("tier").and_then(|t| parse_tier(&t));
    let model = fm.scalar("model");
    Ok(Command {
        name,
        description,
        args,
        tier,
        model,
        body,
        scope,
        path: path.to_path_buf(),
    })
}

fn parse_skill_meta(raw: &str, stem: &str, scope: Scope, dir: &Path) -> Result<SkillMeta, String> {
    let (fm_text, body) = frontmatter::split(raw);
    let fm = frontmatter::parse(fm_text.unwrap_or(""))?;
    let name = fm.scalar("name").unwrap_or_else(|| stem.to_string());
    let description = fm
        .scalar("description")
        .unwrap_or_else(|| first_line(body.trim(), 60));
    let tier = fm.scalar("tier").and_then(|t| parse_tier(&t));
    let resources = fm.list("resources");
    Ok(SkillMeta {
        name,
        description,
        tier,
        resources,
        dir: dir.to_path_buf(),
        scope,
    })
}

fn first_line(body: &str, max: usize) -> String {
    let line = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let line = line.trim();
    if line.chars().count() > max {
        line.chars().take(max).collect::<String>() + "…"
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests;
