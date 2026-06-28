//! Pure rendering helpers: turn model output into styled ratatui [`Line`]s. Free of terminal
//! I/O so it's unit-testable on strings: `markdown_to_lines` (markdown → styled lines),
//! `highlight_code` (syntect syntax highlighting), and `diff_to_lines`/`diff_to_plain`
//! (a `similar`-based unified diff for review-before-apply).
//!
//! `pulldown-cmark` is *total* (it degrades malformed markdown to literal text and never
//! panics), so this renderer never drops content or crashes on bad input.

use std::sync::OnceLock;

use forge_types::{DiffKind, FileDiff};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use similar::{ChangeTag, TextDiff};
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme, ThemeSet};
use syntect::parsing::SyntaxSet;

// Palette — exact values from crate::app so rendered markdown is pixel-consistent with the TUI.
const ORANGE: Color = Color::Rgb(255, 138, 48); // forge brand — warm ember
const ACCENT: Color = Color::Rgb(82, 162, 255); // electric blue (active/interactive)
const DIM: Color = Color::Rgb(82, 87, 108); // muted / secondary
const TEXT: Color = Color::Rgb(208, 213, 224); // primary body text
const OKGREEN: Color = Color::Rgb(92, 208, 122); // success / ok
const ERRRED: Color = Color::Rgb(243, 92, 92); // error
const WARNYEL: Color = Color::Rgb(238, 188, 82); // warning
const TOOLCYAN: Color = Color::Rgb(75, 212, 218); // tools / code / lattice
const CODEFG: Color = TEXT; // code body text = primary body text
const CODEBG: Color = Color::Rgb(40, 40, 48); // code block background

/// Cap on rendered changed lines before truncating (keeps a huge diff from flooding scrollback).
const MAX_DIFF_LINES: usize = 500;

/// The base indent every rendered line carries (matches the plain `body_line` convention).
const INDENT: &str = "  ";

/// Bundled syntaxes + theme, loaded once. The cost is paid at most once per process and
/// only when a code block is actually highlighted — never on `forge --help`/`forge run`.
fn highlighter() -> &'static (SyntaxSet, Theme) {
    static HL: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    HL.get_or_init(|| {
        let ss = SyntaxSet::load_defaults_newlines();
        let ts = ThemeSet::load_defaults();
        let theme = ts
            .themes
            .get("base16-ocean.dark")
            .or_else(|| ts.themes.values().next())
            .cloned()
            .unwrap_or_default();
        (ss, theme)
    })
}

/// Syntax-highlight `lines` of source in `lang` into per-line span vectors. Unknown/empty
/// language falls back to plain (single dim span per line); never panics.
pub fn highlight_code(lang: &str, lines: &[String]) -> Vec<Vec<Span<'static>>> {
    let (ss, theme) = highlighter();
    let syntax = (!lang.is_empty())
        .then(|| ss.find_syntax_by_token(lang))
        .flatten()
        .unwrap_or_else(|| ss.find_syntax_plain_text());
    let mut h = HighlightLines::new(syntax, theme);
    lines
        .iter()
        .map(|line| match h.highlight_line(line, ss) {
            Ok(ranges) => ranges
                .into_iter()
                .map(|(st, text)| {
                    let fg = st.foreground;
                    let mut style = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
                    if st.font_style.contains(FontStyle::BOLD) {
                        style = style.add_modifier(Modifier::BOLD);
                    }
                    if st.font_style.contains(FontStyle::ITALIC) {
                        style = style.add_modifier(Modifier::ITALIC);
                    }
                    Span::styled(text.to_string(), style)
                })
                .collect(),
            Err(_) => vec![Span::styled(line.clone(), Style::default().fg(CODEFG))],
        })
        .collect()
}

fn diff_header(diff: &FileDiff) -> String {
    let kind = match diff.kind {
        DiffKind::Created => "new file",
        DiffKind::Modified => "modified",
        DiffKind::Deleted => "deleted",
    };
    format!("{INDENT}✎ {} ({kind})", diff.path)
}

/// Render a proposed file change as a styled unified diff (path header, `@@` hunks, `+`/`-`/
/// context gutters, syntax-highlighted bodies). Binary targets show a one-line summary.
pub fn diff_to_lines(diff: &FileDiff) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(Span::styled(
        diff_header(diff),
        Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
    ))];

    if diff.binary {
        let o = diff.old.as_ref().map(|s| s.len()).unwrap_or(0);
        let n = diff.new.as_ref().map(|s| s.len()).unwrap_or(0);
        out.push(Line::from(Span::styled(
            format!("{INDENT}binary file ({o} → {n} bytes)"),
            Style::default().fg(DIM),
        )));
        return out;
    }

    let old = diff.old.as_deref().unwrap_or("");
    let new = diff.new.as_deref().unwrap_or("");
    let lang = diff.lang.as_deref().unwrap_or("");
    let td = TextDiff::from_lines(old, new);
    let gutter =
        |sym: &str, c: Color| Span::styled(format!("{INDENT}{sym} "), Style::default().fg(c));
    let mut emitted = 0usize;

    for group in td.grouped_ops(3) {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let os = first.old_range().start;
        let oe = last.old_range().end;
        let ns = first.new_range().start;
        let ne = last.new_range().end;
        out.push(Line::from(Span::styled(
            format!(
                "{INDENT}@@ -{},{} +{},{} @@",
                os + 1,
                oe - os,
                ns + 1,
                ne - ns
            ),
            Style::default().fg(TOOLCYAN),
        )));
        for op in group {
            for change in td.iter_changes(&op) {
                if emitted >= MAX_DIFF_LINES {
                    out.push(Line::from(Span::styled(
                        format!("{INDENT}… (diff truncated — full change in the tool result)"),
                        Style::default().fg(DIM),
                    )));
                    return out;
                }
                emitted += 1;
                let text = change.value().trim_end_matches('\n').to_string();
                let (sym, color) = match change.tag() {
                    ChangeTag::Delete => ("-", ERRRED),
                    ChangeTag::Insert => ("+", OKGREEN),
                    ChangeTag::Equal => (" ", DIM),
                };
                let mut line = vec![gutter(sym, color)];
                // highlight the body; context/added stay readable, deletions tinted red.
                let body = highlight_code(lang, &[text]);
                if change.tag() == ChangeTag::Delete {
                    let joined: String = body
                        .into_iter()
                        .flatten()
                        .map(|s| s.content.into_owned())
                        .collect();
                    line.push(Span::styled(joined, Style::default().fg(ERRRED)));
                } else {
                    line.extend(body.into_iter().flatten());
                }
                out.push(Line::from(line));
            }
        }
    }
    if emitted == 0 {
        out.push(Line::from(Span::styled(
            format!("{INDENT}(no textual changes)"),
            Style::default().fg(DIM),
        )));
    }
    out
}

/// Plain unified-diff text (no ANSI) for the headless/piped path.
pub fn diff_to_plain(diff: &FileDiff) -> String {
    let mut s = format!("{}\n", diff_header(diff).trim_start());
    if diff.binary {
        s.push_str("  binary file (not shown)\n");
        return s;
    }
    let td = TextDiff::from_lines(
        diff.old.as_deref().unwrap_or(""),
        diff.new.as_deref().unwrap_or(""),
    );
    for change in td.iter_all_changes() {
        let sym = match change.tag() {
            ChangeTag::Delete => "-",
            ChangeTag::Insert => "+",
            ChangeTag::Equal => " ",
        };
        s.push_str(&format!("{sym} {}", change.value()));
    }
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

/// Color for a finding's severity (most→least): critical=red, high=orange, medium=yellow, low=dim.
fn severity_color(sev: forge_types::Severity) -> Color {
    use forge_types::Severity;
    match sev {
        Severity::Critical => ERRRED,
        Severity::High => ORANGE,
        Severity::Medium => WARNYEL,
        Severity::Low => DIM,
    }
}

/// Render the MCP server listing (`/mcp`) as styled scrollback lines: a status glyph per server,
/// its transport, and tool/resource/prompt counts (mcp-client.md §5.7).
pub fn mcp_status_lines(servers: &[forge_types::McpServerLine]) -> Vec<Line<'static>> {
    if servers.is_empty() {
        return vec![Line::from(Span::styled(
            "  ⚒ no MCP servers configured — declare them in .forge/mcp.toml (or `forge mcp import`)"
                .to_string(),
            Style::default().fg(DIM),
        ))];
    }
    let mut out = vec![Line::from(Span::styled(
        format!("  ◈ MCP servers  ({} configured)", servers.len()),
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    ))];
    for s in servers {
        let (glyph, color) = match s.status.as_str() {
            "connected" => ("●", OKGREEN),
            "slow" => ("●", WARNYEL),
            "reconnecting" => ("↻", WARNYEL),
            "failed" | "unauthorized" => ("○", ERRRED),
            _ => ("○", DIM), // disabled / unknown
        };
        let mut spans = vec![
            Span::styled(format!("    {glyph} "), Style::default().fg(color)),
            Span::styled(
                format!("{:<12}", s.name),
                Style::default().fg(CODEFG).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{:<13}{:<6}", s.status, s.transport),
                Style::default().fg(color),
            ),
            Span::styled(
                format!(
                    "{} tools · {} resources · {} prompts",
                    s.tools, s.resources, s.prompts
                ),
                Style::default().fg(DIM),
            ),
        ];
        if let Some(detail) = &s.detail {
            spans.push(Span::styled(
                format!("   {detail}"),
                Style::default().fg(ERRRED),
            ));
        }
        out.push(Line::from(spans));
    }
    out.push(Line::from(Span::styled(
        "    tools load on demand — find one with mcp_search_tools, then run it with mcp_call."
            .to_string(),
        Style::default().fg(DIM),
    )));
    out
}

/// Render an [`AssayReport`](forge_types::AssayReport) as styled lines for the TUI scrollback:
/// a colored summary header, then each ranked finding with severity-colored gutter, location,
/// title, why, and suggested fix.
pub fn assay_report_lines(r: &forge_types::AssayReport) -> Vec<Line<'static>> {
    let [crit, high, med, low] = r.severity_counts();
    let id8: String = r.run_id.chars().take(8).collect();
    let mut out = vec![
        Line::from(Span::styled(
            format!(
                "{INDENT}⚒ ASSAY REPORT  run {id8}  scope: {}",
                r.scope.label()
            ),
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        )),
        Line::from(vec![
            Span::styled(
                format!("{INDENT}{} findings  ", r.findings.len()),
                Style::default().fg(CODEFG),
            ),
            Span::styled(format!("{crit} crit "), Style::default().fg(ERRRED)),
            Span::styled(format!("{high} high "), Style::default().fg(ORANGE)),
            Span::styled(format!("{med} med "), Style::default().fg(WARNYEL)),
            Span::styled(format!("{low} low"), Style::default().fg(DIM)),
            Span::styled(
                format!("  ·  ${:.4}", r.cost_usd),
                Style::default().fg(OKGREEN),
            ),
        ]),
    ];
    if !r.skipped_lenses.is_empty() {
        let sk: Vec<String> = r
            .skipped_lenses
            .iter()
            .map(|(l, why)| format!("{l} ({why})"))
            .collect();
        out.push(Line::from(Span::styled(
            format!("{INDENT}skipped: {}", sk.join(", ")),
            Style::default().fg(DIM),
        )));
    }
    if r.findings.is_empty() {
        out.push(Line::from(Span::styled(
            format!("{INDENT}no findings — clean, or the scope had no analyzable source."),
            Style::default().fg(DIM),
        )));
        return out;
    }
    for (i, f) in r.findings.iter().enumerate() {
        let color = severity_color(f.severity);
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        out.push(Line::from(vec![
            Span::styled(format!("{INDENT}{:>2}. ", i + 1), Style::default().fg(DIM)),
            Span::styled(
                format!("{} ", f.severity.as_str().to_uppercase()),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{} ", f.category.as_str()),
                Style::default().fg(CODEFG),
            ),
            Span::styled(loc, Style::default().fg(DIM)),
        ]));
        out.push(Line::from(Span::styled(
            format!("{INDENT}    {}", f.title),
            Style::default().fg(CODEFG),
        )));
        if !f.suggested_fix.is_empty() {
            out.push(Line::from(Span::styled(
                format!(
                    "{INDENT}    fix: {} ({})",
                    f.suggested_fix,
                    f.effort.as_str()
                ),
                Style::default().fg(DIM),
            )));
        }
    }
    out
}

/// Plain (no-ANSI) rendering of an assay report, for the headless/piped presenter.
pub fn assay_report_plain(r: &forge_types::AssayReport) -> String {
    let [crit, high, med, low] = r.severity_counts();
    let id8: String = r.run_id.chars().take(8).collect();
    let mut s = format!("\n⚒ ASSAY REPORT  run {id8}  scope: {}\n", r.scope.label());
    s.push_str(&format!(
        "{} findings · {crit} critical · {high} high · {med} medium · {low} low · ${:.4}\n",
        r.findings.len(),
        r.cost_usd
    ));
    if !r.skipped_lenses.is_empty() {
        let sk: Vec<String> = r
            .skipped_lenses
            .iter()
            .map(|(l, why)| format!("{l} ({why})"))
            .collect();
        s.push_str(&format!("skipped: {}\n", sk.join(", ")));
    }
    if r.findings.is_empty() {
        s.push_str("no findings — clean, or the scope had no analyzable source.\n");
        return s;
    }
    for (i, f) in r.findings.iter().enumerate() {
        let loc = match f.line {
            Some(l) => format!("{}:{l}", f.file),
            None => f.file.clone(),
        };
        s.push_str(&format!(
            "{:>2}. [{} · {}] {} — {loc}\n    {}\n",
            i + 1,
            f.severity.as_str().to_uppercase(),
            f.confidence.as_str(),
            f.category.as_str(),
            f.title,
        ));
        if !f.suggested_fix.is_empty() {
            s.push_str(&format!(
                "    fix: {} ({})\n",
                f.suggested_fix,
                f.effort.as_str()
            ));
        }
    }
    s
}

/// Render a markdown document to styled lines, indented to match the conversation body.
pub fn markdown_to_lines(md: &str) -> Vec<Line<'static>> {
    let mut r = Renderer::default();
    for ev in Parser::new_ext(md, Options::empty()) {
        r.event(ev);
    }
    r.finish()
}

#[derive(Default)]
struct Renderer {
    lines: Vec<Line<'static>>,
    cur: Vec<Span<'static>>,
    bold: u32,
    italic: u32,
    heading: bool,
    /// list markers stack: None = bullet, Some(n) = next ordered number
    lists: Vec<Option<u64>>,
    quote: u32,
    /// when Some, we're inside a fenced code block: (language tag, accumulated raw lines)
    code: Option<(String, Vec<String>)>,
}

impl Renderer {
    fn style(&self) -> Style {
        let mut s = Style::default().fg(CODEFG);
        if self.heading {
            s = s.fg(ORANGE).add_modifier(Modifier::BOLD);
        }
        if self.bold > 0 {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            s = s.add_modifier(Modifier::ITALIC);
        }
        s
    }

    fn indent_prefix(&self) -> String {
        // base indent + 2 spaces per nested list level + a quote bar.
        let mut p = String::from(INDENT);
        for _ in 0..self.lists.len().saturating_sub(1) {
            p.push_str("  ");
        }
        p
    }

    fn push_text(&mut self, text: &str) {
        if self.cur.is_empty() {
            // start the line with its indent (+ quote bar if quoted).
            self.cur.push(Span::raw(self.indent_prefix()));
            if self.quote > 0 {
                self.cur.push(Span::styled("▏ ", Style::default().fg(DIM)));
            }
        }
        self.cur.push(Span::styled(text.to_string(), self.style()));
    }

    fn flush_line(&mut self) {
        if !self.cur.is_empty() {
            self.lines.push(Line::from(std::mem::take(&mut self.cur)));
        }
    }

    fn blank(&mut self) {
        // collapse consecutive blanks
        if self.lines.last().map(|l| l.spans.is_empty()) != Some(true) {
            self.lines.push(Line::default());
        }
    }

    fn event(&mut self, ev: Event<'_>) {
        // Inside a fenced code block, capture raw text verbatim until it closes.
        if let Some((_, buf)) = self.code.as_mut() {
            match ev {
                Event::Text(t) => {
                    for (i, part) in t.split('\n').enumerate() {
                        if i == 0 {
                            if let Some(last) = buf.last_mut() {
                                last.push_str(part);
                                continue;
                            }
                        }
                        buf.push(part.to_string());
                    }
                    return;
                }
                Event::End(TagEnd::CodeBlock) => {
                    let (lang, code) = self.code.take().unwrap_or_default();
                    self.render_code_block(&lang, &code);
                    return;
                }
                _ => return,
            }
        }

        match ev {
            Event::Start(Tag::Heading { .. }) => {
                self.flush_line();
                self.blank();
                self.heading = true;
            }
            Event::End(TagEnd::Heading(level)) => {
                self.flush_line();
                if matches!(level, HeadingLevel::H1 | HeadingLevel::H2) {
                    self.lines.push(Line::from(Span::styled(
                        format!("{INDENT}{}", "─".repeat(44)),
                        Style::default().fg(DIM),
                    )));
                }
                self.heading = false;
                self.blank();
            }
            Event::Start(Tag::Paragraph) => self.flush_line(),
            Event::End(TagEnd::Paragraph) => {
                self.flush_line();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            Event::Start(Tag::Strong) => self.bold += 1,
            Event::End(TagEnd::Strong) => self.bold = self.bold.saturating_sub(1),
            Event::Start(Tag::Emphasis) => self.italic += 1,
            Event::End(TagEnd::Emphasis) => self.italic = self.italic.saturating_sub(1),
            Event::Start(Tag::BlockQuote(_)) => {
                self.flush_line();
                self.quote += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                self.flush_line();
                self.quote = self.quote.saturating_sub(1);
                self.blank();
            }
            Event::Start(Tag::List(first)) => self.lists.push(first),
            Event::End(TagEnd::List(_)) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank();
                }
            }
            Event::Start(Tag::Item) => {
                self.flush_line();
                // emit the marker as the start of this line.
                self.cur.push(Span::raw(self.indent_prefix()));
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let m = format!("{n}. ");
                        *n += 1;
                        m
                    }
                    _ => "• ".to_string(),
                };
                self.cur
                    .push(Span::styled(marker, Style::default().fg(ACCENT)));
            }
            Event::End(TagEnd::Item) => self.flush_line(),
            Event::Start(Tag::CodeBlock(kind)) => {
                self.flush_line();
                let lang = match kind {
                    CodeBlockKind::Fenced(s) => {
                        s.split_whitespace().next().unwrap_or("").to_string()
                    }
                    CodeBlockKind::Indented => String::new(),
                };
                self.code = Some((lang, vec![String::new()]));
            }
            Event::Start(Tag::Link { .. }) => {} // text rendered inline; URL appended on end
            Event::End(TagEnd::Link) => {}
            Event::Text(t) => self.push_text(&t),
            Event::Code(c) => {
                if self.cur.is_empty() {
                    self.cur.push(Span::raw(self.indent_prefix()));
                }
                self.cur.push(Span::styled(
                    c.to_string(),
                    Style::default().fg(TOOLCYAN).bg(CODEBG),
                ));
            }
            Event::SoftBreak => self.push_text(" "),
            Event::HardBreak => self.flush_line(),
            Event::Rule => {
                self.flush_line();
                self.blank();
                self.lines.push(Line::from(Span::styled(
                    format!("{INDENT}{}", "─".repeat(48)),
                    Style::default().fg(DIM),
                )));
                self.blank();
            }
            _ => {}
        }
    }

    fn render_code_block(&mut self, lang: &str, code: &[String]) {
        // Trim a trailing empty line that the fence parser commonly leaves.
        let mut lines: &[String] = code;
        while matches!(lines.last(), Some(l) if l.is_empty()) {
            lines = &lines[..lines.len() - 1];
        }
        let frame = Style::default().fg(DIM);
        let label = if lang.is_empty() { "text" } else { lang };
        let bar = "─".repeat(48usize.saturating_sub(label.len() + 2));
        self.lines.push(Line::from(Span::styled(
            format!("{INDENT}┌ {label} {bar}"),
            frame,
        )));
        for spans in highlight_code(lang, lines) {
            let mut line = vec![Span::styled(format!("{INDENT}│ "), frame)];
            line.extend(spans);
            self.lines.push(Line::from(line));
        }
        self.lines.push(Line::from(Span::styled(
            format!("{INDENT}└{}", "─".repeat(48)),
            frame,
        )));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // drop a trailing blank
        while matches!(self.lines.last(), Some(l) if l.spans.is_empty()) {
            self.lines.pop();
        }
        self.lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn has_modifier(lines: &[Line], needle: &str, m: Modifier) -> bool {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| s.content.contains(needle) && s.style.add_modifier.contains(m))
    }

    #[test]
    fn heading_renders_without_hashes_and_is_bold() {
        let out = markdown_to_lines("## Plan\n");
        let t = text_of(&out);
        assert!(t.contains("Plan"), "heading text shown");
        assert!(!t.contains("##"), "no literal hashes: {t:?}");
        assert!(
            has_modifier(&out, "Plan", Modifier::BOLD),
            "heading is bold"
        );
    }

    #[test]
    fn bold_and_inline_code_strip_markup() {
        let out = markdown_to_lines("step **one** and `two`\n");
        let t = text_of(&out);
        assert!(t.contains("one") && t.contains("two"));
        assert!(
            !t.contains("**") && !t.contains('`'),
            "markup stripped: {t:?}"
        );
        assert!(has_modifier(&out, "one", Modifier::BOLD), "bold applied");
    }

    #[test]
    fn bullet_list_gets_markers() {
        let out = markdown_to_lines("- alpha\n- beta\n");
        let t = text_of(&out);
        assert!(t.contains("• alpha"), "bullet marker: {t:?}");
        assert!(t.contains("• beta"));
        assert!(!t.contains("- alpha"));
    }

    #[test]
    fn ordered_list_numbers() {
        let out = markdown_to_lines("1. first\n2. second\n");
        let t = text_of(&out);
        assert!(t.contains("1. first") && t.contains("2. second"), "{t:?}");
    }

    #[test]
    fn fenced_code_block_is_framed_and_verbatim() {
        let out = markdown_to_lines("```rust\nlet x = 1;\n```\n");
        let t = text_of(&out);
        assert!(t.contains("let x = 1;"), "code preserved verbatim: {t:?}");
        assert!(t.contains('┌') && t.contains('└'), "code block framed");
        assert!(!t.contains("```"), "fence markers not shown");
    }

    #[test]
    fn malformed_markdown_does_not_panic_and_keeps_text() {
        let out = markdown_to_lines("**unbalanced and `weird ## not a heading mid-line");
        let t = text_of(&out);
        assert!(t.contains("unbalanced"));
        assert!(t.contains("weird"));
    }

    #[test]
    fn plain_text_passes_through() {
        let out = markdown_to_lines("the workspace looks healthy");
        assert_eq!(text_of(&out), "  the workspace looks healthy");
    }

    #[test]
    fn highlight_known_language_colors_tokens() {
        let lines = vec!["fn main() { let x = 1; }".to_string()];
        let spans = highlight_code("rust", &lines);
        assert_eq!(spans.len(), 1);
        let joined: String = spans[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(joined.contains("fn main()"), "source preserved: {joined:?}");
        // highlighting splits the line into more than one styled span.
        assert!(spans[0].len() > 1, "tokens are colored into multiple spans");
    }

    #[test]
    fn highlight_unknown_language_falls_back_to_plain() {
        let lines = vec!["some arbitrary text".to_string()];
        let spans = highlight_code("nonsense-lang-xyz", &lines);
        let joined: String = spans[0].iter().map(|s| s.content.as_ref()).collect();
        assert!(
            joined.contains("some arbitrary text"),
            "no crash, text kept"
        );
    }

    #[test]
    fn fenced_block_shows_language_label() {
        let out = markdown_to_lines("```python\nprint('hi')\n```\n");
        let t = text_of(&out);
        assert!(t.contains("python"), "language label on the fence: {t:?}");
        assert!(t.contains("print('hi')"), "code preserved");
    }

    #[test]
    fn diff_modified_has_gutters_and_hunk_header() {
        let diff = FileDiff {
            path: "src/a.rs".into(),
            kind: DiffKind::Modified,
            old: Some("let x = 1;\n".into()),
            new: Some("let x = 2;\n".into()),
            lang: Some("rust".into()),
            binary: false,
        };
        let t = text_of(&diff_to_lines(&diff));
        assert!(
            t.contains("src/a.rs") && t.contains("modified"),
            "header: {t:?}"
        );
        assert!(t.contains("@@"), "hunk header: {t:?}");
        assert!(t.contains("- ") && t.contains("+ "), "+/- gutters: {t:?}");
        assert!(
            t.contains("x = 1") && t.contains("x = 2"),
            "both versions shown"
        );
    }

    #[test]
    fn diff_created_file_is_all_additions() {
        let diff = FileDiff {
            path: "new.txt".into(),
            kind: DiffKind::Created,
            old: None,
            new: Some("hello\nworld\n".into()),
            lang: None,
            binary: false,
        };
        let t = text_of(&diff_to_lines(&diff));
        assert!(t.contains("new file"), "new-file header: {t:?}");
        assert!(t.contains("+ hello") && t.contains("+ world"));
    }

    #[test]
    fn diff_binary_shows_summary_only() {
        let diff = FileDiff {
            path: "img.png".into(),
            kind: DiffKind::Modified,
            old: Some("ab".into()),
            new: Some("abcd".into()),
            lang: None,
            binary: true,
        };
        let t = text_of(&diff_to_lines(&diff));
        assert!(t.contains("binary file"), "{t:?}");
        assert!(!t.contains("@@"), "no textual diff for binary");
    }

    #[test]
    fn assay_report_renders_summary_and_ranked_findings() {
        use forge_types::{
            AssayReport, AssayScope, Confidence, Effort, Finding, FindingCategory, Severity,
        };
        let report = AssayReport {
            run_id: "abcdef123456".into(),
            scope: AssayScope::Repo,
            findings: vec![Finding {
                id: "f1".into(),
                category: FindingCategory::Correctness,
                severity: Severity::Critical,
                confidence: Confidence::High,
                file: "core/lib.rs".into(),
                line: Some(204),
                title: "unwrap panics the turn".into(),
                rationale: "a 5xx aborts the session".into(),
                suggested_fix: "propagate via ?".into(),
                effort: Effort::Small,
                lens: "correctness".into(),
                verified: true,
            }],
            cost_usd: 0.118,
            skipped_lenses: vec![("design".into(), "timeout".into())],
        };
        let t = text_of(&assay_report_lines(&report));
        assert!(t.contains("run abcdef12"), "short run id: {t}");
        assert!(t.contains("1 findings"), "summary count");
        assert!(t.contains("skipped: design (timeout)"), "degradation noted");
        assert!(t.contains("CRITICAL") && t.contains("core/lib.rs:204"));
        assert!(t.contains("fix: propagate via ?"));

        // Plain form is ANSI-free for pipes.
        let plain = assay_report_plain(&report);
        assert!(!plain.contains('\u{1b}'), "no ANSI in plain report");
        assert!(plain.contains("CRITICAL"));
    }

    #[test]
    fn diff_to_plain_is_ansi_free_unified_text() {
        let diff = FileDiff {
            path: "a.txt".into(),
            kind: DiffKind::Modified,
            old: Some("one\ntwo\n".into()),
            new: Some("one\nTWO\n".into()),
            lang: None,
            binary: false,
        };
        let s = diff_to_plain(&diff);
        assert!(
            s.contains("- two") && s.contains("+ TWO"),
            "plain diff: {s:?}"
        );
        assert!(!s.contains('\u{1b}'), "no ANSI escapes");
    }
}
