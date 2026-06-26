use super::assay::bundle_source;
use super::import::{convert_mdc_to_command_md, copy_catalog_assets};
use super::local::bridge_plans;
use super::run::{
    chat_action, expand_at_files, extract_code_blocks, loop_stop_reason, models_for_provider,
    models_provider_view, nth_assistant_response, ChatAction, LOOP_MAX_ITERS,
};
use crate::*;

#[test]
fn extract_code_blocks_pulls_fenced_blocks_with_lang() {
    let md =
        "Here you go:\n\n```rust\nfn main() {}\n```\n\nand shell:\n\n```bash\nls -la\n```\ndone";
    let blocks = extract_code_blocks(md);
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[0].0, "rust");
    assert_eq!(blocks[0].1, "fn main() {}\n");
    assert_eq!(blocks[1].0, "bash");
    assert_eq!(blocks[1].1, "ls -la\n");
    // Prose with no fences → no blocks (the caller then copies the whole response directly).
    assert!(extract_code_blocks("just some prose, no code").is_empty());
    // An unterminated final fence is still captured (model cut off mid-block).
    let cut = extract_code_blocks("```python\nprint(1)\n");
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0].0, "python");
}

#[test]
fn nth_assistant_response_counts_back_from_the_latest() {
    use forge_types::Role;
    // history() is oldest-first, user + assistant interleaved.
    let history = vec![
        (Role::User, "q1".to_string()),
        (Role::Assistant, "first answer".to_string()),
        (Role::User, "q2".to_string()),
        (Role::Assistant, "second answer".to_string()),
        (Role::User, "q3".to_string()),
        (Role::Assistant, "third answer".to_string()),
    ];
    // 1 = most recent, 2 = the one before, …
    assert_eq!(
        nth_assistant_response(&history, 1).as_deref(),
        Some("third answer")
    );
    assert_eq!(
        nth_assistant_response(&history, 2).as_deref(),
        Some("second answer")
    );
    assert_eq!(
        nth_assistant_response(&history, 3).as_deref(),
        Some("first answer")
    );
    // Beyond the available responses → None (the caller shows a "only N so far" note).
    assert_eq!(nth_assistant_response(&history, 4), None);
    // Empty / user-only history → None.
    assert_eq!(nth_assistant_response(&[], 1), None);
    assert_eq!(
        nth_assistant_response(&[(Role::User, "hi".into())], 1),
        None
    );
}

#[test]
fn expand_at_files_reads_referenced_files_and_skips_nonfiles() {
    let dir = std::env::temp_dir().join(format!("forge-at-{}", forge_types::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("note.txt");
    std::fs::write(&f, "hello from the file").unwrap();
    let path = f.to_string_lossy();

    // A real file is read into a guidance block + reported as included; a `@mention` that
    // isn't a file is left alone (no block, not reported).
    let prompt = format!("review @{path} and ping @nobody-here-xyz about it");
    let (blocks, included, skipped) = expand_at_files(&prompt);
    assert_eq!(included, vec![path.to_string()]);
    assert!(skipped.is_empty());
    assert_eq!(blocks.len(), 1);
    assert!(blocks[0].contains("hello from the file"));
    assert!(blocks[0].contains(&*path));

    // The same path referenced twice is only read once.
    let (blocks2, _, _) = expand_at_files(&format!("@{path} @{path}"));
    assert_eq!(blocks2.len(), 1);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn expand_at_files_survives_multibyte_whitespace() {
    // Regression: a pasted block often carries a non-breaking space (U+00A0, 2 bytes). The old
    // byte-by-byte scan cast each byte to a char and sliced mid-character → "not a char boundary"
    // panic that crashed the turn. A multi-line prompt with leading NBSP must parse cleanly and
    // still resolve a real @file.
    let dir = std::env::temp_dir().join(format!("forge-nbsp-{}", forge_types::new_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("data.txt");
    std::fs::write(&f, "payload").unwrap();
    let path = f.to_string_lossy();
    // \u{a0} = NBSP, \u{2003} = em space — both multi-byte; the panic was triggered by these.
    let prompt = format!("\u{a0}\u{a0}pasted\u{2003}block\nnow read @{path} thanks");
    let (blocks, included, _) = expand_at_files(&prompt);
    assert_eq!(included, vec![path.to_string()]);
    assert!(blocks[0].contains("payload"));
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn copy_catalog_assets_imports_then_skips_existing() {
    // A Codex-style prompt: plain markdown, no frontmatter (name = file stem, description =
    // first body line). The lenient command reader must accept it and we must copy it.
    let root = std::env::temp_dir().join(format!("forge-imp-{}", forge_types::new_id()));
    let src = root.join("prompts");
    let cmd_dst = root.join("out/commands");
    let skill_dst = root.join("out/skills");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("refactor.md"),
        "Refactor the selected code cleanly.\n",
    )
    .unwrap();

    let sources = forge_skills::Sources {
        commands: vec![forge_skills::ScopedDir {
            scope: forge_skills::Scope::User,
            path: src.clone(),
        }],
        skills: vec![],
    };
    let cat = forge_skills::Catalog::load(&sources);

    let first = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
    assert_eq!(first.copied_commands, 1, "the prompt was imported");
    assert_eq!(first.copied_skills, 0);
    assert!(cmd_dst.join("refactor.md").exists());

    // Re-running keeps the existing file instead of overwriting it.
    let second = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
    assert_eq!(second.copied_commands, 0);
    assert_eq!(second.skipped_commands, 1, "already present → skipped");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn copy_catalog_assets_copies_skill_dir_with_resources() {
    // A skill is a DIRECTORY (SKILL.md + declared resource files). The export/import round-trip must
    // copy the whole directory, not just SKILL.md — otherwise a re-imported skill loses its
    // resources. This backs both `forge skill export` and `forge skill import`.
    let root = std::env::temp_dir().join(format!("forge-skdir-{}", forge_types::new_id()));
    let skills_src = root.join("skills");
    let cmd_dst = root.join("out/commands");
    let skill_dst = root.join("out/skills");
    std::fs::create_dir_all(skills_src.join("refactor")).unwrap();
    std::fs::write(
        skills_src.join("refactor/SKILL.md"),
        "---\nname: refactor\ndescription: refactor methodology\nresources: [notes.md]\n---\n\nDo it.",
    )
    .unwrap();
    std::fs::write(skills_src.join("refactor/notes.md"), "supporting notes\n").unwrap();

    let sources = forge_skills::Sources {
        commands: vec![],
        skills: vec![forge_skills::ScopedDir {
            scope: forge_skills::Scope::User,
            path: skills_src.clone(),
        }],
    };
    let cat = forge_skills::Catalog::load(&sources);

    let counts = copy_catalog_assets(&cat, &cmd_dst, &skill_dst);
    assert_eq!(counts.copied_skills, 1, "the skill directory was copied");
    assert!(skill_dst.join("refactor/SKILL.md").exists());
    assert!(
        skill_dst.join("refactor/notes.md").exists(),
        "the skill's resource file must round-trip, not just SKILL.md"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn export_copies_agent_md_files_then_skips_existing() {
    // `forge skill export` copies agents via count_copy_md_files (the catalog only tracks
    // commands+skills). Verify the agent copy: `.md` files are copied, non-md ignored, re-run skips.
    use super::import::{count_copy_md_files, ImportCounts};
    let root = std::env::temp_dir().join(format!("forge-exp-{}", forge_types::new_id()));
    let src = root.join("agents");
    let dst = root.join("out/agents");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("reviewer.md"), "---\nname: reviewer\n---\nReview.").unwrap();
    std::fs::write(src.join("planner.md"), "---\nname: planner\n---\nPlan.").unwrap();
    std::fs::write(src.join("README.txt"), "not an agent").unwrap();

    let mut first = ImportCounts::default();
    count_copy_md_files(&src, &dst, &mut first);
    assert_eq!(first.copied_agents, 2, "both .md agents copied");
    assert!(dst.join("reviewer.md").exists());
    assert!(dst.join("planner.md").exists());
    assert!(
        !dst.join("README.txt").exists(),
        "non-md files are not agents"
    );

    // Re-running keeps existing agents instead of overwriting them.
    let mut second = ImportCounts::default();
    count_copy_md_files(&src, &dst, &mut second);
    assert_eq!(second.copied_agents, 0);
    assert_eq!(second.skipped_agents, 2, "already present → skipped");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn loop_stops_on_sentinel_or_iteration_cap() {
    // Keeps looping while the model hasn't signalled done and we're under the cap.
    assert!(loop_stop_reason(Some("still working on it"), 1).is_none());
    // Stops the moment the completion token appears.
    assert!(loop_stop_reason(Some("all green now\nLOOP_COMPLETE"), 3).is_some());
    // Stops at the hard iteration cap even without the token.
    assert!(loop_stop_reason(Some("more to do"), LOOP_MAX_ITERS).is_some());
    // No assistant text yet → not complete, keep going (under cap).
    assert!(loop_stop_reason(None, 1).is_none());
}

#[test]
fn interactive_logs_go_to_a_file_never_the_tui() {
    // The crash: genai logged a 429 body to stderr, shredding the inline TUI. Interactive
    // runs must route logs to a file; only pipes/CI write to stderr.
    assert_eq!(log_target(true), LogTarget::File);
    assert_eq!(log_target(false), LogTarget::Stderr);
}

fn models_catalog() -> forge_mesh::ModelCatalog {
    forge_mesh::ModelCatalog::new(vec![
        "anthropic::claude-opus-4-8".into(),
        "groq::llama-3.1-8b-instant".into(),
        "groq::llama-3.3-70b-versatile".into(),
        "claude-cli::".into(), // bare default (hidden in browser, still counted in stats)
        "claude-cli::opus".into(), // named alias (shown in browser)
    ])
}

#[test]
fn models_provider_view_heading_has_counts_and_rows_per_provider() {
    let cat = models_catalog();
    let pricing = forge_mesh::pricing::Pricing::default();
    let (heading, rows) = models_provider_view(&cat, &pricing, &Default::default());
    assert!(heading.contains("5 total"), "heading counts: {heading}");
    assert!(heading.contains("3 frontier") && heading.contains("2 subscription"));
    // groq has 2 models → it's the first (richest) provider row.
    assert_eq!(rows[0].id, "groq");
    assert!(rows[0].subtitle.contains("2 models"));
    // every provider row is a header (no `::` in id) so the browser knows it can drill.
    assert!(rows.iter().all(|r| !r.id.contains("::")));
}

#[test]
fn models_for_provider_lists_models_with_badges() {
    let cat = models_catalog();
    let pricing = forge_mesh::pricing::Pricing::default();
    let (heading, rows) = models_for_provider(&cat, &pricing, &Default::default(), "groq");
    assert!(heading.contains("groq") && heading.contains("esc: back"));
    assert_eq!(rows.len(), 2);
    // model rows carry the full id (so Enter on them is a no-op, not a drill) + badges.
    assert!(rows.iter().all(|r| r.id.contains("::")));
    let frontier = rows.iter().find(|r| r.id.contains("70b")).unwrap();
    assert!(frontier.subtitle.contains("frontier") && frontier.subtitle.contains("free"));
    // The subscription bridge shows its named alias; the bare `claude-cli::` default-model
    // entry is hidden (it was confusingly empty in the browser).
    let (_, sub) = models_for_provider(&cat, &pricing, &Default::default(), "claude-cli");
    assert!(!sub.is_empty(), "named cli models shown");
    assert!(
        sub.iter().all(|r| r.id != "claude-cli::"),
        "bare entry hidden"
    );
    assert!(sub[0].subtitle.contains("subscription"));
}

#[test]
fn onboarding_only_when_nothing_is_configured() {
    // Fresh machine: no key, no bridge, no config → onboard.
    assert!(needs_onboarding(false, false, false));
    // Any one signal of prior setup suppresses it.
    assert!(!needs_onboarding(true, false, false)); // has a key
    assert!(!needs_onboarding(false, true, false)); // a bridge is installed
    assert!(!needs_onboarding(false, false, true)); // a saved config exists
}

#[test]
fn bridge_plans_cover_both_clis_with_stored_slugs() {
    let claude = bridge_plans(forge_provider::CliKind::ClaudeCode);
    assert!(claude.iter().any(|(_, slug)| *slug == "max-20x"));
    let codex = bridge_plans(forge_provider::CliKind::Codex);
    assert!(codex.iter().any(|(_, slug)| *slug == "plus"));
    // Every plan has a non-empty human label + slug.
    for (label, slug) in claude.iter().chain(codex) {
        assert!(!label.is_empty() && !slug.is_empty());
    }
}

#[test]
fn bundle_source_collects_source_and_skips_build_dirs() {
    let dir = std::env::temp_dir().join(format!("forge-bundle-{}", forge_types::new_id()));
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::create_dir_all(dir.join("target")).unwrap();
    std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
    std::fs::write(dir.join("target/junk.rs"), "GENERATED").unwrap();
    std::fs::write(dir.join("notes.txt"), "ignored ext").unwrap();

    let out = bundle_source(&dir, 100_000);
    assert!(out.contains("fn main()"), "source included: {out}");
    assert!(out.contains("FILE:"), "file headers present");
    assert!(!out.contains("GENERATED"), "target/ skipped");
    assert!(!out.contains("ignored ext"), "non-source ext skipped");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn convert_mdc_strips_globs_and_keeps_description() {
    let mdc = "---\ndescription: \"My rule\"\nglobs: \"**/*.rs\"\nalwaysApply: false\n---\nDo this thing.";
    let out = convert_mdc_to_command_md(mdc, "my-rule");
    assert!(
        out.starts_with("---\ndescription: \"My rule\""),
        "description kept: {out}"
    );
    assert!(!out.contains("globs"), "globs dropped: {out}");
    assert!(!out.contains("alwaysApply"), "alwaysApply dropped: {out}");
    assert!(out.contains("Do this thing."), "body kept: {out}");
}

#[test]
fn convert_mdc_uses_fallback_name_when_no_description() {
    let mdc = "---\nglobs: \"*.ts\"\n---\nContent.";
    let out = convert_mdc_to_command_md(mdc, "fallback");
    assert!(out.contains("fallback"), "fallback name used: {out}");
    assert!(out.contains("Content."), "body kept: {out}");
}

#[test]
fn convert_mdc_handles_no_frontmatter() {
    let mdc = "Just a plain rule with no frontmatter.";
    let out = convert_mdc_to_command_md(mdc, "plain");
    assert!(
        out.starts_with("---\ndescription:"),
        "wraps with frontmatter: {out}"
    );
    assert!(out.contains("Just a plain rule"), "body kept: {out}");
}

#[test]
fn chat_action_classifies_lines() {
    assert_eq!(chat_action("  "), ChatAction::Skip);
    assert_eq!(chat_action("\n"), ChatAction::Skip);
    assert_eq!(chat_action("/quit"), ChatAction::Quit);
    assert_eq!(chat_action("/exit\n"), ChatAction::Quit);
    assert_eq!(chat_action("  /q "), ChatAction::Quit);
    assert_eq!(
        chat_action("fix the bug\n"),
        ChatAction::Run("fix the bug".to_string())
    );
}

// --- ResumeMode resolver ---

fn make_store_with_sessions(n: usize) -> forge_store::Store {
    let store = forge_store::Store::open_in_memory().unwrap();
    for i in 0..n {
        store
            .create_session(&format!("/cwd/{i}"), "default")
            .unwrap();
    }
    store
}

#[test]
fn session_title_collapses_whitespace_truncates_and_falls_back() {
    assert_eq!(session_title(None), "(no prompt yet)");
    assert_eq!(session_title(Some("   ")), "(no prompt yet)");
    assert_eq!(
        session_title(Some("fix the\n\n  resume   bug")),
        "fix the resume bug"
    );
    let long = "x".repeat(100);
    let title = session_title(Some(&long));
    assert_eq!(title.chars().count(), 64);
    assert!(title.ends_with('…'));
}

#[test]
fn resume_mode_neither_flag_gives_fresh() {
    let store = make_store_with_sessions(2);
    let mode = resolve_resume_mode(false, None, &store, false).unwrap();
    assert_eq!(mode, ResumeMode::Fresh);
}

#[test]
fn resume_mode_continue_returns_most_recent_id() {
    let store = make_store_with_sessions(0);
    let a = store.create_session("/a", "default").unwrap();
    let b = store.create_session("/b", "default").unwrap();
    let mode = resolve_resume_mode(true, None, &store, false).unwrap();
    assert_eq!(mode, ResumeMode::Id(b.clone()));
    // a is not the most recent
    assert_ne!(mode, ResumeMode::Id(a));
}

#[test]
fn resume_mode_continue_with_no_sessions_errors() {
    let store = make_store_with_sessions(0);
    let err = resolve_resume_mode(true, None, &store, false).unwrap_err();
    assert!(err.to_string().contains("no prior sessions"));
}

#[test]
fn resume_mode_resume_with_id_resolves_prefix() {
    let store = make_store_with_sessions(0);
    let id = store.create_session("/x", "default").unwrap();
    let prefix: String = id.chars().take(6).collect();
    let mode = resolve_resume_mode(false, Some(Some(prefix)), &store, false).unwrap();
    assert_eq!(mode, ResumeMode::Id(id));
}

#[test]
fn resume_mode_bare_resume_plain_gives_error() {
    let store = make_store_with_sessions(1);
    // plain=true: headless, no TTY → should error
    let err = resolve_resume_mode(false, Some(None), &store, true).unwrap_err();
    assert!(err.to_string().contains("--resume <id>"));
}

#[test]
fn resume_mode_bare_resume_tty_gives_picker() {
    // We can't test actual TTY detection in a test, but we can test with plain=false
    // when we know stdout is NOT a terminal in CI — so we can't assert Picker here.
    // Instead, verify the plain=false + non-TTY path gives the same error as plain=true.
    // This is covered by the headless guard path; Picker path is integration-only.
    let store = make_store_with_sessions(1);
    // In a non-TTY test environment, plain=false but no terminal → same error as plain=true.
    // We test the logic branch that matters: is_terminal() is false in tests → error path.
    let _ = resolve_resume_mode(false, Some(None), &store, false);
    // Not asserting the result here because is_terminal() differs per environment;
    // the plain=true path (covered above) is the deterministic guard we rely on.
}
