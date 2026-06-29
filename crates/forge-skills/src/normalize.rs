//! Skill content normalizer: rewrites imported skill/command markdown so filesystem paths and
//! CLI binary references point to Forge rather than Claude Code or Codex.
//!
//! This fixes **path** and **command** references in code contexts. It does NOT do a blanket
//! brand-rename — that is `forge_native`, which runs at display time against text shown to the
//! model. This function normalizes files on disk so a skill that says "put this in
//! `~/.claude/skills/`" says "put this in `~/.config/forge/skills/`" instead.

/// Ordered path substitutions applied to ALL content (most-specific first so no partial matches).
const PATH_SUBS: &[(&str, &str)] = &[
    ("~/.claude/skills/", "~/.config/forge/skills/"),
    ("~/.claude/commands/", "~/.config/forge/commands/"),
    ("~/.claude/", "~/.config/forge/"),
    (".claude/skills/", ".forge/skills/"),
    (".claude/commands/", ".forge/commands/"),
    (".claude/", ".forge/"),
];

/// CLI binary names replaced with `forge` when appearing as commands in code contexts.
const BINARIES: &[&str] = &["claude", "codex"];

/// Normalize skill/command markdown content: replace Claude/Codex filesystem paths with their
/// Forge equivalents, and replace `claude`/`codex` binary names in code contexts (inline
/// backtick spans, fenced code blocks, `$ command` prefixes) with `forge`.
///
/// Prose mentions of "Claude" or "Codex" — e.g. "Built for Claude Code" — are left untouched
/// because they are not command references. Display-time rebranding is handled separately by
/// [`super::forge_native`].
///
/// Safe to call multiple times; returns the input unchanged when nothing matches.
pub fn normalize_skill_content(content: &str) -> String {
    if content.is_empty() {
        return String::new();
    }

    // Phase 1: path substitutions everywhere (most-specific first)
    let mut s = content.to_string();
    for &(from, to) in PATH_SUBS {
        if s.contains(from) {
            s = s.replace(from, to);
        }
    }

    // Phase 2: inline code-context binary replacements (backtick spans, shell-prompt prefixes)
    s = replace_inline_binaries(s);

    // Phase 3: binary replacement inside fenced code blocks (word-boundary match)
    replace_in_fenced_blocks(&s)
}

/// Replace binary names in inline code contexts: backtick spans and shell-prompt prefixes.
/// Applied to ALL content because inline backtick spans appear in any line.
fn replace_inline_binaries(mut s: String) -> String {
    for &binary in BINARIES {
        // `<binary> ...`  →  `forge ...`  (backtick then binary then space)
        let pat = format!("`{binary} ");
        if s.contains(&pat) {
            s = s.replace(&pat, "`forge ");
        }
        // `<binary>`  →  `forge`  (backtick-wrapped standalone)
        let pat = format!("`{binary}`");
        if s.contains(&pat) {
            s = s.replace(&pat, "`forge`");
        }
        // $ <binary> ...  →  $ forge ...  (shell prompt)
        let pat = format!("$ {binary} ");
        if s.contains(&pat) {
            s = s.replace(&pat, "$ forge ");
        }
        // verb-prefixed: "run <binary> ", "via <binary> ", "using <binary> "
        for prefix in ["run ", "via ", "using "] {
            let pat = format!("{prefix}{binary} ");
            if s.contains(&pat) {
                let rep = format!("{prefix}forge ");
                s = s.replace(&pat, &rep);
            }
        }
    }
    s
}

/// Walk the text line by line; inside triple-backtick or triple-tilde fenced code blocks,
/// replace binary names at word boundaries with `forge`.
fn replace_in_fenced_blocks(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut in_fence = false;
    let mut fence_char = b'`';
    let trailing_newline = text.ends_with('\n');

    for line in text.lines() {
        let trimmed = line.trim_start();
        let is_backtick_fence = trimmed.starts_with("```");
        let is_tilde_fence = trimmed.starts_with("~~~");

        if is_backtick_fence || is_tilde_fence {
            let this_fence = if is_backtick_fence { b'`' } else { b'~' };
            if !in_fence {
                in_fence = true;
                fence_char = this_fence;
                result.push_str(line);
            } else if this_fence == fence_char {
                in_fence = false;
                result.push_str(line);
            } else {
                // Different fence char inside an open fence — treat as code content
                result.push_str(&replace_at_word_boundaries(line));
            }
        } else if in_fence {
            result.push_str(&replace_at_word_boundaries(line));
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }

    if !trailing_newline && result.ends_with('\n') {
        result.pop();
    }
    result
}

/// Replace each binary name in `line` where it appears as a standalone word (not part of a
/// longer identifier like `claude_code` or `aiclaude`).
fn replace_at_word_boundaries(line: &str) -> String {
    let mut s = line.to_string();
    for &binary in BINARIES {
        s = replace_word(&s, binary, "forge");
    }
    s
}

/// Replace `from` with `to` whenever `from` appears surrounded by non-identifier characters.
fn replace_word(text: &str, from: &str, to: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;

    while let Some(idx) = remaining.find(from) {
        let after = idx + from.len();
        let before_ok = idx == 0 || {
            let b = remaining.as_bytes()[idx - 1];
            !b.is_ascii_alphanumeric() && b != b'_' && b != b'-'
        };
        let after_ok = after >= remaining.len() || {
            let b = remaining.as_bytes()[after];
            !b.is_ascii_alphanumeric() && b != b'_' && b != b'-'
        };

        if before_ok && after_ok {
            result.push_str(&remaining[..idx]);
            result.push_str(to);
            remaining = &remaining[after..];
        } else {
            // Not a word-boundary match; advance past this position to avoid re-scanning it.
            // Safe: idx is the start of an ASCII pattern so remaining[idx] is a valid ASCII byte.
            result.push_str(&remaining[..idx + 1]);
            remaining = &remaining[idx + 1..];
        }
    }
    result.push_str(remaining);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_in_code_block_replaced() {
        // Path inside a fenced code block must be rewritten.
        let input =
            "Install the skill:\n\n```bash\ncp SKILL.md ~/.claude/skills/myskill/SKILL.md\n```\n";
        let out = normalize_skill_content(input);
        assert!(
            !out.contains("~/.claude/skills/"),
            "old path still present: {out}"
        );
        assert!(
            out.contains("~/.config/forge/skills/"),
            "new path not inserted: {out}"
        );
    }

    #[test]
    fn binary_in_code_block_replaced() {
        // Inline backtick span: `claude skills create` → `forge skills create`
        let input = "Run `claude skills create` to make a skill.";
        let out = normalize_skill_content(input);
        assert_eq!(out, "Run `forge skills create` to make a skill.");
    }

    #[test]
    fn prose_claude_not_replaced() {
        // Plain prose — no code context — must be left untouched.
        let input = "Built for Claude Code by Anthropic.";
        let out = normalize_skill_content(input);
        assert_eq!(out, input, "prose should be unchanged: {out:?}");
    }

    #[test]
    fn codex_binary_replaced() {
        // Inline backtick span: `codex edit` → `forge edit`
        let input = "Use `codex edit` to apply changes.";
        let out = normalize_skill_content(input);
        assert_eq!(out, "Use `forge edit` to apply changes.");
    }

    #[test]
    fn all_path_subs_applied() {
        let input = concat!(
            "Skills live in ~/.claude/skills/.\n",
            "Commands live in ~/.claude/commands/.\n",
            "Config is at ~/.claude/.\n",
            "Project skills: .claude/skills/.\n",
            "Project commands: .claude/commands/.\n",
            "Project config: .claude/.\n",
        );
        let out = normalize_skill_content(input);
        assert!(out.contains("~/.config/forge/skills/"));
        assert!(out.contains("~/.config/forge/commands/"));
        assert!(!out.contains("~/.claude/"));
        assert!(out.contains(".forge/skills/"));
        assert!(out.contains(".forge/commands/"));
        assert!(!out.contains(".claude/"));
    }

    #[test]
    fn fenced_block_binary_replaced_at_word_boundary() {
        let input = "```sh\nclaude chat --model opus\n```\n";
        let out = normalize_skill_content(input);
        assert!(out.contains("forge chat"), "binary not replaced: {out}");
        assert!(
            !out.contains("claude chat"),
            "old binary still present: {out}"
        );
    }

    #[test]
    fn shell_prompt_replaced() {
        let input = "Run it with `$ claude run my-task`";
        let out = normalize_skill_content(input);
        assert!(
            out.contains("$ forge run"),
            "shell prompt not replaced: {out}"
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(normalize_skill_content(""), "");
    }

    #[test]
    fn idempotent_on_already_normalized_content() {
        let input = "Use `forge skills create` or ~/.config/forge/skills/.";
        let first = normalize_skill_content(input);
        let second = normalize_skill_content(&first);
        assert_eq!(first, second, "normalize is not idempotent");
    }
}
