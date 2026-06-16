//! Prompt-template substitution: `$ARGUMENTS` (the raw remainder of the line), `$1..$9`
//! (positional args), and `$NAME` (a declared named arg). Unknown `$tokens` are left verbatim
//! so a body can contain literal `$` text. No shell parsing — values substitute as-is.

/// Expand `body` using positional args, named args (`(name, value)`), and `raw` (the verbatim
/// text after the command name, for `$ARGUMENTS`).
pub fn expand(body: &str, positional: &[&str], named: &[(String, String)], raw: &str) -> String {
    let chars: Vec<char> = body.chars().collect();
    let mut out = String::with_capacity(body.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut j = start;
        while j < chars.len() && (chars[j].is_alphanumeric() || chars[j] == '_') {
            j += 1;
        }
        let token: String = chars[start..j].iter().collect();
        if token.is_empty() {
            out.push('$');
            i += 1;
            continue;
        }
        if token == "ARGUMENTS" {
            out.push_str(raw);
        } else if let Ok(n) = token.parse::<usize>() {
            if n >= 1 {
                out.push_str(positional.get(n - 1).copied().unwrap_or(""));
            } else {
                out.push('$');
                out.push_str(&token);
            }
        } else if let Some((_, v)) = named.iter().find(|(k, _)| *k == token) {
            out.push_str(v);
        } else {
            out.push('$');
            out.push_str(&token);
        }
        i = j;
    }
    out
}
