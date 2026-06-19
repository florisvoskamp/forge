//! Distill a past session transcript into a reusable Forge skill.
//!
//! Pure, sync functions (no I/O except the final write). The CLI wires the store
//! load and provider call around these helpers.

use std::path::{Path, PathBuf};

/// Max characters of transcript content fed to the distillation prompt.
/// Keeps the cheap model call bounded even for very long sessions.
const MAX_TRANSCRIPT_CHARS: usize = 24_000;

/// A single entry from the session transcript, used for distillation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptEntry {
    /// "user", "assistant", or "tool"
    pub role: String,
    /// Message content (may be empty for pure tool-call turns)
    pub content: String,
    /// Tool calls made in this turn (tool name + compact args)
    pub tool_actions: Vec<String>,
}

/// Derive a kebab-case slug from an arbitrary string (typically the first user prompt).
/// Lowercases, strips non-alphanumeric chars (keeping hyphens), collapses runs of
/// hyphens, trims leading/trailing hyphens, and caps at 40 chars.
pub fn derive_slug(text: &str) -> String {
    let raw: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    // Collapse runs of hyphens
    let mut slug = String::with_capacity(raw.len());
    let mut prev_hyphen = false;
    for c in raw.chars() {
        if c == '-' {
            if !prev_hyphen {
                slug.push(c);
            }
            prev_hyphen = true;
        } else {
            slug.push(c);
            prev_hyphen = false;
        }
    }
    let slug = slug.trim_matches('-').to_string();
    if slug.is_empty() {
        return "custom-skill".to_string();
    }
    slug.chars()
        .take(40)
        .collect::<String>()
        .trim_matches('-')
        .to_string()
}

/// Build the prompt sent to the cheap model to synthesize a skill.
///
/// The prompt includes the compact session transcript (user prompts + assistant
/// tool actions) and instructs the model to produce a reusable, generalised
/// SKILL.md body plus a one-line description.
///
/// Kept pure and `#[cfg(test)]`-friendly — no side effects.
pub fn build_distillation_prompt(entries: &[TranscriptEntry]) -> String {
    let transcript = compact_transcript(entries);
    format!(
        r#"You are a Forge skill author. Your task is to distil the following session transcript \
into a reusable Forge skill methodology that generalises the workflow for future use.

## Session transcript (compact)

{transcript}

## Instructions

Write a SKILL.md body (NOT the frontmatter — that is added separately).
The body must be:
1. A concise, reusable, step-by-step methodology (use numbered steps or headed sections).
2. Generalised from the session — not a verbatim replay. Strip project-specific details.
3. Actionable: someone invoking this skill in a DIFFERENT project should be able to follow it.
4. Under 600 words.

Then, on a NEW line after the body, write exactly:
DESCRIPTION: <one sentence, max 15 words, describing what this skill does>

Output ONLY the methodology body + the DESCRIPTION line. No preamble, no code fences."#
    )
}

/// Compact a list of transcript entries to a bounded string.
/// Includes user messages and assistant tool actions; trims if over `MAX_TRANSCRIPT_CHARS`.
fn compact_transcript(entries: &[TranscriptEntry]) -> String {
    let mut out = String::new();
    for entry in entries {
        match entry.role.as_str() {
            "user" if !entry.content.trim().is_empty() => {
                out.push_str(&format!("[USER] {}\n", entry.content.trim()));
            }
            "assistant" => {
                if !entry.content.trim().is_empty() {
                    out.push_str(&format!("[ASSISTANT] {}\n", entry.content.trim()));
                }
                for action in &entry.tool_actions {
                    out.push_str(&format!("[TOOL] {}\n", action));
                }
            }
            "tool" => {
                // Tool results are noisy; skip them for compactness
            }
            _ => {}
        }
    }
    if out.chars().count() > MAX_TRANSCRIPT_CHARS {
        // Truncate at a char boundary and note it
        let truncated: String = out.chars().take(MAX_TRANSCRIPT_CHARS).collect();
        format!("{truncated}\n[... transcript truncated for brevity ...]")
    } else {
        out
    }
}

/// Parse the model's raw output into `(body, description)`.
/// Looks for a `DESCRIPTION: ...` line (last one wins). The body is everything else.
pub fn parse_model_output(raw: &str) -> (String, String) {
    let mut body_lines: Vec<&str> = Vec::new();
    let mut description = String::new();
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("DESCRIPTION:") {
            description = rest.trim().to_string();
        } else {
            body_lines.push(line);
        }
    }
    let body = body_lines.join("\n").trim_end().to_string();
    if description.is_empty() {
        description = "A generalised workflow distilled from a past session.".to_string();
    }
    (body, description)
}

/// Assemble the complete SKILL.md content from name, description, and body.
///
/// Produces the exact frontmatter format the `Catalog` parser expects:
/// `---\nname: <slug>\ndescription: <one line>\n---\n\n<body>\n`
pub fn assemble_skill_md(name: &str, description: &str, body: &str) -> String {
    format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n")
}

/// Write a skill to `<skills_dir>/<slug>/SKILL.md`, creating dirs as needed.
/// Returns the path written. Errors if the skill directory already exists (clobber guard).
pub fn write_skill(skills_dir: &Path, slug: &str, contents: &str) -> std::io::Result<PathBuf> {
    let skill_dir = skills_dir.join(slug);
    if skill_dir.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "skill '{}' already exists at {} — use --name to choose a different name",
                slug,
                skill_dir.display()
            ),
        ));
    }
    std::fs::create_dir_all(&skill_dir)?;
    let path = skill_dir.join("SKILL.md");
    std::fs::write(&path, contents)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    // --- slug derivation ---

    #[test]
    fn slug_lowercases_and_kebab_cases() {
        assert_eq!(derive_slug("Fix the Auth Bug"), "fix-the-auth-bug");
    }

    #[test]
    fn slug_strips_non_alphanumeric() {
        // 'Hello' → 'hello', ',' → '-', ' ' → '-', 'World' → 'world', '!' → '-'
        // collapse consecutive hyphens, trim trailing → "hello-world"
        assert_eq!(derive_slug("Hello, World!"), "hello-world");
        assert_eq!(derive_slug("hello, world!"), "hello-world");
    }

    #[test]
    fn slug_caps_at_40_chars() {
        let long = "a".repeat(100);
        assert_eq!(derive_slug(&long).len(), 40);
    }

    #[test]
    fn slug_empty_input_returns_fallback() {
        assert_eq!(derive_slug(""), "custom-skill");
        assert_eq!(derive_slug("!!!"), "custom-skill");
    }

    #[test]
    fn slug_trims_leading_trailing_hyphens() {
        assert_eq!(derive_slug("  hello world  "), "hello-world");
    }

    // --- prompt builder ---

    #[test]
    fn build_prompt_includes_user_messages() {
        let entries = vec![
            TranscriptEntry {
                role: "user".into(),
                content: "Please refactor the parser".into(),
                tool_actions: vec![],
            },
            TranscriptEntry {
                role: "assistant".into(),
                content: "I'll start by reading the file".into(),
                tool_actions: vec!["read_file src/parser.rs".into()],
            },
        ];
        let prompt = build_distillation_prompt(&entries);
        assert!(
            prompt.contains("[USER] Please refactor the parser"),
            "user msg: {prompt}"
        );
        assert!(
            prompt.contains("[TOOL] read_file src/parser.rs"),
            "tool action: {prompt}"
        );
        assert!(
            prompt.contains("DESCRIPTION:"),
            "instruction present: {prompt}"
        );
    }

    #[test]
    fn build_prompt_excludes_tool_result_role() {
        let entries = vec![TranscriptEntry {
            role: "tool".into(),
            content: "huge tool output".into(),
            tool_actions: vec![],
        }];
        let prompt = build_distillation_prompt(&entries);
        assert!(
            !prompt.contains("huge tool output"),
            "tool results excluded: {prompt}"
        );
    }

    #[test]
    fn build_prompt_truncates_oversize_transcript() {
        let big_content = "x".repeat(30_000);
        let entries = vec![TranscriptEntry {
            role: "user".into(),
            content: big_content,
            tool_actions: vec![],
        }];
        let prompt = build_distillation_prompt(&entries);
        assert!(
            prompt.contains("transcript truncated"),
            "truncation note present: {prompt}"
        );
    }

    #[test]
    fn build_prompt_stable_for_empty_transcript() {
        let prompt = build_distillation_prompt(&[]);
        assert!(prompt.contains("DESCRIPTION:"));
        assert!(prompt.contains("step-by-step methodology"));
    }

    // --- model output parser ---

    #[test]
    fn parse_model_output_splits_description() {
        let raw = "## Step 1\nDo this.\n## Step 2\nDo that.\nDESCRIPTION: Refactor a parser module systematically.";
        let (body, desc) = parse_model_output(raw);
        assert_eq!(desc, "Refactor a parser module systematically.");
        assert!(body.contains("## Step 1"));
        assert!(body.contains("## Step 2"));
        assert!(!body.contains("DESCRIPTION:"));
    }

    #[test]
    fn parse_model_output_uses_fallback_description_when_absent() {
        let raw = "## Step 1\nDo this.";
        let (body, desc) = parse_model_output(raw);
        assert!(!desc.is_empty());
        assert!(body.contains("## Step 1"));
    }

    #[test]
    fn parse_model_output_last_description_wins() {
        let raw = "DESCRIPTION: first\nBody.\nDESCRIPTION: second";
        let (_body, desc) = parse_model_output(raw);
        assert_eq!(desc, "second");
    }

    // --- SKILL.md assembly ---

    #[test]
    fn assemble_skill_md_produces_correct_frontmatter() {
        let md = assemble_skill_md("my-skill", "Does something useful.", "## Step 1\nRun it.");
        assert!(md.starts_with("---\nname: my-skill\n"), "frontmatter: {md}");
        assert!(
            md.contains("description: Does something useful."),
            "desc: {md}"
        );
        assert!(md.contains("---\n\n## Step 1"), "body after fence: {md}");
    }

    #[test]
    fn assemble_skill_md_is_parseable_by_catalog() {
        use crate::{Catalog, Scope, ScopedDir, Sources};
        let tmp =
            std::env::temp_dir().join(format!("forge-skills-assemble-test-{}", std::process::id()));
        let skills_dir = tmp.join("skills");
        fs::create_dir_all(&skills_dir).unwrap();

        let contents = assemble_skill_md("test-skill", "A test skill.", "## Step 1\nDo the thing.");
        let slug = "test-skill";
        let written = write_skill(&skills_dir, slug, &contents).unwrap();
        assert!(written.exists());

        let sources = Sources {
            commands: vec![],
            skills: vec![ScopedDir {
                scope: Scope::User,
                path: skills_dir.clone(),
            }],
        };
        let cat = Catalog::load(&sources);
        let meta = cat.skill("test-skill").expect("skill discoverable");
        assert_eq!(meta.name, "test-skill");
        assert_eq!(meta.description, "A test skill.");

        let skill = crate::Skill::load(meta);
        assert!(skill.body.contains("## Step 1"));

        fs::remove_dir_all(&tmp).unwrap();
    }

    // --- write_skill clobber guard ---

    #[test]
    fn write_skill_refuses_to_clobber_existing() {
        let tmp =
            std::env::temp_dir().join(format!("forge-skills-clobber-test-{}", std::process::id()));
        let skills_dir = tmp.join("skills");
        fs::create_dir_all(skills_dir.join("my-skill")).unwrap();

        let result = write_skill(&skills_dir, "my-skill", "content");
        assert!(result.is_err());
        assert!(result.unwrap_err().kind() == std::io::ErrorKind::AlreadyExists);

        fs::remove_dir_all(&tmp).unwrap();
    }
}
