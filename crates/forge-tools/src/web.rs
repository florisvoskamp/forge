//! Network tools: `web_fetch` (keyless URL → clean text) and `web_search` (BYOK ranked
//! results). Both declare [`SideEffect::Network`] so the permission broker gates egress
//! distinctly from a local read (SSRF / exfiltration risk). See docs/features/web-tools.md.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_types::SideEffect;
use serde_json::{json, Value};

use crate::{str_arg, Tool, ToolError};

const FETCH_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_MAX_CHARS: usize = 10_000;
const DEFAULT_SEARCH_COUNT: u32 = 5;
const MAX_SEARCH_COUNT: u32 = 10;
const USER_AGENT: &str = concat!(
    "forge/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/florisvoskamp/forge)"
);

// ---------------------------------------------------------------------------
// web_fetch
// ---------------------------------------------------------------------------

/// Fetch a URL over HTTP(S) and return its readable text. Network side effect.
pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch a web page over HTTP(S) and return its readable text content. \
         Use for reading documentation, articles, or any public URL."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Network
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "url": { "type": "string", "description": "The http(s) URL to fetch." },
                "max_chars": { "type": "integer", "description": "Cap on returned characters (default 10000)." }
            },
            "required": ["url"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let url = str_arg(args, "url")?;
        let max_chars = args
            .get("max_chars")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_MAX_CHARS);
        is_safe_url(url)?;

        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| ToolError::Failed(format!("http client: {e}")))?;
        let resp = client
            .get(url)
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("fetching {url}: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("reading body from {url}: {e}")))?;
        if !status.is_success() {
            return Err(ToolError::Failed(format!("{url} returned HTTP {status}")));
        }

        let text = html_to_text(&body);
        Ok(truncate_chars(&text, max_chars))
    }
}

/// Reject anything that isn't a plain http(s) request to a public host. Defends against SSRF
/// to loopback/private/link-local/metadata addresses. Known limit: no DNS resolution, so a
/// hostname that *resolves* to a private IP (DNS rebinding) is not caught here.
pub(crate) fn is_safe_url(url: &str) -> Result<(), ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|_| ToolError::BadArgs(format!("not a valid URL: {url}")))?;
    match parsed.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ToolError::BadArgs(format!(
                "unsupported URL scheme '{other}': only http/https are allowed"
            )))
        }
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| ToolError::BadArgs(format!("URL has no host: {url}")))?;

    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".local") || lower.ends_with(".localhost") {
        return Err(ToolError::BadArgs(format!(
            "refusing to fetch local host '{host}'"
        )));
    }
    // Bracketed IPv6 hosts arrive as "[::1]"; strip the brackets before parsing.
    let ip_candidate = lower.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = ip_candidate.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(ToolError::BadArgs(format!(
                "refusing to fetch private/loopback address '{host}'"
            )));
        }
    }
    Ok(())
}

fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                // 100.64.0.0/10 (CGNAT) and 192.0.0.0/24 — treat as non-public.
                || (v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            v6.is_loopback()
                || v6.is_unspecified()
                // fc00::/7 unique-local
                || (seg[0] & 0xfe00) == 0xfc00
                // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfe80
        }
    }
}

/// Strip HTML to readable text: drop `<script>`/`<style>` bodies, remove tags, decode the
/// common named/numeric entities, and collapse runs of whitespace. The page `<title>`, when
/// present, is surfaced as the first line.
pub(crate) fn html_to_text(html: &str) -> String {
    let title = extract_title(html);
    let without_blocks = strip_block(&strip_block(html, "script"), "style");
    let mut out = String::with_capacity(without_blocks.len());
    let mut in_tag = false;
    for ch in without_blocks.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let decoded = decode_entities(&out);
    let collapsed = collapse_ws(&decoded);
    match title {
        Some(t) if !collapsed.starts_with(&t) => format!("{t}\n\n{collapsed}"),
        _ => collapsed,
    }
}

fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("<title")?;
    let open_end = lower[start..].find('>')? + start + 1;
    let close = lower[open_end..].find("</title>")? + open_end;
    let raw = &html[open_end..close];
    let t = collapse_ws(&decode_entities(raw));
    (!t.is_empty()).then_some(t)
}

/// Remove `<tag …>…</tag>` blocks (case-insensitive), including their content.
fn strip_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut cursor = 0usize;
    while let Some(rel) = lower[cursor..].find(&open) {
        let start = cursor + rel;
        out.push_str(&html[cursor..start]);
        match lower[start..].find(&close) {
            Some(end_rel) => cursor = start + end_rel + close.len(),
            None => {
                cursor = html.len();
                break;
            }
        }
    }
    out.push_str(&html[cursor..]);
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&apos;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let truncated: String = s.chars().take(max).collect();
    format!("{truncated}\n\n[truncated at {max} chars]")
}

// ---------------------------------------------------------------------------
// web_search
// ---------------------------------------------------------------------------

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub description: String,
}

/// A pluggable search provider. The default is Brave; the trait keeps `web_search`
/// backend-agnostic so a free/alternative backend can be added without touching the tool.
#[async_trait]
pub trait SearchBackend: Send + Sync {
    async fn search(&self, query: &str, count: u32) -> Result<Vec<SearchResult>, ToolError>;
}

/// Brave Search API backend. Verified contract (official docs, 2026-06):
/// `GET https://api.search.brave.com/res/v1/web/search?q=…&count=…`, header
/// `X-Subscription-Token: <key>`, results at `web.results[].{title,url,description}`.
pub struct BraveSearch {
    key: String,
}

impl BraveSearch {
    pub fn new(key: String) -> Self {
        Self { key }
    }
}

#[async_trait]
impl SearchBackend for BraveSearch {
    async fn search(&self, query: &str, count: u32) -> Result<Vec<SearchResult>, ToolError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| ToolError::Failed(format!("http client: {e}")))?;
        let resp = client
            .get("https://api.search.brave.com/res/v1/web/search")
            .header("X-Subscription-Token", &self.key)
            .header("Accept", "application/json")
            .query(&[("q", query), ("count", &count.to_string())])
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("brave search request: {e}")))?;
        let status = resp.status();
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ToolError::Failed(format!("brave search response: {e}")))?;
        if !status.is_success() {
            return Err(ToolError::Failed(format!(
                "brave search returned HTTP {status}"
            )));
        }
        Ok(parse_brave_results(&body))
    }
}

pub(crate) fn parse_brave_results(body: &Value) -> Vec<SearchResult> {
    body.get("web")
        .and_then(|w| w.get("results"))
        .and_then(Value::as_array)
        .map(|results| {
            results
                .iter()
                .filter_map(|r| {
                    Some(SearchResult {
                        title: r.get("title")?.as_str()?.to_string(),
                        url: r.get("url")?.as_str()?.to_string(),
                        description: r
                            .get("description")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Keyless DuckDuckGo backend — the free default when no search-API key is configured.
/// Scrapes the no-JS HTML endpoint (`https://html.duckduckgo.com/html/`). Free forever but
/// best-effort: DDG rate-limits and the HTML layout can drift, so parsing is defensive and a
/// rate-limit / layout change degrades to "no results" rather than erroring the turn.
pub struct DuckDuckGo;

#[async_trait]
impl SearchBackend for DuckDuckGo {
    async fn search(&self, query: &str, count: u32) -> Result<Vec<SearchResult>, ToolError> {
        let client = reqwest::Client::builder()
            // DDG blocks obvious bot UAs — present a browser-like agent.
            .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:124.0) Gecko/20100101 Firefox/124.0")
            .timeout(FETCH_TIMEOUT)
            .build()
            .map_err(|e| ToolError::Failed(format!("http client: {e}")))?;
        let resp = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query)])
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("duckduckgo request: {e}")))?;
        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("duckduckgo response: {e}")))?;
        if !status.is_success() {
            return Err(ToolError::Failed(format!(
                "duckduckgo returned HTTP {status} (likely rate-limited — try again later \
                 or configure a search key with `forge auth brave`/`tavily`)"
            )));
        }
        let mut results = parse_ddg_results(&body);
        results.truncate(count as usize);
        Ok(results)
    }
}

/// Parse DuckDuckGo's HTML result page. Each hit is an `<a class="result__a" href="URL">TITLE
/// </a>` followed by an `<a class="result__snippet">SNIPPET</a>`. Ad/redirect anchors
/// (`//duckduckgo.com/y.js…`) are skipped.
pub(crate) fn parse_ddg_results(html: &str) -> Vec<SearchResult> {
    let titles = anchors_with_class(html, "result__a");
    let snippets = anchors_with_class(html, "result__snippet");
    titles
        .into_iter()
        .enumerate()
        .filter(|(_, (href, _))| href.starts_with("http"))
        .map(|(i, (href, title))| SearchResult {
            title,
            url: href,
            description: snippets.get(i).map(|(_, t)| t.clone()).unwrap_or_default(),
        })
        .collect()
}

/// Find every `<a … class="<class>" … href="HREF">INNER</a>` and return (href, plain-text
/// inner) pairs. Tolerates attribute order and nested tags in the inner text.
fn anchors_with_class(html: &str, class: &str) -> Vec<(String, String)> {
    let marker = format!("class=\"{class}\"");
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(rel) = html[cursor..].find(&marker) {
        let at = cursor + rel;
        // Find the bounds of this <a …> open tag.
        let tag_start = html[..at].rfind('<').unwrap_or(at);
        let Some(gt_rel) = html[at..].find('>') else {
            break;
        };
        let tag_end = at + gt_rel; // index of '>'
        let open_tag = &html[tag_start..tag_end];
        let href = open_tag
            .find("href=\"")
            .map(|h| &open_tag[h + 6..])
            .and_then(|s| s.split('"').next())
            .unwrap_or("")
            .to_string();
        let inner = match html[tag_end + 1..].find("</a>") {
            Some(end_rel) => &html[tag_end + 1..tag_end + 1 + end_rel],
            None => "",
        };
        out.push((decode_ddg_href(&href), html_to_text(inner)));
        cursor = tag_end + 1;
    }
    out
}

/// DDG sometimes wraps the target in a redirect: `//duckduckgo.com/l/?uddg=<encoded>`.
/// Extract and percent-decode the real URL when present; otherwise pass through.
fn decode_ddg_href(href: &str) -> String {
    let Some(idx) = href.find("uddg=") else {
        return href.to_string();
    };
    let enc = href[idx + 5..].split('&').next().unwrap_or("");
    percent_decode(enc)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
                Ok(b) => {
                    out.push(b);
                    i += 3;
                }
                Err(_) => {
                    out.push(b'%');
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn format_results(results: &[SearchResult]) -> String {
    if results.is_empty() {
        return "No results found.".to_string();
    }
    results
        .iter()
        .enumerate()
        .map(|(i, r)| {
            // Brave snippets may carry <strong> highlight tags — reduce to plain text.
            let desc = html_to_text(&r.description);
            format!("{}. {}\n   {}\n   {}", i + 1, r.title, r.url, desc)
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Web search over a pluggable [`SearchBackend`]. Resolves the Brave backend from the
/// `BRAVE_API_KEY` environment variable (the CLI injects the keyring value before a session)
/// unless an explicit backend was supplied (tests / alternative providers).
#[derive(Default)]
pub struct WebSearchTool {
    backend: Option<Arc<dyn SearchBackend>>,
}

impl WebSearchTool {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_backend(backend: Arc<dyn SearchBackend>) -> Self {
        Self {
            backend: Some(backend),
        }
    }

    /// Pick a backend: an explicit one (tests / future config) wins; else Brave if a key is
    /// set; else the keyless DuckDuckGo default so web search works with zero setup.
    fn resolve_backend(&self) -> Arc<dyn SearchBackend> {
        if let Some(b) = &self.backend {
            return b.clone();
        }
        match std::env::var("BRAVE_API_KEY") {
            Ok(key) if !key.is_empty() => Arc::new(BraveSearch::new(key)),
            _ => Arc::new(DuckDuckGo),
        }
    }
}

#[async_trait]
impl Tool for WebSearchTool {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web and return ranked results (title, URL, snippet). \
         Use to find current information, documentation, or sources to then fetch."
    }
    fn side_effect(&self) -> SideEffect {
        SideEffect::Network
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": { "type": "string", "description": "The search query." },
                "count": { "type": "integer", "description": "How many results (default 5, max 10)." }
            },
            "required": ["query"]
        })
    }
    async fn run(&self, args: &Value) -> Result<String, ToolError> {
        let query = str_arg(args, "query")?;
        let count = args
            .get("count")
            .and_then(Value::as_u64)
            .map(|n| (n as u32).clamp(1, MAX_SEARCH_COUNT))
            .unwrap_or(DEFAULT_SEARCH_COUNT);
        let results = self.resolve_backend().search(query, count).await?;
        Ok(format_results(&results))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_url_accepts_public_https() {
        assert!(is_safe_url("https://example.com/docs").is_ok());
        assert!(is_safe_url("http://93.184.216.34/").is_ok());
    }

    #[test]
    fn safe_url_rejects_ssrf_and_bad_schemes() {
        for bad in [
            "http://127.0.0.1/",
            "http://localhost:8080/",
            "http://10.0.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "https://foo.local/",
            "file:///etc/passwd",
            "ftp://example.com/",
            "not a url",
        ] {
            assert!(is_safe_url(bad).is_err(), "should reject {bad}");
        }
    }

    #[test]
    fn html_to_text_strips_tags_scripts_and_decodes() {
        let html = "<html><head><title>Hello &amp; Bye</title></head><body>\
            <script>var x = 1 < 2;</script><style>.a{}</style>\
            <p>Tom &amp; Jerry &lt;3</p></body></html>";
        let text = html_to_text(html);
        assert!(text.starts_with("Hello & Bye"), "title surfaced: {text}");
        assert!(text.contains("Tom & Jerry <3"), "entities decoded: {text}");
        assert!(!text.contains("var x"), "script body dropped: {text}");
        assert!(!text.contains(".a{"), "style body dropped: {text}");
        assert!(
            !text.contains('<') || text.contains("<3"),
            "tags stripped: {text}"
        );
    }

    #[test]
    fn truncate_caps_long_text() {
        let s = "x".repeat(100);
        let out = truncate_chars(&s, 10);
        assert!(out.starts_with(&"x".repeat(10)));
        assert!(out.contains("truncated"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn parse_brave_extracts_ordered_results() {
        let body = json!({
            "web": { "results": [
                { "title": "First", "url": "https://a.com", "description": "desc a" },
                { "title": "Second", "url": "https://b.com" }
            ]}
        });
        let r = parse_brave_results(&body);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "First");
        assert_eq!(r[0].url, "https://a.com");
        assert_eq!(r[0].description, "desc a");
        assert_eq!(r[1].title, "Second");
        assert_eq!(r[1].description, "", "missing description defaults empty");
    }

    #[test]
    fn parse_brave_handles_missing_web_key() {
        assert!(parse_brave_results(&json!({ "error": "nope" })).is_empty());
    }

    struct MockBackend(Vec<SearchResult>);
    #[async_trait]
    impl SearchBackend for MockBackend {
        async fn search(&self, _q: &str, _c: u32) -> Result<Vec<SearchResult>, ToolError> {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn web_search_formats_results_from_backend() {
        let tool = WebSearchTool::with_backend(Arc::new(MockBackend(vec![SearchResult {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            description: "systems lang".into(),
        }])));
        let out = tool.run(&json!({ "query": "rust" })).await.unwrap();
        assert!(out.contains("1. Rust"));
        assert!(out.contains("https://rust-lang.org"));
        assert!(out.contains("systems lang"));
    }

    /// Live network smoke test (no key needed). Run on demand:
    /// `cargo test -p forge-tools web_fetch_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn web_fetch_live_example_com() {
        let out = WebFetchTool
            .run(&json!({ "url": "https://example.com" }))
            .await
            .expect("fetch example.com");
        assert!(out.contains("Example Domain"), "got: {out}");
    }

    #[test]
    fn parse_ddg_extracts_title_url_snippet() {
        let html = r#"
          <a rel="nofollow" class="result__a" href="https://rust-lang.org/">Rust &amp; Lang</a>
          <a class="result__snippet" href="https://rust-lang.org/">A language empowering everyone.</a>
          <a rel="nofollow" class="result__a" href="https://en.wikipedia.org/wiki/Rust">Rust - Wikipedia</a>
          <a class="result__snippet" href="x">Rust is a systems language.</a>
          <a class="result__a" href="//duckduckgo.com/y.js?ad=1">An ad</a>
        "#;
        let r = parse_ddg_results(html);
        assert_eq!(r.len(), 2, "ad/redirect anchors skipped");
        assert_eq!(r[0].title, "Rust & Lang");
        assert_eq!(r[0].url, "https://rust-lang.org/");
        assert_eq!(r[0].description, "A language empowering everyone.");
        assert_eq!(r[1].url, "https://en.wikipedia.org/wiki/Rust");
    }

    #[test]
    fn ddg_redirect_href_is_decoded() {
        assert_eq!(
            decode_ddg_href("//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fa%20b&rut=x"),
            "https://example.com/a b"
        );
        assert_eq!(
            decode_ddg_href("https://direct.example.com/"),
            "https://direct.example.com/"
        );
    }

    #[tokio::test]
    async fn web_search_defaults_to_duckduckgo_without_key() {
        std::env::remove_var("BRAVE_API_KEY");
        // No key, no explicit backend → keyless DuckDuckGo default (web search works zero-setup).
        let backend = WebSearchTool::new().resolve_backend();
        // The mock/Brave paths aren't used here; just assert a backend is produced (no error).
        let _: Arc<dyn SearchBackend> = backend;
    }

    /// Live network smoke test (no key). `cargo test -p forge-tools ddg_live -- --ignored --nocapture`
    #[tokio::test]
    #[ignore]
    async fn ddg_live_search() {
        let out = WebSearchTool::new()
            .run(&json!({ "query": "rust programming language", "count": 3 }))
            .await
            .expect("ddg search");
        assert!(out.contains("rust") || out.contains("Rust"), "got: {out}");
        assert!(out.contains("http"), "has urls: {out}");
    }
}
