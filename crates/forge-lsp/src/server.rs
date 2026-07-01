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
            .kill_on_drop(true)
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

    pub async fn initialize(&mut self, root_uri: &str, timeout: Duration) -> std::io::Result<()> {
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
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "lsp: initialize response timed out",
                ));
            }
            match tokio::time::timeout(remaining, read_msg(&mut self.stdout)).await {
                Ok(Some(msg)) => {
                    if msg.get("id").and_then(|v| v.as_u64()) == Some(id) {
                        break;
                    }
                }
                Ok(None) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "lsp: server closed stdout during initialize",
                    ))
                }
                Err(_) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::TimedOut,
                        "lsp: initialize response timed out",
                    ))
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn full_diag() -> Value {
        json!({
            "message": "cannot find value `x`",
            "severity": 1,
            "code": "E0425",
            "range": {
                "start": {"line": 4, "character": 8},
                "end": {"line": 4, "character": 9}
            }
        })
    }

    #[test]
    fn parse_full_diagnostic() {
        let d = parse_diagnostic(&full_diag()).expect("valid diagnostic");
        assert_eq!(d.severity, DiagnosticSeverity::Error);
        assert_eq!(d.message, "cannot find value `x`");
        assert_eq!(d.line, 4);
        assert_eq!(d.character, 8);
        assert_eq!(d.code.as_deref(), Some("E0425"));
    }

    #[test]
    fn severity_int_maps_to_enum() {
        let mut v = full_diag();
        v["severity"] = json!(2);
        assert_eq!(
            parse_diagnostic(&v).unwrap().severity,
            DiagnosticSeverity::Warning
        );
        v["severity"] = json!(3);
        assert_eq!(
            parse_diagnostic(&v).unwrap().severity,
            DiagnosticSeverity::Information
        );
        v["severity"] = json!(4);
        assert_eq!(
            parse_diagnostic(&v).unwrap().severity,
            DiagnosticSeverity::Hint
        );
    }

    #[test]
    fn missing_severity_defaults_to_error() {
        // LSP allows omitting severity; Forge treats an unlabeled diagnostic as an error.
        let mut v = full_diag();
        v.as_object_mut().unwrap().remove("severity");
        assert_eq!(
            parse_diagnostic(&v).unwrap().severity,
            DiagnosticSeverity::Error
        );
    }

    #[test]
    fn code_can_be_integer() {
        let mut v = full_diag();
        v["code"] = json!(2304);
        assert_eq!(parse_diagnostic(&v).unwrap().code.as_deref(), Some("2304"));
    }

    #[test]
    fn missing_code_is_none() {
        let mut v = full_diag();
        v.as_object_mut().unwrap().remove("code");
        assert_eq!(parse_diagnostic(&v).unwrap().code, None);
    }

    #[test]
    fn non_scalar_code_is_none() {
        // A float/object code is not representable; degrade to None rather than panic.
        let mut v = full_diag();
        v["code"] = json!(1.5);
        assert_eq!(parse_diagnostic(&v).unwrap().code, None);
    }

    #[test]
    fn missing_message_is_rejected() {
        let mut v = full_diag();
        v.as_object_mut().unwrap().remove("message");
        assert!(parse_diagnostic(&v).is_none());
    }

    #[test]
    fn non_string_message_is_rejected() {
        let mut v = full_diag();
        v["message"] = json!(42);
        assert!(parse_diagnostic(&v).is_none());
    }

    #[test]
    fn missing_range_is_rejected() {
        let mut v = full_diag();
        v.as_object_mut().unwrap().remove("range");
        assert!(parse_diagnostic(&v).is_none());
    }

    #[test]
    fn missing_start_position_is_rejected() {
        let mut v = full_diag();
        v["range"]["start"] = Value::Null;
        assert!(parse_diagnostic(&v).is_none());
    }

    #[test]
    fn empty_object_is_rejected() {
        assert!(parse_diagnostic(&json!({})).is_none());
    }

    #[test]
    fn non_numeric_line_is_rejected() {
        let mut v = full_diag();
        v["range"]["start"]["line"] = json!("oops");
        assert!(parse_diagnostic(&v).is_none());
    }
}
