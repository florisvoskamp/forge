use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::warn;

use forge_config::LspConfig;

use crate::server::LspServer;
use crate::types::Diagnostic;

/// A lazily-initialized language-server slot for one `(language, repo-root)` pair, behind its
/// own lock so a hung server only blocks callers waiting on that same pair.
type ServerSlot = Arc<Mutex<Option<LspServer>>>;

/// Owns the live language-server processes and routes a file to the right one.
///
/// One server is spawned lazily per `(language, repo-root)` and reused across calls (kept in
/// `servers`), so repeated diagnostics on the same project don't pay startup each time.
///
/// Each entry has its own `Mutex` so a stalled/hung server for one `(language, repo-root)` only
/// blocks callers waiting on *that* entry; the outer `servers` lock is only ever held briefly to
/// look up or insert the entry itself, never across the slow spawn/initialize/diagnostics work.
pub struct LspRegistry {
    config: LspConfig,
    servers: Mutex<HashMap<(String, PathBuf), ServerSlot>>,
}

impl LspRegistry {
    /// Build a registry from the user's `[lsp]` config (which servers are enabled, their commands).
    pub fn from_config(config: &LspConfig) -> Self {
        Self {
            config: config.clone(),
            servers: Mutex::new(HashMap::new()),
        }
    }

    /// Diagnostics for one file, or an empty vec if the language is unconfigured, the server binary
    /// isn't on PATH, or it doesn't answer within `timeout`. Never errors — best-effort by design.
    pub async fn diagnostics_for(&self, abs_path: &Path, timeout: Duration) -> Vec<Diagnostic> {
        let Some(lang) = lang_from_ext(abs_path) else {
            return vec![];
        };
        let Some(root) = repo_root(abs_path) else {
            return vec![];
        };
        let Some((cmd, args)) = self.server_for_lang(lang) else {
            return vec![];
        };
        if which(&cmd).is_none() {
            return vec![];
        }

        let text = match std::fs::read_to_string(abs_path) {
            Ok(t) => t,
            Err(e) => {
                warn!("lsp: cannot read {}: {e}", abs_path.display());
                return vec![];
            }
        };

        let uri = path_to_uri(abs_path);
        let root_uri = path_to_uri(&root);
        let key = (lang.to_string(), root.clone());

        // Only the map lookup/insert happens under the registry-wide lock; the entry's own
        // lock (acquired below, after this guard is dropped) is what serializes the actual
        // spawn/initialize/diagnostics work for that one (language, repo-root) pair.
        let entry = {
            let mut servers = self.servers.lock().await;
            servers
                .entry(key)
                .or_insert_with(|| Arc::new(Mutex::new(None)))
                .clone()
        };

        let mut slot = entry.lock().await;
        if slot.is_none() {
            match LspServer::spawn(&cmd, &args).await {
                Ok(mut srv) => {
                    if let Err(e) = srv.initialize(&root_uri, timeout).await {
                        warn!("lsp: initialize failed for {lang}: {e}");
                        return vec![];
                    }
                    *slot = Some(srv);
                }
                Err(e) => {
                    warn!("lsp: spawn failed for {lang} ({cmd}): {e}");
                    return vec![];
                }
            }
        }
        let server = slot.as_mut().unwrap();

        if let Err(e) = server.did_open(&uri, lang, &text).await {
            warn!("lsp: did_open failed: {e}");
            return vec![];
        }
        server.collect_diagnostics(&uri, timeout).await
    }

    fn server_for_lang(&self, lang: &str) -> Option<(String, Vec<String>)> {
        if let Some(entry) = self.config.servers.get(lang) {
            return Some((entry.command.clone(), entry.args.clone()));
        }
        match lang {
            "rust" => Some(("rust-analyzer".to_string(), vec![])),
            "typescript" | "javascript" => Some((
                "typescript-language-server".to_string(),
                vec!["--stdio".to_string()],
            )),
            "python" => Some((
                "pyright-langserver".to_string(),
                vec!["--stdio".to_string()],
            )),
            "go" => Some(("gopls".to_string(), vec![])),
            _ => None,
        }
    }
}

/// Map a file extension to the language key used to look up its server (`None` = unsupported).
pub fn lang_from_ext(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "ts" | "tsx" => Some("typescript"),
        "js" | "jsx" => Some("javascript"),
        "py" => Some("python"),
        "go" => Some("go"),
        _ => None,
    }
}

/// Walk up from `path` to the nearest project root (a dir holding `Cargo.toml`, `package.json`,
/// `pyproject.toml`, `go.mod`, or `.git`) — the directory the language server is rooted at.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let mut dir = path.parent()?;
    loop {
        for marker in &[
            "Cargo.toml",
            "package.json",
            "pyproject.toml",
            "go.mod",
            ".git",
        ] {
            if dir.join(marker).exists() {
                return Some(dir.to_path_buf());
            }
        }
        dir = dir.parent()?;
    }
}

/// Resolve a server command to an executable path (absolute path as-is, else searched on `PATH`).
pub fn which(cmd: &str) -> Option<PathBuf> {
    let p = Path::new(cmd);
    if p.is_absolute() {
        return p.exists().then(|| p.to_path_buf());
    }
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(cmd);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

fn path_to_uri(path: &Path) -> String {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    // RFC 8089 file URIs use forward slashes and a leading `/` before the path. On Unix
    // MAIN_SEPARATOR is `/` so this is a no-op; on Windows it turns `C:\a\b` into `/C:/a/b`,
    // yielding `file:///C:/a/b` instead of the malformed `file://C:\a\b`.
    let mut s = p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    if !s.starts_with('/') {
        s.insert(0, '/');
    }
    format!("file://{}", percent_encode_path(&s))
}

/// Percent-encode a URI path per RFC 3986, leaving `/` (segment separator) and `:` (needed for
/// Windows drive letters, e.g. `/C:/...`) unescaped. Without this, a path containing a space or
/// other reserved character never matches the (typically percent-encoded) URI a language server
/// echoes back in `publishDiagnostics`, silently dropping diagnostics for that file forever.
fn percent_encode_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn lang_from_ext_table() {
        assert_eq!(lang_from_ext(Path::new("foo.rs")), Some("rust"));
        assert_eq!(lang_from_ext(Path::new("foo.ts")), Some("typescript"));
        assert_eq!(lang_from_ext(Path::new("foo.tsx")), Some("typescript"));
        assert_eq!(lang_from_ext(Path::new("foo.js")), Some("javascript"));
        assert_eq!(lang_from_ext(Path::new("foo.jsx")), Some("javascript"));
        assert_eq!(lang_from_ext(Path::new("foo.py")), Some("python"));
        assert_eq!(lang_from_ext(Path::new("foo.go")), Some("go"));
        assert_eq!(lang_from_ext(Path::new("foo.txt")), None);
        assert_eq!(lang_from_ext(Path::new("noext")), None);
    }

    #[test]
    fn repo_root_finds_cargo_toml() {
        let dir = TempDir::new().unwrap();
        let cargo = dir.path().join("Cargo.toml");
        fs::write(&cargo, "[package]").unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        let file = src.join("lib.rs");
        fs::write(&file, "").unwrap();
        let found = repo_root(&file).unwrap();
        assert_eq!(found, dir.path());
    }

    fn empty_config() -> LspConfig {
        LspConfig {
            enabled: true,
            timeout_ms: 100,
            servers: std::collections::HashMap::new(),
        }
    }

    #[test]
    fn server_for_lang_built_in_defaults() {
        let reg = LspRegistry::from_config(&empty_config());
        assert_eq!(
            reg.server_for_lang("rust"),
            Some(("rust-analyzer".to_string(), vec![]))
        );
        assert_eq!(
            reg.server_for_lang("typescript"),
            Some((
                "typescript-language-server".to_string(),
                vec!["--stdio".to_string()]
            ))
        );
        // typescript and javascript share the same server.
        assert_eq!(
            reg.server_for_lang("javascript"),
            reg.server_for_lang("typescript")
        );
        assert_eq!(
            reg.server_for_lang("python"),
            Some((
                "pyright-langserver".to_string(),
                vec!["--stdio".to_string()]
            ))
        );
        assert_eq!(
            reg.server_for_lang("go"),
            Some(("gopls".to_string(), vec![]))
        );
        assert_eq!(reg.server_for_lang("cobol"), None);
    }

    #[test]
    fn server_for_lang_config_overrides_default() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "rust".to_string(),
            forge_config::LspServerEntry {
                command: "my-analyzer".to_string(),
                args: vec!["--flag".to_string()],
            },
        );
        let cfg = LspConfig {
            enabled: true,
            timeout_ms: 100,
            servers,
        };
        let reg = LspRegistry::from_config(&cfg);
        assert_eq!(
            reg.server_for_lang("rust"),
            Some(("my-analyzer".to_string(), vec!["--flag".to_string()]))
        );
    }

    #[test]
    fn config_can_add_a_new_language() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "ruby".to_string(),
            forge_config::LspServerEntry {
                command: "solargraph".to_string(),
                args: vec!["stdio".to_string()],
            },
        );
        let cfg = LspConfig {
            enabled: true,
            timeout_ms: 100,
            servers,
        };
        let reg = LspRegistry::from_config(&cfg);
        assert_eq!(
            reg.server_for_lang("ruby"),
            Some(("solargraph".to_string(), vec!["stdio".to_string()]))
        );
    }

    #[test]
    fn repo_root_finds_git_dir() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join(".git")).unwrap();
        let nested = dir.path().join("a").join("b");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("x.rs");
        fs::write(&file, "").unwrap();
        assert_eq!(repo_root(&file).unwrap(), dir.path());
    }

    #[test]
    fn repo_root_none_without_marker() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("loose.rs");
        fs::write(&file, "").unwrap();
        // A bare TempDir has no project marker above it within the temp tree.
        assert!(repo_root(&file).is_none());
    }

    #[test]
    fn repo_root_picks_nearest_ancestor() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("Cargo.toml"), "[package]").unwrap();
        let inner = dir.path().join("sub");
        fs::create_dir(&inner).unwrap();
        fs::write(inner.join("package.json"), "{}").unwrap();
        let file = inner.join("app.ts");
        fs::write(&file, "").unwrap();
        // Walks up only to the closest marker, not the outer Cargo.toml.
        assert_eq!(repo_root(&file).unwrap(), inner);
    }

    #[test]
    fn which_resolves_absolute_path_when_present() {
        let dir = TempDir::new().unwrap();
        let bin = dir.path().join("fake-lsp");
        fs::write(&bin, "#!/bin/sh\n").unwrap();
        assert_eq!(which(bin.to_str().unwrap()).unwrap(), bin);
    }

    #[test]
    fn which_absolute_path_missing_is_none() {
        let dir = TempDir::new().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(which(missing.to_str().unwrap()).is_none());
    }

    #[test]
    fn which_bare_nonexistent_command_is_none() {
        assert!(which("__forge_definitely_not_a_real_binary_zzz__").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn path_to_uri_absolute_has_file_scheme() {
        let uri = path_to_uri(Path::new("/home/user/project/main.rs"));
        assert_eq!(uri, "file:///home/user/project/main.rs");
    }

    #[cfg(windows)]
    #[test]
    fn path_to_uri_absolute_has_file_scheme() {
        // RFC 8089: a Windows drive path becomes file:///C:/... with forward slashes and a
        // leading slash before the drive letter, never file://C:\... .
        let uri = path_to_uri(Path::new(r"C:\home\user\main.rs"));
        assert_eq!(uri, "file:///C:/home/user/main.rs");
    }

    #[cfg(unix)]
    #[test]
    fn path_to_uri_percent_encodes_reserved_characters() {
        // Spaces and other reserved/unsafe URI characters must be percent-encoded, or the
        // language server's own (encoded) echoed URI in publishDiagnostics never matches ours.
        let uri = path_to_uri(Path::new("/home/user/My Project/a#b%c?.rs"));
        assert_eq!(uri, "file:///home/user/My%20Project/a%23b%25c%3F.rs");
    }

    #[test]
    fn path_to_uri_relative_is_anchored_to_absolute() {
        let uri = path_to_uri(Path::new("rel/file.rs"));
        assert!(uri.starts_with("file:///"), "uri was: {uri}");
        // Output URIs always use forward slashes, regardless of the host separator.
        assert!(uri.ends_with("rel/file.rs"), "uri was: {uri}");
        assert!(
            !uri.contains('\\'),
            "uri must not contain backslashes: {uri}"
        );
    }

    #[tokio::test]
    async fn diagnostics_for_returns_empty_when_no_lang() {
        let cfg = LspConfig {
            enabled: true,
            timeout_ms: 100,
            servers: std::collections::HashMap::new(),
        };
        let reg = LspRegistry::from_config(&cfg);
        let tmp = TempDir::new().unwrap();
        let f = tmp.path().join("test.txt");
        fs::write(&f, "hello").unwrap();
        let diags = reg.diagnostics_for(&f, Duration::from_millis(100)).await;
        assert!(diags.is_empty());
    }

    #[tokio::test]
    async fn diagnostics_for_returns_empty_when_binary_not_found() {
        let mut servers = std::collections::HashMap::new();
        servers.insert(
            "rust".to_string(),
            forge_config::LspServerEntry {
                command: "__forge_lsp_nonexistent_binary_xyz__".to_string(),
                args: vec![],
            },
        );
        let cfg = LspConfig {
            enabled: true,
            timeout_ms: 100,
            servers,
        };
        let reg = LspRegistry::from_config(&cfg);
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("Cargo.toml"), "[package]").unwrap();
        let f = tmp.path().join("main.rs");
        fs::write(&f, "fn main() {}").unwrap();
        let diags = reg.diagnostics_for(&f, Duration::from_millis(100)).await;
        assert!(diags.is_empty());
    }
}
