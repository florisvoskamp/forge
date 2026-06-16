//! A small, lenient YAML-frontmatter reader — just enough for command/skill metadata, with no
//! external YAML dependency. Supports `key: value`, inline lists `[a, b]`, and block lists
//! (`- item` lines). Unknown keys are kept; a line with no `:` (and not a list item) is an
//! error so the caller can skip a genuinely-malformed file.

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
enum FmValue {
    Scalar(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, Default)]
pub struct Frontmatter {
    map: BTreeMap<String, FmValue>,
}

impl Frontmatter {
    /// A scalar value for `key` (None if absent, empty, or a list).
    pub fn scalar(&self, key: &str) -> Option<String> {
        match self.map.get(key) {
            Some(FmValue::Scalar(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    /// A list value for `key`. A non-empty scalar is promoted to a one-element list; absent or
    /// empty keys yield an empty list.
    pub fn list(&self, key: &str) -> Vec<String> {
        match self.map.get(key) {
            Some(FmValue::List(v)) => v.clone(),
            Some(FmValue::Scalar(s)) if !s.is_empty() => vec![s.clone()],
            _ => Vec::new(),
        }
    }
}

/// Split a file into its `---`-fenced frontmatter (if any) and the body. A file without a valid
/// opening+closing fence yields `(None, whole_file)`.
pub fn split(raw: &str) -> (Option<&str>, &str) {
    let s = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let lead = s.len() - s.trim_start_matches(['\n', '\r', ' ', '\t']).len();
    let rest = &s[lead..];
    let first_line_len = rest.find('\n').map(|i| i + 1).unwrap_or(rest.len());
    if rest[..first_line_len].trim_end() != "---" {
        return (None, raw);
    }
    let fm_start = lead + first_line_len;
    let after = &s[fm_start..];
    let mut off = 0;
    for line in after.split_inclusive('\n') {
        if line.trim_end() == "---" {
            let fm = &s[fm_start..fm_start + off];
            let body_start = fm_start + off + line.len();
            let body = s.get(body_start..).unwrap_or("");
            return (Some(fm), body);
        }
        off += line.len();
    }
    (None, raw) // no closing fence → treat the whole file as body (lenient)
}

/// Parse a frontmatter block. Returns an error on a genuinely-malformed line so the caller can
/// skip the file and warn.
pub fn parse(text: &str) -> Result<Frontmatter, String> {
    let mut map = BTreeMap::new();
    let mut lines = text.lines().peekable();
    while let Some(line) = lines.next() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let (key, val) = match line.split_once(':') {
            Some((k, v)) => (k.trim().to_lowercase(), v.trim()),
            None => return Err(format!("malformed frontmatter line: {trimmed:?}")),
        };
        if key.is_empty() {
            return Err("empty frontmatter key".into());
        }
        if val.is_empty() {
            // Possibly a block list: following `- item` lines.
            let mut items = Vec::new();
            while let Some(peek) = lines.peek() {
                let pt = peek.trim();
                if let Some(item) = pt.strip_prefix('-') {
                    items.push(strip_quotes(item.trim()).to_string());
                    lines.next();
                } else {
                    break;
                }
            }
            if items.is_empty() {
                map.insert(key, FmValue::Scalar(String::new()));
            } else {
                map.insert(key, FmValue::List(items));
            }
        } else if val.starts_with('[') && val.ends_with(']') {
            let inner = &val[1..val.len() - 1];
            let items = inner
                .split(',')
                .map(|s| strip_quotes(s.trim()).to_string())
                .filter(|s| !s.is_empty())
                .collect();
            map.insert(key, FmValue::List(items));
        } else {
            map.insert(key, FmValue::Scalar(strip_quotes(val).to_string()));
        }
    }
    Ok(Frontmatter { map })
}

fn strip_quotes(s: &str) -> &str {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return &s[1..s.len() - 1];
        }
    }
    s
}
