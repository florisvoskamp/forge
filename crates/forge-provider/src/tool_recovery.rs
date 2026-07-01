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
        && !content.contains("{\"name\"")
        && !content.contains("```json")
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

    // Bare JSON object or array of objects (often fenced with ```json)
    // We only attempt this if the remaining content is *mostly* just the JSON,
    // to avoid false positives on prose that happens to contain a JSON example.
    let trimmed = cleaned.trim();
    let mut json_candidate = trimmed;
    if json_candidate.starts_with("```json") {
        json_candidate = json_candidate.trim_start_matches("```json").trim_start();
        if json_candidate.ends_with("```") {
            json_candidate = json_candidate.trim_end_matches("```").trim_end();
        }
    } else if json_candidate.starts_with("```") {
        json_candidate = json_candidate.trim_start_matches("```").trim_start();
        if json_candidate.ends_with("```") {
            json_candidate = json_candidate.trim_end_matches("```").trim_end();
        }
    }

    if json_candidate.starts_with('{') || json_candidate.starts_with('[') {
        if let Ok(val) = serde_json::from_str::<Value>(json_candidate) {
            let mut extracted = Vec::new();
            if let Some(arr) = val.as_array() {
                for item in arr {
                    if let Some(call) = parse_json_tool_call(item, calls.len() + extracted.len()) {
                        extracted.push(call);
                    }
                }
            } else if let Some(call) = parse_json_tool_call(&val, calls.len()) {
                extracted.push(call);
            }

            if !extracted.is_empty() {
                calls.extend(extracted);
                // If we successfully parsed the whole thing as tool calls, clear the content
                // so it doesn't leak.
                cleaned = String::new();
            }
        }
    }

    let cleaned = cleaned.trim().to_string();
    (calls, cleaned)
}

fn parse_json_tool_call(v: &Value, idx: usize) -> Option<ToolCall> {
    let name = v.get("name")?.as_str()?.to_string();
    let args = v
        .get("arguments")
        .or_else(|| v.get("parameters"))
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));

    Some(ToolCall {
        id: format!("recovered_{idx}"),
        name: normalize_tool_name(&name),
        args,
    })
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
        // Preferred: a JSON object body (Llama/Groq). Fallback: some models (e.g. qwen3-coder)
        // emit a MIXED format — Anthropic-style `<parameter …>` sub-tags INSIDE a `<function=…>`
        // tag — whose body is not JSON. Recover those params so the call isn't reduced to empty
        // args (an empty-arg call dispatches as a no-op and was seen to loop until timeout).
        let args = serde_json::from_str::<Value>(body)
            .ok()
            .filter(Value::is_object)
            .unwrap_or_else(|| Value::Object(parse_parameter_tags(body)));
        return mk(name, args);
    }

    if span.starts_with("<tool_call") {
        // Inner JSON: {"name": "...", "arguments"|"parameters": {...}}
        let inner = span
            .trim_start_matches("<tool_call>")
            .trim_end_matches("</tool_call>")
            .trim();
        let v: Value = serde_json::from_str(inner).ok()?;
        return parse_json_tool_call(&v, idx);
    }

    // <invoke name="T"> … </invoke>
    let name = attr_value(span, "name")?;
    let mut args = Map::new();
    let mut rest = span;
    while let Some(p) = rest.find("<parameter") {
        let after = &rest[p..];
        // A `<parameter>` with no name attribute must SKIP that tag, not abort the whole call —
        // otherwise one malformed param drops a recovered tool call and the bridge phantom-succeeds.
        // (Sister fn parse_parameter_tags already honors this contract.)
        let Some(pname) = attr_value(after, "name") else {
            let Some(gt) = after.find('>') else { break };
            rest = &after[gt + 1..];
            continue;
        };
        let gt = after.find('>')? + 1;
        let val_end = after.find("</parameter>")?;
        // A malformed open tag (no closing `>`) makes the first `>` land INSIDE `</parameter>`, so
        // `gt > val_end` and a raw `after[gt..val_end]` slice would panic on untrusted model output.
        // `.get` returns None for an inverted/out-of-bounds range; stop parsing params on a bad one.
        let Some(raw) = after.get(gt..val_end).map(str::trim) else {
            break;
        };
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

/// Pull `<parameter …>value</parameter>` sub-tags out of a tag body, supporting BOTH spellings a
/// model might use: Anthropic `<parameter name="key">` and Llama-ish `<parameter=key>`. Used as the
/// fallback when a `<function=…>` body isn't JSON (the mixed format some local models emit). Skips a
/// malformed tag rather than aborting, so one bad param can't drop the whole call.
fn parse_parameter_tags(s: &str) -> Map<String, Value> {
    let mut args = Map::new();
    let mut rest = s;
    while let Some(p) = rest.find("<parameter") {
        let after = &rest[p..];
        let Some(gt) = after.find('>') else { break };
        let head = &after[..gt];
        let key = if let Some(k) = attr_value(head, "name") {
            k
        } else if let Some(k) = head.strip_prefix("<parameter=") {
            k.trim().trim_matches(['"', '\'']).to_string()
        } else {
            rest = &after[gt + 1..];
            continue;
        };
        let Some(val_end) = after.find("</parameter>") else {
            break;
        };
        // Guard against an inverted range (open tag missing its `>` → first `>` is inside
        // `</parameter>`, so `gt+1 > val_end`): `.get` returns None instead of panicking.
        let Some(raw) = after.get(gt + 1..val_end).map(str::trim) else {
            break;
        };
        args.insert(key, coerce(raw));
        rest = &after[val_end + "</parameter>".len()..];
    }
    args
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

/// Repair a structured tool call's arguments when the provider adapter (genai) failed to parse
/// them as JSON and fell back to the raw accumulated string — e.g. a streaming reassembly glitch
/// (often via an OpenRouter-proxied model) that dropped or duplicated a chunk of the
/// `tool_calls[].function.arguments` stream before genai's own end-of-stream `serde_json::from_str`
/// ran. Passes through anything that's already a JSON object (the overwhelmingly common case)
/// untouched.
///
/// ALWAYS returns a JSON object, never a bare string — this is load-bearing, not cosmetic: the
/// repaired (or, worst case, empty) object is what gets stored on the `ToolCall` and replayed
/// verbatim to the provider in every subsequent request in the conversation. A raw invalid JSON
/// value sitting in `tool_calls[].function.arguments` on replay gets flatly rejected by a strict
/// provider's own request-shape validation (observed as OpenRouter's generic `"Provider returned
/// error"` 400 wrapper) — which Forge's mesh correctly does NOT fail over on, since a genuinely
/// malformed request would fail identically on any model. Left unrepaired, one streaming glitch on
/// one tool call permanently poisons the rest of that turn (and any later turn that replays the
/// same history): every subsequent request 400s regardless of which model handles it, with no
/// error-classification path that fixes it. Repairing at the source, before the bad value is ever
/// stored, is the only place that actually breaks the poison loop.
///
/// Two repair strategies, in order:
/// 1. Trim stray characters surrounding an otherwise-intact object (`start..=end` between the
///    first `{` and last `}`) and retry parsing — recovers e.g. a duplicated trailing byte.
/// 2. A best-effort scrape of `"key": value` fragments from whatever text survived — recovers
///    partial data even when a leading chunk (including the opening `{`) was dropped entirely, as
///    in the field report this was written for. Whatever a real required key IS still legible
///    survives; a genuinely missing key is then reported to the model as a normal, actionable
///    "missing required field" error on the next step instead of a generic "must be a JSON object"
///    that gives it nothing to act on.
///
/// If neither recovers anything, falls back to an empty object — always object-shaped, never worse
/// than what genai handed back.
pub fn repair_malformed_args(raw: Value) -> Value {
    let Value::String(s) = raw else { return raw };

    if let (Some(start), Some(end)) = (s.find('{'), s.rfind('}')) {
        if start <= end {
            if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(&s[start..=end]) {
                return v;
            }
        }
    }

    Value::Object(scrape_key_value_fragments(&s))
}

/// Best-effort extraction of `"key": value` pairs from a string that isn't valid JSON as a whole —
/// used when a tool call's arguments were truncated or corrupted badly enough that trimming stray
/// surrounding characters isn't enough to make the whole thing parse. Tolerant of a missing
/// opening brace/key (the common case: a dropped leading stream chunk), of a stray leftover quote
/// character right at the start (e.g. the orphaned closing `"` of a value whose own key got
/// dropped with it), and of trailing junk after the last recognizable value. Scans for a `"` and
/// only commits to it as a key if a `:` genuinely follows (skipping whitespace) — anything else is
/// noise, and the scan resumes from just past that quote rather than losing the rest of the
/// string. Never panics on adversarial input — every slice comes from `.find()` positions (always
/// on a UTF-8 char boundary since `"`/`:`/`,`/`}` are all single-byte ASCII) with an early
/// skip-and-continue on anything that doesn't match the expected shape.
fn scrape_key_value_fragments(s: &str) -> Map<String, Value> {
    let mut obj = Map::new();
    let mut pos = 0usize;
    while let Some(rel) = s[pos..].find('"') {
        let key_start = pos + rel + 1;
        let Some(rel_end) = s[key_start..].find('"') else {
            break; // an unterminated quote — nothing usable left
        };
        let key_end = key_start + rel_end;
        let key = &s[key_start..key_end];
        let after_key = s[key_end + 1..].trim_start();

        if key.is_empty() || !after_key.starts_with(':') {
            // Either an empty "key" or no `:` follows — this quote wasn't a real key (e.g. a
            // stray leftover value-closing quote). Resume scanning right after it, not past it.
            pos = key_start;
            continue;
        }
        let val_part = after_key[1..].trim_start();

        let (value, consumed_to) = if let Some(vs) = val_part.strip_prefix('"') {
            match vs.find('"') {
                Some(vend) => (
                    Value::String(vs[..vend].to_string()),
                    s.len() - vs[vend + 1..].len(),
                ),
                None => {
                    pos = key_start;
                    continue;
                }
            }
        } else {
            let vend = val_part.find([',', '}']).unwrap_or(val_part.len());
            let tok = val_part[..vend].trim();
            if tok.is_empty() {
                pos = key_start;
                continue;
            }
            let value = serde_json::from_str::<Value>(tok)
                .unwrap_or_else(|_| Value::String(tok.to_string()));
            (value, s.len() - val_part[vend..].len())
        };
        obj.insert(key.to_string(), value);
        pos = consumed_to;
    }
    obj
}

/// Cheap detector for forge-core's honest-failure guard: does this text contain an un-executed
/// tool call? (Same markers `recover_text_tool_calls` keys on, plus the bare `default_api:` form.)
pub fn looks_like_unexecuted_tool_call(content: &str) -> bool {
    content.contains("<invoke")
        || content.contains("<tool_call")
        || content.contains("<function=")
        || content.contains("default_api:")
        || content.contains("default_api.")
        || content.contains("{\"name\"")
        || content.contains("```json")
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
    fn nameless_parameter_tag_does_not_drop_the_whole_call() {
        // A `<parameter>` with no name must be SKIPPED, not abort the recovered call — otherwise one
        // malformed param drops the tool call and the bridge phantom-succeeds.
        let text = "<invoke name=\"shell\">\
            <parameter>no_name_here</parameter>\
            <parameter name=\"command\">ls -la</parameter></invoke>";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "call survives one nameless parameter");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args["command"], "ls -la");
        assert!(!cleaned.contains("<invoke"));
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
    fn recovers_function_with_parameter_subtags() {
        // qwen3-coder mixed format observed live on failover: a <function=…> tag whose body is
        // NOT json but Anthropic-style <parameter=…> sub-tags. Before the fix this recovered the
        // name but EMPTY args → an empty `shell({})` no-op that looped until timeout.
        let text = "<function=shell>\n<parameter=command>\nls -la src/\n</parameter>\n</function>";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1, "should recover one call");
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[0].args["command"], "ls -la src/");
        assert!(
            !cleaned.contains("<function="),
            "tag must be stripped: {cleaned:?}"
        );
    }

    #[test]
    fn recovers_function_with_quoted_parameter_name() {
        // The other parameter spelling: <parameter name="key"> inside a <function=…> tag.
        let text =
            "<function=search><parameter name=\"query\">resolve_redirects</parameter></function>";
        let (calls, _) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "search");
        assert_eq!(calls[0].args["query"], "resolve_redirects");
    }

    #[test]
    fn malformed_parameter_tag_does_not_panic() {
        // The exact inverted-bounds case: an `<parameter>` open tag missing its `>`, so the first
        // `>` lands inside `</parameter>` (gt > val_end). Pre-fix this panicked the slice and crashed
        // the whole turn on untrusted model output. Both recovery entry points must return cleanly.
        for input in [
            "<invoke name=\"T\"><parameter name=\"x\"</parameter> trailing>",
            "<function=shell><parameter=cmd</parameter> ls>",
            "<invoke name=\"R\"><parameter name=\"a\"</parameter><parameter name=\"b\">ok</parameter></invoke>",
        ] {
            // Reaching here without unwinding IS the assertion (pre-fix this panicked).
            let (_calls, _cleaned) = recover_text_tool_calls(input);
        }
    }

    #[test]
    fn plain_prose_is_untouched() {
        let text = "Here is the plan. I will run the build and report back.";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn recovers_bare_json_tool_call() {
        let text = "{\"name\":\"update_tasks\",\"arguments\":{\"tasks\":[{\"title\":\"Run Ruff Check\",\"status\":\"in_progress\"}]}}";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "update_tasks");
        assert_eq!(calls[0].args["tasks"][0]["title"], "Run Ruff Check");
        assert!(
            cleaned.is_empty(),
            "cleaned should be empty when fully parsed"
        );
    }

    #[test]
    fn recovers_fenced_json_tool_call() {
        let text = "```json\n{\"name\":\"update_tasks\",\"arguments\":{\"tasks\":[]}}\n```";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "update_tasks");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn recovers_json_array_tool_calls() {
        let text = "[{\"name\":\"shell\",\"arguments\":{\"command\":\"ls\"}}, {\"name\":\"read_file\",\"arguments\":{\"path\":\"x.rs\"}}]";
        let (calls, cleaned) = recover_text_tool_calls(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "shell");
        assert_eq!(calls[1].name, "read_file");
        assert!(cleaned.is_empty());
    }

    #[test]
    fn ignores_prose_with_json_example() {
        let text = "Here is an example of a JSON object:\n```json\n{\"name\":\"example\",\"arguments\":{}}\n```\nDo not run this.";
        let (calls, cleaned) = recover_text_tool_calls(text);
        // Because it's not *just* the JSON, it shouldn't be parsed as a tool call.
        assert!(calls.is_empty());
        assert_eq!(cleaned, text);
    }

    #[test]
    fn detector_flags_bare_json() {
        assert!(looks_like_unexecuted_tool_call(
            "{\"name\":\"update_tasks\"}"
        ));
        assert!(looks_like_unexecuted_tool_call(
            "```json\n{\"name\":\"x\"}\n```"
        ));
    }

    #[test]
    fn detector_flags_textual_calls_only() {
        assert!(looks_like_unexecuted_tool_call("<invoke name=\"x\">"));
        assert!(looks_like_unexecuted_tool_call(
            "default_api:update_tasks(...)"
        ));
        assert!(!looks_like_unexecuted_tool_call("a normal sentence"));
    }

    /// Deterministic adversarial fuzz: model output is UNTRUSTED text, and a panic in recovery
    /// crashes the whole turn (the worst failure mode — it can't even fail over). Assemble thousands
    /// of pathological strings from the fragments that have historically tripped parsers (unbalanced
    /// braces, truncated JSON, the real tool-call markers spliced mid-prose, control chars, deep
    /// nesting, huge repeats, lone surrogates-as-text) via a seeded LCG so the corpus is the same on
    /// every run / CI box, and assert the two entry points uphold their invariants on ALL of them:
    ///   1. neither panics (an unwind here = a crashed turn);
    ///   2. every recovered call has a non-empty name (a nameless call can't be dispatched — it would
    ///      vanish silently, the exact "phantom success" failure the recovery exists to prevent);
    ///   3. both functions are deterministic (same input → same output), since routing depends on it.
    #[test]
    fn recovery_never_panics_on_adversarial_input() {
        const FRAGMENTS: &[&str] = &[
            "{",
            "}",
            "[",
            "]",
            "\"name\"",
            "\"arguments\"",
            ":",
            ",",
            "\n",
            "  ",
            "<invoke name=\"x\">",
            "</invoke>",
            "<function=foo>",
            "{\"name\":\"update_tasks\"}",
            "```json",
            "```",
            "default_api:do_thing(",
            ")",
            "\\u0000",
            "\u{1f600}",
            "你好",
            "\t\r",
            "null",
            "true",
            "0",
            "-1e9",
            "\"\"",
            "tool_call",
            "</function>",
            // <parameter> fragments — combine into malformed tags (open tag missing its `>`) that
            // make `gt > val_end` and used to panic the slice in parse_invoke_span / parse_parameter_tags.
            "<parameter name=\"x\"",
            "<parameter=key",
            "<parameter name=\"x\">",
            "</parameter>",
            ">",
            "\\",
            "prose words here",
            "{{{{",
            "}}}}",
            "[[[[",
            "{\"name\":",
            "\"name\":\"\"",
        ];
        // Seeded LCG (Numerical Recipes constants) — no rng dep, identical corpus everywhere.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..5000 {
            let pieces = 1 + next() % 24;
            let mut s = String::new();
            for _ in 0..pieces {
                let frag = FRAGMENTS[next() % FRAGMENTS.len()];
                // Occasionally blow a fragment up to stress length/repeat handling.
                if next() % 17 == 0 {
                    s.push_str(&frag.repeat(1 + next() % 50));
                } else {
                    s.push_str(frag);
                }
            }
            // Invariant 1 (no panic) holds implicitly — any unwind fails the test.
            let (calls, _residual) = recover_text_tool_calls(&s);
            for c in &calls {
                assert!(
                    !c.name.is_empty(),
                    "recovered a nameless tool call from input: {s:?}"
                );
            }
            // Invariant 3: determinism.
            let (calls2, residual2) = recover_text_tool_calls(&s);
            assert_eq!(
                calls.len(),
                calls2.len(),
                "non-deterministic call count: {s:?}"
            );
            let _ = residual2;
            let flagged = looks_like_unexecuted_tool_call(&s);
            assert_eq!(
                flagged,
                looks_like_unexecuted_tool_call(&s),
                "non-deterministic detector: {s:?}"
            );
        }
    }

    #[test]
    fn repair_passes_through_an_already_valid_object_untouched() {
        let v = serde_json::json!({"query": "mesh", "path": "crates"});
        assert_eq!(repair_malformed_args(v.clone()), v);
    }

    #[test]
    fn repair_passes_through_non_string_non_object_unchanged() {
        // Only a String fallback is genai's malformed case; anything else (shouldn't happen, but
        // must never be *worsened*) is returned as-is.
        assert_eq!(repair_malformed_args(Value::Null), Value::Null);
        assert_eq!(
            repair_malformed_args(Value::Array(vec![])),
            Value::Array(vec![])
        );
    }

    #[test]
    fn repair_trims_stray_characters_around_an_intact_object() {
        // A duplicated trailing byte from a streaming reassembly glitch — the object itself is
        // otherwise complete and valid.
        let raw = Value::String(r#"{"query":"mesh","path":"crates"}}"#.to_string());
        let repaired = repair_malformed_args(raw);
        assert_eq!(repaired["query"], "mesh");
        assert_eq!(repaired["path"], "crates");
    }

    #[test]
    fn repair_recovers_fields_from_a_fragment_missing_its_opening_brace() {
        // The exact field-reported shape: a dropped leading chunk took the opening `{` and the
        // `query` key with it, leaving a fragment that starts mid-object.
        let raw = Value::String(r#"","path":"crates","context":3}"#.to_string());
        let repaired = repair_malformed_args(raw);
        assert!(repaired.is_object(), "must always repair to an object");
        assert_eq!(repaired["path"], "crates");
        assert_eq!(repaired["context"], 3);
        // `query` is genuinely gone — not fabricated — so a downstream required-field check still
        // correctly reports it missing instead of the call silently "succeeding" with bad data.
        assert!(repaired.get("query").is_none());
    }

    #[test]
    fn repair_falls_back_to_empty_object_on_pure_garbage() {
        let repaired = repair_malformed_args(Value::String("not json at all, no quotes".into()));
        assert_eq!(repaired, Value::Object(Map::new()));
    }

    #[test]
    fn repair_never_panics_on_adversarial_strings() {
        // Fuzz-style pass over ragged/truncated/binary-ish input — mirrors
        // `recovery_never_panics_on_adversarial_input`'s discipline for this new entry point.
        let mut state: u64 = 0xC0FFEE;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let fragments = [
            "\"", "{", "}", ":", ",", "\":", "\"a\"", "1", "true", "null", "🦀", "\\\"", "",
        ];
        for _ in 0..2000 {
            let len = (next() % 8) as usize;
            let mut s = String::new();
            for _ in 0..len {
                s.push_str(fragments[(next() % fragments.len() as u64) as usize]);
            }
            let repaired = repair_malformed_args(Value::String(s.clone()));
            assert!(repaired.is_object(), "non-object result for input: {s:?}");
        }
    }
}
