//! On-demand TUI end-to-end smokes over a real PTY. Drives the actual `forge chat --mock` binary
//! through a pseudo-terminal — answering the terminal's cursor-position (DSR) query so the inline
//! viewport initializes (a CI runner's null terminal won't, hence `#[ignore]`).
//!
//! Run locally: `cargo test -p forge-cli --test tui_e2e -- --ignored --nocapture`

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

/// Launch `forge chat --mock` on a PTY in a throwaway cwd, answer DSR queries, then feed the
/// `(keys, sleep_ms_after)` script. Returns `(clean_exit, plain_output)`.
fn drive_pty(script: &[(&str, u64)]) -> (bool, String) {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    let dir = std::env::temp_dir().join(format!("forge-e2e-{}", forge_id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("sample.rs"), "fn main() { let x = 1; }\n").unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_forge"));
    cmd.arg("chat");
    cmd.arg("--mock");
    cmd.cwd(&dir);

    let mut child = pair.slave.spawn_command(cmd).expect("spawn forge");
    drop(pair.slave);

    let mut reader = pair.master.try_clone_reader().expect("reader");
    let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("writer")));
    let captured = Arc::new(Mutex::new(String::new()));

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

    let send = |s: &str| {
        let mut w = writer.lock().unwrap();
        let _ = w.write_all(s.as_bytes());
        let _ = w.flush();
    };
    thread::sleep(Duration::from_millis(1500)); // init + first DSR round-trip
    for (keys, after_ms) in script {
        send(keys);
        thread::sleep(Duration::from_millis(*after_ms));
    }

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

    let plain = strip_ansi(&captured.lock().unwrap());
    let clean = status.map(|s| s.success()).unwrap_or(false);
    (clean, plain)
}

#[test]
#[ignore = "needs a DSR-answering pty; run locally with --ignored"]
fn tui_autocheckpoints_then_undo_picker_rewinds_over_a_pty() {
    // A real turn auto-checkpoints; /undo opens the rewind picker; Enter rewinds.
    let (clean, plain) = drive_pty(&[
        ("say hi\r", 1200),
        ("/undo\r", 800),
        ("\r", 800),
        ("\x1b", 0),
    ]);
    assert!(clean, "clean exit, no panic: {plain}");
    assert!(!plain.to_lowercase().contains("panic"), "no panic: {plain}");
    assert!(
        plain.contains("rewound to that point"),
        "auto-checkpoint → /undo picker → rewind worked end to end: {plain}"
    );
}

#[test]
#[ignore = "needs a DSR-answering pty; run locally with --ignored"]
fn tui_assay_mode_opens_choice_picker_and_runs_without_crashing() {
    // /assay opens the analysis-vs-cleanup picker; selecting a choice runs the flow. Under --mock
    // with no provider keys there are no live models, so it degrades gracefully (a note) rather
    // than crashing — this smoke proves the palette → AssayChoice picker → spawn_assay wiring.
    let (clean, plain) = drive_pty(&[("/assay\r", 800), ("\r", 1200), ("\x1b", 0)]);
    assert!(clean, "clean exit, no panic: {plain}");
    assert!(!plain.to_lowercase().contains("panic"), "no panic: {plain}");
    assert!(
        plain.to_lowercase().contains("assay"),
        "the assay choice picker was reached: {plain}"
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
