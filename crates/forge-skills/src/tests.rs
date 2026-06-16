use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A throwaway directory tree with command/skill scope dirs, removed on drop.
struct Tmp {
    root: PathBuf,
}

impl Tmp {
    fn new() -> Tmp {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root = std::env::temp_dir().join(format!("forge-skills-{}-{n}", std::process::id()));
        std::fs::create_dir_all(root.join("user/commands")).unwrap();
        std::fs::create_dir_all(root.join("user/skills")).unwrap();
        std::fs::create_dir_all(root.join("project/commands")).unwrap();
        std::fs::create_dir_all(root.join("project/skills")).unwrap();
        Tmp { root }
    }

    fn cmd(&self, scope: &str, name: &str, contents: &str) {
        std::fs::write(
            self.root
                .join(scope)
                .join("commands")
                .join(format!("{name}.md")),
            contents,
        )
        .unwrap();
    }

    fn skill(&self, scope: &str, name: &str, skill_md: &str) {
        let dir = self.root.join(scope).join("skills").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), skill_md).unwrap();
    }

    fn skill_resource(&self, scope: &str, name: &str, res: &str, contents: &str) {
        let dir = self.root.join(scope).join("skills").join(name);
        std::fs::write(dir.join(res), contents).unwrap();
    }

    fn sources(&self) -> Sources {
        Sources {
            // Order doesn't decide precedence — Scope does.
            commands: vec![
                ScopedDir {
                    scope: Scope::User,
                    path: self.root.join("user/commands"),
                },
                ScopedDir {
                    scope: Scope::Project,
                    path: self.root.join("project/commands"),
                },
            ],
            skills: vec![
                ScopedDir {
                    scope: Scope::User,
                    path: self.root.join("user/skills"),
                },
                ScopedDir {
                    scope: Scope::Project,
                    path: self.root.join("project/skills"),
                },
            ],
        }
    }

    fn load(&self) -> Catalog {
        Catalog::load(&self.sources())
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn project_command_shadows_user_command_of_same_name() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "review",
        "---\ndescription: user review\n---\nUser body",
    );
    t.cmd(
        "project",
        "review",
        "---\ndescription: project review\n---\nProject body",
    );
    let cat = t.load();
    let cmd = cat.command("review").unwrap();
    assert_eq!(cmd.scope, Scope::Project);
    assert_eq!(cmd.description, "project review");
    let entry = cat
        .entries()
        .into_iter()
        .find(|e| e.name == "review")
        .unwrap();
    assert!(
        entry.shadows,
        "/help marks it as shadowing the user command"
    );
}

#[test]
fn lists_a_command_with_its_description_and_scope() {
    let t = Tmp::new();
    t.cmd(
        "project",
        "ship",
        "---\ndescription: stage, commit, push\n---\ngit ...",
    );
    let cat = t.load();
    let e = cat
        .entries()
        .into_iter()
        .find(|e| e.name == "ship")
        .unwrap();
    assert_eq!(e.description, "stage, commit, push");
    assert_eq!(e.scope, Scope::Project);
    assert!(!e.is_skill);
}

#[test]
fn expands_positional_and_arguments_tokens() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "fix",
        "---\ndescription: fix\nargs: [target]\n---\nFix the bug in $1 described as: $ARGUMENTS",
    );
    let cat = t.load();
    match cat.resolve("/fix auth.rs token expiry") {
        Resolved::Command { prompt, .. } => {
            assert_eq!(
                prompt,
                "Fix the bug in auth.rs described as: auth.rs token expiry"
            );
        }
        other => panic!("expected Command, got {other:?}"),
    }
}

#[test]
fn named_arg_substitution() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "greet",
        "---\ndescription: g\nargs: [who]\n---\nHello $who, from $1",
    );
    let cat = t.load();
    match cat.resolve("/greet world") {
        Resolved::Command { prompt, .. } => assert_eq!(prompt, "Hello world, from world"),
        other => panic!("got {other:?}"),
    }
}

#[test]
fn missing_required_arg_short_circuits_with_no_model_call() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "fix",
        "---\ndescription: fix\nargs: [path]\n---\nFix $path",
    );
    let cat = t.load();
    match cat.resolve("/fix") {
        Resolved::MissingArgs { name, missing } => {
            assert_eq!(name, "fix");
            assert_eq!(missing, vec!["path".to_string()]);
        }
        other => panic!("expected MissingArgs, got {other:?}"),
    }
}

#[test]
fn optional_arg_marked_with_question_mark_is_not_required() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "review",
        "---\ndescription: r\nargs: [target, scope?]\n---\n$target $scope",
    );
    let cat = t.load();
    match cat.resolve("/review file.rs") {
        Resolved::Command { prompt, cmd, .. } => {
            assert_eq!(prompt.trim(), "file.rs");
            assert_eq!(cmd.arg_hint(), "<target> [scope]");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn unknown_command_resolves_to_unknown() {
    let t = Tmp::new();
    let cat = t.load();
    assert_eq!(
        cat.resolve("/doesnotexist hello"),
        Resolved::Unknown("doesnotexist".to_string())
    );
}

#[test]
fn plain_text_and_escaped_slash_pass_through() {
    let t = Tmp::new();
    let cat = t.load();
    assert_eq!(
        cat.resolve("hello world"),
        Resolved::Plain("hello world".into())
    );
    assert_eq!(cat.resolve("//literal"), Resolved::Plain("literal".into()));
}

#[test]
fn skill_resolves_via_bare_name_and_skill_prefix() {
    let t = Tmp::new();
    t.skill(
        "user",
        "honest-review",
        "---\ndescription: skeptical audit\ntier: complex\n---\nBody",
    );
    let cat = t.load();
    match cat.resolve("/skill honest-review check this") {
        Resolved::Skill { meta, prompt } => {
            assert_eq!(meta.name, "honest-review");
            assert_eq!(meta.tier, Some(TaskTier::Complex));
            assert_eq!(prompt, "check this");
        }
        other => panic!("got {other:?}"),
    }
    match cat.resolve("/honest-review") {
        Resolved::Skill { meta, prompt } => {
            assert_eq!(meta.name, "honest-review");
            assert_eq!(prompt, "");
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn skill_body_and_resources_load_only_on_invoke() {
    let t = Tmp::new();
    t.skill(
        "user",
        "auditor",
        "---\ndescription: audit\nresources: [checklist.md, missing.md]\n---\nMethodology body here.",
    );
    t.skill_resource("user", "auditor", "checklist.md", "1. check this");
    let cat = t.load();
    let meta = cat.skill("auditor").unwrap().clone();
    // Progressive disclosure: the discovered meta carries no body.
    assert_eq!(meta.description, "audit");

    let skill = Skill::load(&meta);
    assert_eq!(skill.body, "Methodology body here.");
    assert_eq!(
        skill.resources,
        vec![("checklist.md".to_string(), "1. check this".to_string())]
    );
    assert!(
        skill.warnings.iter().any(|w| w.contains("missing.md")),
        "missing resource is warned, not fatal: {:?}",
        skill.warnings
    );
    assert!(skill.guidance().contains("Methodology body here."));
    assert!(skill.guidance().contains("1. check this"));
}

#[test]
fn malformed_frontmatter_file_is_skipped_and_others_still_load() {
    let t = Tmp::new();
    t.cmd("user", "good", "---\ndescription: fine\n---\nbody");
    t.cmd(
        "user",
        "broken",
        "---\nthis is not valid frontmatter at all\n---\nbody",
    );
    let cat = t.load();
    assert!(cat.command("good").is_some(), "valid command still loads");
    assert!(cat.command("broken").is_none(), "broken command skipped");
    assert!(
        cat.warnings().iter().any(|w| w.contains("broken")),
        "a single warning is collected: {:?}",
        cat.warnings()
    );
}

#[test]
fn empty_body_command_is_rejected() {
    let t = Tmp::new();
    t.cmd("user", "hollow", "---\ndescription: nothing\n---\n");
    let cat = t.load();
    assert!(cat.command("hollow").is_none());
    assert!(cat.warnings().iter().any(|w| w.contains("hollow")));
}

#[test]
fn fuzzy_prefix_beats_subsequence_and_caps_at_limit() {
    let t = Tmp::new();
    t.cmd("user", "review", "---\ndescription: r\n---\nb");
    t.cmd("user", "rearchitect", "---\ndescription: r\n---\nb");
    t.cmd("user", "clear", "---\ndescription: c\n---\nb");
    let cat = t.load();
    let m = cat.fuzzy("re", 10);
    // Both review + rearchitect are prefix matches (clear is not); equal score → alpha tiebreak.
    assert_eq!(m[0].name, "rearchitect");
    assert!(m.iter().any(|e| e.name == "review"));
    assert!(m.iter().all(|e| e.name != "clear"));
    assert_eq!(cat.fuzzy("", 2).len(), 2, "limit respected");
}

#[test]
fn description_defaults_to_first_body_line_when_absent() {
    let t = Tmp::new();
    t.cmd(
        "user",
        "nodesc",
        "---\nname: nodesc\n---\nDo the important thing now.",
    );
    let cat = t.load();
    assert_eq!(
        cat.command("nodesc").unwrap().description,
        "Do the important thing now."
    );
}

#[test]
fn claude_code_command_with_unknown_keys_parses_leniently() {
    // A real CC command: extra keys (allowed-tools), $ARGUMENTS, no Forge-specific fields.
    let t = Tmp::new();
    t.cmd(
        "user",
        "commit",
        "---\ndescription: make a commit\nallowed-tools: [Bash]\nmodel: claude-opus-4-8\n---\nCommit staged changes: $ARGUMENTS",
    );
    let cat = t.load();
    let cmd = cat.command("commit").unwrap();
    assert_eq!(cmd.description, "make a commit");
    assert_eq!(cmd.model.as_deref(), Some("claude-opus-4-8"));
    match cat.resolve("/commit fix the parser") {
        Resolved::Command { prompt, .. } => {
            assert_eq!(prompt, "Commit staged changes: fix the parser")
        }
        other => panic!("got {other:?}"),
    }
}

#[test]
fn block_style_list_frontmatter_parses() {
    let t = Tmp::new();
    t.skill(
        "user",
        "blocky",
        "---\ndescription: d\nresources:\n  - a.md\n  - b.md\n---\nbody",
    );
    let cat = t.load();
    assert_eq!(cat.skill("blocky").unwrap().resources, vec!["a.md", "b.md"]);
}

#[test]
fn command_wins_namespace_over_same_named_skill() {
    let t = Tmp::new();
    t.cmd("user", "audit", "---\ndescription: cmd audit\n---\nbody");
    t.skill("user", "audit", "---\ndescription: skill audit\n---\nbody");
    let cat = t.load();
    // bare /audit → the command
    match cat.resolve("/audit") {
        Resolved::Command { .. } => {}
        other => panic!("command should win bare name, got {other:?}"),
    }
    // skill still reachable explicitly
    match cat.resolve("/skill audit") {
        Resolved::Skill { .. } => {}
        other => panic!("skill reachable via /skill, got {other:?}"),
    }
    // listing shows the command, not a duplicate skill entry
    assert_eq!(
        cat.entries().iter().filter(|e| e.name == "audit").count(),
        1
    );
}
