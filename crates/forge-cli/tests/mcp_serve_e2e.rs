//! End-to-end smoke of the CLI-bridge tool server (`forge mcp-serve`) — the surface a bridged
//! claude/codex actually talks to. Spawns the real binary and speaks newline-delimited JSON-RPC
//! (MCP stdio) to prove the bridge advertises `use_skill` and returns a skill's methodology, i.e.
//! that "codex/claude can find + load Forge's skills." `#[ignore]`: spawns a process + does timed
//! stdio I/O, so it's run on demand, not in CI.
//!
//! Run: `cargo test -p forge-cli --test mcp_serve_e2e -- --ignored --nocapture`

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Send a JSON-RPC line and (for requests with an id) read until the matching response.
fn rpc(
    stdin: &mut impl Write,
    reader: &mut impl BufRead,
    msg: &str,
    want_id: Option<i64>,
) -> Option<serde_json::Value> {
    stdin.write_all(msg.as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
    let want_id = want_id?;
    let start = Instant::now();
    let mut line = String::new();
    while start.elapsed() < Duration::from_secs(10) {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
            if v.get("id").and_then(|i| i.as_i64()) == Some(want_id) {
                return Some(v);
            }
        }
    }
    None
}

#[test]
#[ignore = "spawns forge mcp-serve + timed stdio; run locally with --ignored"]
fn bridge_advertises_and_serves_use_skill() {
    // Seed a project skill in a throwaway cwd so the served catalog has a known entry.
    let dir = std::env::temp_dir().join(format!("forge-mcpserve-{}", std::process::id()));
    std::fs::create_dir_all(dir.join(".forge/skills/e2eskill")).unwrap();
    std::fs::write(
        dir.join(".forge/skills/e2eskill/SKILL.md"),
        "---\nname: e2eskill\ndescription: bridge e2e skill\n---\nBRIDGE_SKILL_MARKER: do it.",
    )
    .unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_forge"))
        .arg("mcp-serve")
        .current_dir(&dir)
        // Isolate the store from the developer's real per-user DB (see open_store).
        .env("FORGE_DB", dir.join("forge.db"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn forge mcp-serve");
    let mut stdin = child.stdin.take().unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());

    let init = rpc(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        Some(1),
    );
    assert!(init.is_some(), "server answered initialize");
    rpc(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        None,
    );

    // use_skill must be advertised to the bridged model.
    let tools = rpc(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
        Some(2),
    )
    .unwrap();
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str().map(String::from))
        .collect();
    assert!(
        names.iter().any(|n| n == "use_skill"),
        "use_skill advertised over the bridge: {names:?}"
    );

    // Calling it returns the seeded skill's methodology.
    let call = rpc(
        &mut stdin,
        &mut reader,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"use_skill","arguments":{"name":"e2eskill"}}}"#,
        Some(3),
    )
    .unwrap();
    let text = serde_json::to_string(&call["result"]).unwrap();
    assert!(
        text.contains("BRIDGE_SKILL_MARKER"),
        "use_skill returned the methodology: {text}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

/// Send a raw HTTP/1.1 request over a fresh connection and return the response's status line.
/// `auth` adds an `Authorization: Bearer` header when `Some`. Keeps the test dependency-free and
/// cross-platform (no async runtime, no reqwest blocking feature).
fn http_status(addr: &str, method: &str, body: &str, auth: Option<&str>) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect to mcp-serve http");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let host = addr;
    let mut req = format!("{method} /mcp HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(token) = auth {
        req.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    req.push_str("Content-Type: application/json\r\n");
    req.push_str("Accept: application/json, text/event-stream\r\n");
    req.push_str(&format!("Content-Length: {}\r\n", body.len()));
    req.push_str("Connection: close\r\n\r\n");
    req.push_str(body);
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.lines().next().unwrap_or("").to_string()
}

#[test]
#[ignore = "spawns forge mcp-serve --transport http on a real loopback port; run locally with --ignored"]
fn http_transport_binds_and_enforces_bearer() {
    let dir = std::env::temp_dir().join(format!("forge-mcpserve-http-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_forge"))
        .args(["mcp-serve", "--transport", "http", "--bind", "127.0.0.1:0"])
        .current_dir(&dir)
        .env("FORGE_DB", dir.join("forge.db"))
        .env("FORGE_MCP_SERVE_TOKEN", "topsecret")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn forge mcp-serve --transport http");

    // The server prints `... http://127.0.0.1:<port>/mcp ...` to stderr once bound; parse the addr.
    let mut err = BufReader::new(child.stderr.take().unwrap());
    let mut addr = String::new();
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(15) {
        let mut line = String::new();
        if err.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        if let Some(i) = line.find("http://") {
            let rest = &line[i + "http://".len()..];
            if let Some(end) = rest.find("/mcp") {
                addr = rest[..end].to_string();
                break;
            }
        }
    }
    assert!(!addr.is_empty(), "server printed its bound address");

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#;

    // No bearer → 401 from the auth gate, never reaching the MCP service.
    let unauth = http_status(&addr, "POST", body, None);
    assert!(
        unauth.contains("401"),
        "unauthenticated request is rejected: {unauth:?}"
    );

    // Valid bearer → the request passes the gate and the streamable-HTTP service answers (200).
    let authed = http_status(&addr, "POST", body, Some("topsecret"));
    assert!(
        authed.contains("200"),
        "authenticated initialize reaches the MCP service: {authed:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_dir_all(&dir);
}
