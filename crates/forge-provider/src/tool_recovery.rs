//! Recover tool calls a model emitted as TEXT instead of structured calls.
//!
//! Some providers' native adapters (notably genai 0.6's Gemini adapter for newer models) fail to
//! parse a model's function-call output into structured `tool_calls` — the call leaks into the
//! assistant's text content as XML/markup and is never executed. Forge then sees empty
//! `tool_calls`, treats the narration as a final answer, and "succeeds" without doing anything
//! (e.g. claiming a PR was merged / a tag pushed). This module is the recovery pass: when the
//! provider returned no structured calls, scan the text for the well-known textual tool-call
//! formats, reconstruct the calls, and strip them from the visible content.
//!
//! Formats handled (the ones models actually emit):
//!   - Anthropic / Claude-style:  `<invoke name="T"><parameter name="p">v</parameter></invoke>`,
//!     optionally wrapped in `<function_calls>…</function_calls>`.
//!   - Qwen / ollama-style:       `<tool_call>{"name":"T","arguments":{…}}</tool_call>`.
//!   - Llama / Groq-style:        `<function=T>{"p":"v"}</function>`, optionally wrapped in
//!     `<tool_call>…</tool_call>` (observed leaking from groq llama-3.x via the mesh).
//!
//! Wrapper namespaces some SDKs prepend (`default_api:`, `default_api.`, `functions.`,
//! `mcp__forge__`) are normalized back to the bare Forge tool name so the recovered call dispatches.

use forge_types::ToolCall;
use serde_json::{Map, Value};

/// Scan `content` for textual tool calls. Returns the recovered calls and `content` with the
/// matched spans removed (so the user doesn't see raw markup). Returns no calls and the content
/// unchanged when nothing tool-call-shaped is present — the overwhelmingly common case, so this is
/// cheap (a couple of substring checks) on normal prose.
pub fn recover_text_tool_calls(content: &str) -> (Vec<ToolCall>, String) {
    // Fast bail: none of the recoverable markers are present.
    if !content.contains("<invoke")
        && !content.contains("<tool_call")
        && !content.contains("<function=")
    {
        return (Vec::new(), content.to_string());
    }

    let mut calls = Vec::new();
    let mut cleaned = content.to_string();

    // `<function=…>` is processed first so a `<function=…>` wrapped inside `<tool_call>…</tool_call>`
    // is extracted before the (now-empty) tool_call wrapper is stripped as leftover.
    for (open, close) in [
        ("<function=", "</function>"),
        ("<invoke", "</invoke>"),
        ("<tool_call>", "</tool_call>"),
    ] {
        while let Some(start) = cleaned.find(open) {
            let Some(rel_end) = cleaned[start..].find(close) else {
                break; // unterminated — leave it as text rather than guess
            };
            let end = start + rel_end + close.len();
            let span = cleaned[start..end].to_string();
            if let Some(call) = parse_span(&span, calls.len()) {
                calls.push(call);
            }
            cleaned.replace_range(start..end, "");
        }
    }

    // Tidy leftover wrapper tags and the whitespace the removed spans left behind.
    for tag in ["<function_calls>", "</function_calls>"] {
        cleaned = cleaned.replace(tag, "");
    }
    let cleaned = cleaned.trim().to_string();
    (calls, cleaned)
}

fn parse_span(span: &str, idx: usize) -> Option<ToolCall> {
    let mk = |name: String, args: Value| {
        Some(ToolCall {
            id: format!("recovered_{idx}"),
            name: normalize_tool_name(&name),
            args,
        })
    };

    // <function=NAME>{json args}</function>  (Llama/Groq). NAME up to the first '>'; body, if
    // present, is a JSON object of arguments. Recover even an empty body so a degenerate call can't
    // silently vanish from the cleaned text (the honest-failure guard / dispatch error then handles
    // it) rather than being mistaken for a final answer.
    if let Some(after) = span.strip_prefix("<function=") {
        let gt = after.find('>')?;
        let name = after[..gt].trim().trim_matches(['"', '\'']).to_string();
        if name.is_empty() {
            return None;
        }
        let body = after[gt + 1..].trim_end_matches("</function>").trim();
        let args = serde_json::from_str::<Value>(body)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(Map::new()));
        return mk(name, args);
    }

    if span.starts_with("<tool_call") {
        // Inner JSON: {"name": "...", "arguments"|"parameters": {...}}
        let inner = span
            .trim_start_matches("<tool_call>")
            .trim_end_matches("</tool_call>")
            .trim();
        let v: Value = serde_json::from_str(inner).ok()?;
        let name = v.get("name")?.as_str()?.to_string();
        let args = v
            .get("arguments")
            .or_else(|| v.get("parameters"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        return mk(name, args);
    }

    // <invoke name="T"> … </invoke>
    let name = attr_value(span, "name")?;
    let mut args = Map::new();
    let mut rest = span;
    while let Some(p) = rest.find("<parameter") {
        let after = &rest[p..];
        let pname = attr_value(after, "name")?;
        let gt = after.find('>')? + 1;
        let val_end = after.find("</parameter>")?;
        let raw = after[gt..val_end].trim();
        args.insert(pname, coerce(raw));
        rest = &after[val_end + "</parameter>".len()..];
    }
    // No <parameter> tags but a JSON body inside the invoke is a valid single-object arg form.
    if args.is_empty() {
        if let Some(gt) = span.find('>') {
            let body = span[gt + 1..].trim_end_matches("</invoke>").trim();
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(body) {
                return mk(name, Value::Object(m));
            }
        }
    }
    mk(name, Value::Object(args))
}

/// Extract `attr="value"` (or `attr='value'`) immediately following the tag name.
fn attr_value(s: &str, attr: &str) -> Option<String> {
    let key = format!("{attr}=");
    let at = s.find(&key)? + key.len();
    let bytes = s.as_bytes();
    let quote = *bytes.get(at)?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let rest = &s[at + 1..];
    let close = rest.find(quote as char)?;
    Some(rest[..close].to_string())
}

/// A parameter's text value parsed as JSON when it looks like JSON (object/array/number/bool),
/// else kept as a plain string. Models emit both `<parameter name="n">5</parameter>` and
/// `<parameter name="tasks">[{…}]</parameter>`, and the tool schema expects the typed form.
fn coerce(raw: &str) -> Value {
    let looks_json = raw.starts_with('{')
        || raw.starts_with('[')
        || raw == "true"
        || raw == "false"
        || raw == "null"
        || raw.parse::<f64>().is_ok();
    if looks_json {
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            return v;
        }
    }
    Value::String(raw.to_string())
}

/// Strip SDK wrapper namespaces so the recovered name matches a registered Forge tool. Forge's
/// own tools are advertised bare on the direct path (`update_tasks`, `shell`), but models copy
/// the bridge/SDK spelling (`mcp__forge__update_tasks`, `default_api:update_tasks`).
fn normalize_tool_name(raw: &str) -> String {
    let n = raw.trim();
    for pre in ["default_api:", "default_api.", "functions.", "mcp__forge__"] {
        if let Some(s) = n.strip_prefix(pre) {
            return s.to_string();
        }
    }
    n.to_string()
}

/// Cheap detector for forge-core's honest-failure guard: does this text contain an un-executed
/// tool call? (Same markers `recover_text_tool_calls` keys on, plus the bare `default_api:` form.)
pub fn looks_like_unexecuted_tool_call(content: &str) -> bool {
    content.contains("<invoke")
        || content.contains("<tool_call")
        || content.contains("<function=")
        || content.contains("default_api:")
        || content.contains("default_api.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_gemini_invoke_block_and_normalizes_namespace() {
        let text = "Updating tasks now.\n\
            <invoke name=\"default_api:update_tasks\">\
            <parameter name=\"tasks\">[{\"title\":\"a\",\"status\":\"done\"}]</parameter>\
            </invoke>\nDone.";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].name, "update_tasks",
            "default_api: prefix stripped"
        );
        assert_eq!(calls[0].args["tasks"][0]["title"], "a");
        assert!(!cleaned.contains("<invoke"), "markup stripped from content");
        assert!(cleaned.contains("Updating tasks now."));
    }

    #[test]
    fn recovers_mcp_forge_prefixed_invoke() {
        let text = "<function_calls><invoke name=\"mcp__forge__shell\">\
            <parameter name=\"command\">git status</parameter></invoke></function_calls>";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args["command"], "git status");
        assert!(!cleaned.contains("function_calls"));
    }

    #[test]
    fn recovers_qwen_tool_call_json() {
        let text =
            "<tool_call>{\"name\":\"read_file\",\"arguments\":{\"path\":\"x.rs\"}}</tool_call>";
        let (calls, _) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args["path"], "x.rs");
    }

    #[test]
    fn coerces_numeric_and_json_params_but_keeps_strings() {
        let text = "<invoke name=\"t\">\
            <parameter name=\"n\">5</parameter>\
            <parameter name=\"msg\">hello world</parameter>\
            <parameter name=\"on\">true</parameter></invoke>";
        let (calls, _) = recover_text_tool_calls(text);
        assert_eq!(calls[0].args["n"], 5);
        assert_eq!(calls[0].args["msg"], "hello world");
        assert_eq!(calls[0].args["on"], true);
    }

    #[test]
    fn recovers_llama_function_format() {
        let text = "Calling it.\n<function=read_file>{\"path\":\"x.rs\"}</function>\nok";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].args["path"], "x.rs");
        assert!(
            !cleaned.contains("<function="),
            "markup stripped: {cleaned}"
        );
        assert!(cleaned.contains("Calling it."));
    }

    #[test]
    fn recovers_function_format_wrapped_in_tool_call() {
        let text = "<tool_call><function=shell>{\"command\":\"ls\"}</function></tool_call>";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "calls: {calls:?}");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args["command"], "ls");
        assert!(
            !cleaned.contains("<function="),
            "no leftover markup: {cleaned}"
        );
        assert!(!cleaned.contains("<tool_call"), "wrapper tidied: {cleaned}");
    }

    #[test]
    fn recovers_prefixed_function_name() {
        let text = "<function=mcp__forge__update_tasks>{\"tasks\":[]}</function>";
        let (calls, _) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].name, "update_tasks",
            "mcp__forge__ prefix stripped"
        );
    }

    #[test]
    fn empty_body_function_still_recovers_so_it_cannot_vanish() {
        // The exact degenerate leak observed live: <function=use_tool></function>.
        let text = "<function=use_tool></function>";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "recovered so it isn't silently dropped");
        assert_eq!(calls[0].name, "use_tool");
        assert!(calls[0].args.is_object());
        assert!(!cleaned.contains("<function="));
    }

    #[test]
    fn detector_flags_function_format() {
        assert!(looks_like_unexecuted_tool_call(
            "<function=shell>{\"command\":\"ls\"}</function>"
        ));
    }

    #[test]
    fn plain_prose_is_untouched() {
        let text = "Here is the plan. I will run the build and report back.";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn detector_flags_textual_calls_only() {
        assert!(looks_like_unexecuted_tool_call("<invoke name=\"x\">"));
        assert!(looks_like_unexecuted_tool_call(
            "default_api:update_tasks(...)"
        ));
        assert!(!looks_like_unexecuted_tool_call("a normal sentence"));
    }
}
