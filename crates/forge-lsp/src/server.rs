use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout};

use crate::rpc::{read_msg, write_msg};
use crate::types::{Diagnostic, DiagnosticSeverity};

pub struct LspServer {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl LspServer {
    pub async fn spawn(cmd: &str, args: &[String]) -> std::io::Result<Self> {
        let mut child = tokio::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("stdin piped");
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Ok(Self {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
        })
    }

    fn new_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub async fn initialize(&mut self, root_uri: &str) -> std::io::Result<()> {
        let id = self.new_id();
        let req = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "publishDiagnostics": {}
                    }
                }
            }
        });
        write_msg(&mut self.stdin, &req).await?;
        while let Some(msg) = read_msg(&mut self.stdout).await {
            if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                break;
            }
        }
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "initialized",
            "params": {}
        });
        write_msg(&mut self.stdin, &notif).await?;
        Ok(())
    }

    pub async fn did_open(&mut self, uri: &str, lang: &str, text: &str) -> std::io::Result<()> {
        let notif = json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": {
                "textDocument": {
                    "uri": uri,
                    "languageId": lang,
                    "version": 1,
                    "text": text
                }
            }
        });
        write_msg(&mut self.stdin, &notif).await
    }

    pub async fn collect_diagnostics(&mut self, uri: &str, timeout: Duration) -> Vec<Diagnostic> {
        let mut diags = Vec::new();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let msg = match tokio::time::timeout(remaining, read_msg(&mut self.stdout)).await {
                Ok(Some(m)) => m,
                _ => break,
            };
            if msg.get("method").and_then(|v| v.as_str()) == Some("textDocument/publishDiagnostics")
            {
                if let Some(params) = msg.get("params") {
                    let msg_uri = params.get("uri").and_then(|v| v.as_str()).unwrap_or("");
                    if msg_uri == uri {
                        if let Some(arr) = params.get("diagnostics").and_then(|v| v.as_array()) {
                            diags = arr.iter().filter_map(parse_diagnostic).collect();
                        }
                        break;
                    }
                }
            }
        }
        diags
    }
}

fn parse_diagnostic(v: &Value) -> Option<Diagnostic> {
    let message = v.get("message")?.as_str()?.to_string();
    let range = v.get("range")?;
    let start = range.get("start")?;
    let line = start.get("line")?.as_u64()? as u32;
    let character = start.get("character")?.as_u64()? as u32;
    let severity = v
        .get("severity")
        .and_then(|s| s.as_u64())
        .map(DiagnosticSeverity::from_lsp_int)
        .unwrap_or(DiagnosticSeverity::Error);
    let code = v.get("code").and_then(|c| {
        if let Some(s) = c.as_str() {
            Some(s.to_string())
        } else {
            c.as_u64().map(|n| n.to_string())
        }
    });
    Some(Diagnostic {
        severity,
        message,
        line,
        character,
        code,
    })
}
