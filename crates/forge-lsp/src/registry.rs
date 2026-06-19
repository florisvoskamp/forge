use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::warn;

use forge_config::LspConfig;

use crate::server::LspServer;
use crate::types::Diagnostic;

pub struct LspRegistry {
    config: LspConfig,
    servers: Mutex<HashMap<(String, PathBuf), LspServer>>,
}

impl LspRegistry {
    pub fn from_config(config: &LspConfig) -> Self {
        Self {
            config: config.clone(),
            servers: Mutex::new(HashMap::new()),
        }
    }

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

        let mut servers = self.servers.lock().await;
        if !servers.contains_key(&key) {
            match LspServer::spawn(&cmd, &args).await {
                Ok(mut srv) => {
                    if let Err(e) = srv.initialize(&root_uri).await {
                        warn!("lsp: initialize failed for {lang}: {e}");
                        return vec![];
                    }
                    servers.insert(key.clone(), srv);
                }
                Err(e) => {
                    warn!("lsp: spawn failed for {lang} ({cmd}): {e}");
                    return vec![];
                }
            }
        }
        let server = servers.get_mut(&key).unwrap();

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
    format!("file://{}", p.display())
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
