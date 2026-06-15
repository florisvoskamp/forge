//! On-demand TUI end-to-end smoke over a real PTY (RFC session-management-and-commands).
//!
//! Drives the actual `forge chat --mock` binary through a pseudo-terminal: runs a turn (which
//! auto-checkpoints), opens the `/undo` "rewind to a message" picker, rewinds, and quits —
//! asserting the binary renders the expected feedback and exits cleanly with no panic. This is the
//! one test that exercises the render-loop key wiring + auto-checkpoint + rewind end to end.
//!
//! `#[ignore]` by default: it needs to *answer* the terminal's cursor-position (DSR) query for the
//! inline viewport to initialize, which a CI runner's null terminal won't do — this test supplies
//! that answer itself. Run locally with:
//!   cargo test -p forge-cli --test tui_e2e -- --ignored --nocapture

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

#[test]
#[ignore = "needs a DSR-answering pty; run locally with --ignored"]
fn tui_autocheckpoints_then_undo_picker_rewinds_over_a_pty() {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // Run in an isolated cwd so the test never touches the repo's `.forge/`.
    let dir = std::env::temp_dir().join(format!("forge-e2e-{}", forge_id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_forge"));
    cmd.arg("chat");
    cmd.arg("--mock");
    cmd.cwd(&dir);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn forge");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
    let captured = Arc::new(Mutex::new(String::new()));

    // Reader thread: accumulate output and answer every DSR cursor query so the inline viewport
    // initializes (and re-initializes on the resize it does at startup).
    let w_reader = Arc::clone(&writer);
    let cap = Arc::clone(&captured);
    let reader_thread = thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                    if chunk.contains("\u{1b}[6n") {
                        if let Ok(mut w) = w_reader.lock() {
                            let _ = w.write_all(b"\x1b[1;1R");
                            let _ = w.flush();
                        }
                    }
                    cap.lock().unwrap().push_str(&chunk);
                }
            }
        }
    });

    // Give the TUI time to initialize (banner + first DSR round-trip), then drive the script.
    // Palette opens on `/`; typing the command name + Enter dispatches it.
    let send = |s: &str| {
        let mut w = writer.lock().unwrap();
        let _ = w.write_all(s.as_bytes());
        let _ = w.flush();
    };
    thread::sleep(Duration::from_millis(1500));
    send("say hi\r"); // a real turn → auto-checkpoint is created at the turn boundary
    thread::sleep(Duration::from_millis(1200));
    send("/undo\r"); // palette → /undo opens the interactive "rewind to a message" picker
    thread::sleep(Duration::from_millis(800));
    send("\r"); // Enter: rewind to the selected (only) past message
    thread::sleep(Duration::from_millis(800));
    send("\x1b"); // Esc (idle): quit

    // Wait for a clean exit (kill if it wedges).
    let start = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().expect("try_wait") {
            break Some(s);
        }
        if start.elapsed() > Duration::from_secs(8) {
            let _ = child.kill();
            break None;
        }
        thread::sleep(Duration::from_millis(50));
    };
    drop(writer);
    let _ = reader_thread.join();
    std::fs::remove_dir_all(&dir).ok();

    let out = captured.lock().unwrap().clone();
    let plain = strip_ansi(&out);

    assert!(status.is_some(), "the TUI should exit (not hang): {plain}");
    assert!(
        status.unwrap().success(),
        "clean exit (Esc quit), no panic: {plain}"
    );
    assert!(
        !plain.to_lowercase().contains("panic"),
        "no panic in output: {plain}"
    );
    assert!(
        plain.contains("rewound to that point"),
        "auto-checkpoint → /undo picker → rewind worked end to end: {plain}"
    );
}

fn forge_id() -> String {
    format!("{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], "")
}

/// Drop CSI/escape sequences so assertions match the visible text, not the control bytes.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // Skip an escape sequence: ESC [ ... <final byte 0x40..=0x7e>, or ESC <single>.
            if chars.peek() == Some(&'[') {
                chars.next();
                for d in chars.by_ref() {
                    if ('@'..='~').contains(&d) {
                        break;
                    }
                }
            } else {
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}
