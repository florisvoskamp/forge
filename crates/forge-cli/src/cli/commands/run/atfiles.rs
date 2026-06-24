/// Enumerate project files for `@path` completion: `git ls-files` first, then a portable directory
/// walk. The fallback used to shell out to Unix `find`, which silently produced nothing on Windows
/// (where `find.exe` is an unrelated text-search tool) — so `@path` completion was dead outside a git
/// repo on Windows. A plain `std::fs` walk works everywhere and needs no external program.
pub(crate) fn load_at_files() -> Vec<String> {
    if let Ok(out) = std::process::Command::new("git")
        .args(["ls-files"])
        .output()
    {
        if out.status.success() {
            return String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(|s| s.to_string())
                .collect();
        }
    }
    let base = std::path::Path::new(".");
    let mut out = Vec::new();
    walk_at_files(base, base, 5, &mut out);
    out
}

/// Recursive file walk for [`load_at_files`]: up to `depth` levels under `base`, files only,
/// skipping dot-entries, with `/`-normalized paths relative to `base`. Bounded so a giant tree can't
/// stall completion.
fn walk_at_files(
    dir: &std::path::Path,
    base: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth == 0 || out.len() >= 10_000 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue; // skip .git, dotfiles, hidden dirs
        }
        let Ok(ft) = entry.file_type() else { continue };
        let path = entry.path();
        if ft.is_dir() {
            walk_at_files(&path, base, depth - 1, out);
        } else if ft.is_file() {
            let rel = path.strip_prefix(base).unwrap_or(&path);
            out.push(rel.to_string_lossy().replace('\\', "/"));
        }
        if out.len() >= 10_000 {
            return;
        }
    }
}

/// Keep the `@path` picker in sync with the `@token` at the cursor: open + filter when present,
/// close when the token disappears. Files are loaded once on first open (cache lives in picker).
pub(crate) fn sync_at_picker_to_at_token(app: &mut forge_tui::App) {
    let cur = app.input_cursor.min(app.input.len());
    if let Some(tok) = forge_tui::at_token_at(&app.input, cur) {
        if app.at_picker.open {
            app.at_picker.query = tok.query;
            app.at_picker.clamp();
        } else {
            let files = load_at_files();
            app.at_picker.open_with(&tok.query, files);
        }
    } else {
        app.at_picker.close();
    }
}

/// Cap on a single `@file`'s injected size, so dropping a huge file into context can't blow the
/// window. Larger files are skipped with a note rather than truncated mid-token.
pub(crate) const AT_FILE_MAX_BYTES: usize = 96 * 1024;

/// Read the `@path` file references in a submitted prompt and return them as guidance context
/// blocks (one per file) plus the list of paths actually included. The `@path` token stays in the
/// user's text (echoed verbatim); the contents ride along as separate guidance so the displayed
/// line stays clean. Missing paths are treated as ordinary text (silently skipped — `@` is also a
/// mention sigil); binary/oversized files are skipped with a visible note.
pub(crate) fn expand_at_files(prompt: &str) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let (mut blocks, mut included, mut skipped) = (Vec::new(), Vec::new(), Vec::new());
    // Split on Unicode whitespace. The previous byte-by-byte scan cast each byte to a `char` and
    // sliced `&prompt[start..i]` on the result — which lands MID-CHARACTER for any multi-byte
    // whitespace (a pasted non-breaking space `\u{a0}`, an em space, …) and PANICS ("not a char
    // boundary"), crashing the whole turn. `split_whitespace` is UTF-8-correct and recognizes those.
    for word in prompt.split_whitespace() {
        let Some(path) = word.strip_prefix('@') else {
            continue;
        };
        if path.is_empty() || !seen.insert(path.to_string()) {
            continue;
        }
        match std::fs::read(path) {
            Ok(raw) if raw.len() > AT_FILE_MAX_BYTES => {
                skipped.push(format!("@{path} (>{}KB)", AT_FILE_MAX_BYTES / 1024));
            }
            Ok(raw) => match String::from_utf8(raw) {
                Ok(text) => {
                    blocks.push(format!("Referenced file `{path}`:\n```\n{text}\n```"));
                    included.push(path.to_string());
                }
                Err(_) => skipped.push(format!("@{path} (binary)")),
            },
            Err(_) => {} // not a real file — leave as plain text
        }
    }
    (blocks, included, skipped)
}
