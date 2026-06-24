/// Copy text to the clipboard from inside the TUI via two complementary paths, because neither
/// alone is enough. `arboard` (the long-lived instance) covers X11 / macOS / Windows-native, but on
/// Wayland it silently no-ops — its Wayland backend needs an owned window/surface a terminal app
/// doesn't have, so `/copy` "succeeded" yet copied nothing (the reported bug). OSC 52 covers that
/// gap by asking the TERMINAL to set the clipboard: the reliable path on Wayland, over SSH, and in
/// Windows Terminal / kitty / iTerm, with no display server needed. Both are best-effort and silent
/// (OSC 52 is an out-of-band control sequence the terminal intercepts, so it never corrupts the
/// alt-screen grid). The single arboard instance is reused so its X11 selection thread stays alive
/// (recreating it logs "clipboard dropped" and wrecks the layout).
pub(crate) fn copy_selection(clipboard: &mut Option<arboard::Clipboard>, text: &str) {
    if let Some(cb) = clipboard.as_mut() {
        let _ = cb.set_text(text.to_owned());
    }
    osc52_copy(text);
}

/// Emit an OSC 52 "set clipboard" escape so the terminal copies `text` (base64-encoded). When
/// running inside tmux/screen, wrap it in the multiplexer passthrough so it reaches the outer
/// terminal (requires tmux `set-clipboard on` / `allow-passthrough on`, the modern defaults).
fn osc52_copy(text: &str) {
    use base64::Engine as _;
    use std::io::Write as _;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    let term = std::env::var("TERM").unwrap_or_default();
    let seq = if term.starts_with("tmux") || term.starts_with("screen") {
        // tmux passthrough: ESC P tmux ; <payload with each ESC doubled> ESC \
        format!("\x1bPtmux;\x1b\x1b]52;c;{b64}\x07\x1b\\")
    } else {
        format!("\x1b]52;c;{b64}\x07")
    };
    let mut out = std::io::stdout();
    let _ = out.write_all(seq.as_bytes());
    let _ = out.flush();
}

/// The Nth-latest non-empty assistant response from a [`Session::history`] list (which is
/// oldest-first, user + assistant only). `nth` is 1-based counting back from the most recent
/// (1 = the last response, 2 = the one before). `None` when fewer than `nth` assistant responses
/// exist. Pure, so `/copy`'s selection logic is unit-tested without a live session.
pub(crate) fn nth_assistant_response(
    history: &[(forge_types::Role, String)],
    nth: usize,
) -> Option<String> {
    history
        .iter()
        .filter(|(role, text)| {
            matches!(role, forge_types::Role::Assistant) && !text.trim().is_empty()
        })
        .map(|(_, text)| text.clone())
        .rev()
        .nth(nth.saturating_sub(1))
}

/// Extract fenced code blocks from a markdown string as `(lang, code)` pairs (lang is the fence
/// info string, e.g. `rust`, or `""`). Handles the common ```-fence form; an unterminated final
/// block is still captured. Used by `/copy` to offer per-block selection.
pub(crate) fn extract_code_blocks(md: &str) -> Vec<(String, String)> {
    let mut blocks = Vec::new();
    let mut in_block = false;
    let mut lang = String::new();
    let mut buf = String::new();
    for line in md.lines() {
        if let Some(rest) = line.trim_start().strip_prefix("```") {
            if in_block {
                blocks.push((std::mem::take(&mut lang), std::mem::take(&mut buf)));
                in_block = false;
            } else {
                in_block = true;
                lang = rest.trim().to_string();
            }
        } else if in_block {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    if in_block && !buf.trim().is_empty() {
        blocks.push((lang, buf));
    }
    blocks
}

/// A sensible file extension for a fenced block's language tag (`rust` → `rs`, …); `txt` default.
fn ext_for_lang(lang: &str) -> &'static str {
    match lang
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rust" | "rs" => "rs",
        "python" | "py" => "py",
        "bash" | "sh" | "shell" | "zsh" => "sh",
        "javascript" | "js" => "js",
        "typescript" | "ts" => "ts",
        "json" => "json",
        "toml" => "toml",
        "yaml" | "yml" => "yaml",
        "go" => "go",
        "c" => "c",
        "cpp" | "c++" => "cpp",
        "java" => "java",
        "html" => "html",
        "css" => "css",
        "sql" => "sql",
        "markdown" | "md" => "md",
        _ => "txt",
    }
}

/// Write copied text to a timestamped file in the cwd (the `w` key in the `/copy` picker — useful
/// over SSH where the clipboard can't reach the local machine). Returns the path written.
pub(crate) fn write_copy_to_file(text: &str, lang: &str) -> std::io::Result<std::path::PathBuf> {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path = std::env::current_dir()
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join(format!("forge-copy-{ts}.{}", ext_for_lang(lang)));
    std::fs::write(&path, text)?;
    Ok(path)
}
