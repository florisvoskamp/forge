//! End-to-end smoke of the CLI-bridge tool server (`forge mcp-serve`) — the surface a bridged
//! claude/codex actually talks to. Spawns the real binary and speaks newline-delimited JSON-RPC
//! (MCP stdio) to prove the bridge advertises `use_skill` and returns a skill's methodology, i.e.
//! that "codex/claude can find + load Forge's skills." `#[ignore]`: spawns a process + does timed
//! stdio I/O, so it's run on demand, not in CI.
//!
//! Run: `cargo test -p forge-cli --test mcp_serve_e2e -- --ignored --nocapture`

use std::io::{BufRead, BufReader, Write};
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
