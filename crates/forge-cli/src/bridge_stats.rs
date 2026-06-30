/// Read live usage stats from local Codex and Claude session files.
///
/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — each turn emits
/// an `event_msg / token_count` line with `rate_limits.primary` (5h window)
/// and `rate_limits.secondary` (weekly) `used_percent` values.
///
/// Claude: `~/.claude/projects/**/*.jsonl` — each assistant turn has
/// `message.usage.{input,output,cache_read,cache_creation}_tokens`.
/// Claude doesn't embed rate-limit percentages, so we return raw token sums.
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local};
use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct BridgeStats {
    pub codex_5h_pct: Option<f64>,
    pub codex_weekly_pct: Option<f64>,
    pub claude_5h_pct: Option<f64>,
    pub claude_weekly_pct: Option<f64>,
    pub claude_5h_in: u64,
    pub claude_5h_out: u64,
    pub claude_weekly_in: u64,
    pub claude_weekly_out: u64,
    /// Age (seconds) of the Claude rate-limit cache when it was read — `None` if the cache is
    /// missing. Lets the overlay flag stale percentages instead of presenting them as live.
    pub claude_rl_age_secs: Option<i64>,
}

/// Harvest the CURRENT Claude rate-limit utilisation for BOTH windows by running one minimal
/// `claude` turn with `--debug` and reading the `anthropic-ratelimit-unified-{5h,7d}-utilization`
/// response headers it logs. Unlike the stream-json `rate_limit_event` (which only reports the
/// window near its limit), the headers always carry both the 5-hour and 7-day windows — the same
/// data Claude Code feeds its statusline. The only fresh source when the statusline cache is stale.
/// Returns (window, fraction) pairs, e.g. `[("five_hour", 0.10), ("weekly", 0.81)]`. Best-effort:
/// empty on failure. Costs one tiny Haiku turn, so callers should gate it on staleness.
pub fn probe_claude_limits() -> Vec<(String, f64)> {
    // Bound the probe: `claude --print` can stall on a cold network or an auth prompt. Run it on a
    // detached thread and wait at most PROBE_TIMEOUT for the result; on timeout return empty so the
    // (backgrounded) quota refresh completes instead of leaking a task blocked on a hung child. The
    // statusline cache / next refresh fills the numbers in later.
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        #[allow(unused_mut)]
        let mut cmd = std::process::Command::new("claude");
        cmd.args([
            "--debug",
            "--print",
            "--model",
            "haiku",
            "--append-system-prompt",
            "Reply with a single period.",
        ])
        .arg(".")
        .env("ANTHROPIC_LOG", "debug")
        .stdin(std::process::Stdio::null());
        // `--debug` makes the real `claude` CLI write verbose diagnostic output straight to the
        // controlling terminal via /dev/tty, bypassing stdout/stderr redirection entirely (a
        // common "always show this even if piped" pattern). Stdio::piped() (what `.output()`
        // uses) does NOT stop that — it only redirects fds 1/2, and /dev/tty is a separate path
        // to the same terminal as long as this child shares our session. Detach it into its own
        // session (setsid) so /dev/tty has no controlling terminal to resolve to: the probe still
        // runs and its captured stdout/stderr are unaffected, but it can no longer scribble raw
        // debug text over our own TUI's rendering on the same pty. Unix-only; Windows consoles
        // don't have this controlling-terminal/setsid concept, so no equivalent is needed there.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // Safety: setsid() is async-signal-safe and valid to call between fork and exec
            // (the same pattern already used in forge-tools/src/shell.rs's sandbox pre_exec).
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let out = cmd.output();
        let _ = tx.send(out);
    });
    let out = match rx.recv_timeout(PROBE_TIMEOUT) {
        Ok(Ok(out)) => out,
        _ => return Vec::new(),
    };
    // Debug logs (with the headers) go to stderr; scan both streams to be safe.
    let mut text = String::from_utf8_lossy(&out.stderr).into_owned();
    text.push_str(&String::from_utf8_lossy(&out.stdout));
    let mut res = Vec::new();
    for (hdr, window) in [
        ("anthropic-ratelimit-unified-5h-utilization", "five_hour"),
        ("anthropic-ratelimit-unified-7d-utilization", "weekly"),
    ] {
        if let Some(frac) = first_float_after(&text, hdr) {
            res.push((window.to_string(), frac));
        }
    }
    res
}

/// Find the first numeric run (digits + `.`) appearing after `key` in `text`. Tolerant of the
/// surrounding `": "..."` / log punctuation between the key and its value.
fn first_float_after(text: &str, key: &str) -> Option<f64> {
    let after = &text[text.find(key)? + key.len()..];
    let start = after.find(|c: char| c.is_ascii_digit())?;
    let tail = &after[start..];
    let end = tail
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
}

pub fn fetch() -> BridgeStats {
    let mut stats = BridgeStats::default();
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        fetch_codex(&mut stats, &home);
        fetch_claude(&mut stats, &home);
    }
    stats
}

// ── Codex ────────────────────────────────────────────────────────────────────

fn fetch_codex(stats: &mut BridgeStats, home: &Path) {
    let root = home.join(".codex/sessions");
    // Collect all session files from the last 2 days, sorted newest-first.
    let files = jsonl_files_in_recent_days(&root, 2);
    let now = now_epoch();
    for path in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines().rev() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v["type"] != "event_msg" || v["payload"]["type"] != "token_count" {
                continue;
            }
            let rl = &v["payload"]["rate_limits"];
            let p_resets = rl["primary"]["resets_at"].as_i64().unwrap_or(0);
            if p_resets > now {
                // Window still open — use reported usage.
                stats.codex_5h_pct = rl["primary"]["used_percent"].as_f64();
            } else if p_resets > 0 && now - p_resets < 5 * 3600 {
                // Window just reset (within the prior 5h period) — usage restarted at 0.
                stats.codex_5h_pct = Some(0.0);
            }
            let s_resets = rl["secondary"]["resets_at"].as_i64().unwrap_or(0);
            if s_resets > now {
                stats.codex_weekly_pct = rl["secondary"]["used_percent"].as_f64();
            }
            // Stop as soon as we have at least weekly (most durable) data.
            if stats.codex_weekly_pct.is_some() {
                return;
            }
            break; // No valid data in this file; try the next one.
        }
    }
}

/// All Codex session `.jsonl` files from the last `look_back` days, sorted newest-first.
fn jsonl_files_in_recent_days(root: &Path, look_back: u32) -> Vec<PathBuf> {
    let now = Local::now();
    let mut all: Vec<PathBuf> = Vec::new();
    for delta in 0..=look_back {
        let day = now.date_naive() - chrono::Duration::days(delta as i64);
        let dir = root
            .join(day.year().to_string())
            .join(format!("{:02}", day.month()))
            .join(format!("{:02}", day.day()));
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut files: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
                .collect();
            files.sort_by(|a, b| b.cmp(a)); // newest first within each day
            all.extend(files);
        }
    }
    all
}

// ── Claude ───────────────────────────────────────────────────────────────────

fn fetch_claude_rate_limits(stats: &mut BridgeStats, home: &Path) {
    let path = home.join(".claude/.rate-limits-cache.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&content) else {
        return;
    };
    // Staleness is per-window: a 5-hour window's % is meaningless once it's hours old, but a
    // 7-day window barely moves — keeping a 6–24h-old weekly reading is far better than showing
    // nothing (which makes the overlay fall back to raw tokens and the mesh see the plan as 0%).
    // The cache only refreshes while Claude Code renders its statusline, so it routinely lags.
    let age = now_epoch() - v["ts"].as_i64().unwrap_or(0);
    stats.claude_rl_age_secs = Some(age);
    if age <= 6 * 3600 {
        stats.claude_5h_pct = v["5h_pct"].as_f64();
    }
    if age <= 24 * 3600 {
        stats.claude_weekly_pct = v["7d_pct"].as_f64();
    }
}

fn fetch_claude(stats: &mut BridgeStats, home: &Path) {
    fetch_claude_rate_limits(stats, home);
    let root = home.join(".claude/projects");
    let now_secs = now_epoch();
    let cutoff_5h = now_secs - 5 * 3600;
    let cutoff_week = now_secs - 7 * 24 * 3600;

    let mut files: Vec<PathBuf> = Vec::new();
    collect_recent_jsonl(&root, cutoff_week, &mut files);

    for path in files {
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if v["type"] != "assistant" {
                continue;
            }
            let ts = v["timestamp"].as_str().map(parse_ts).unwrap_or(0);
            if ts < cutoff_week {
                continue;
            }
            let u = &v["message"]["usage"];
            let inp = u["input_tokens"].as_u64().unwrap_or(0)
                + u["cache_read_input_tokens"].as_u64().unwrap_or(0)
                + u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
            let out = u["output_tokens"].as_u64().unwrap_or(0);
            stats.claude_weekly_in += inp;
            stats.claude_weekly_out += out;
            if ts >= cutoff_5h {
                stats.claude_5h_in += inp;
                stats.claude_5h_out += out;
            }
        }
    }
}

fn collect_recent_jsonl(dir: &PathBuf, cutoff_secs: i64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_recent_jsonl(&path, cutoff_secs, out);
        } else if path.extension().is_some_and(|e| e == "jsonl") {
            let recent = entry
                .metadata()
                .and_then(|m| m.modified())
                .map(|t| {
                    t.duration_since(UNIX_EPOCH)
                        .unwrap_or(Duration::ZERO)
                        .as_secs() as i64
                        >= cutoff_secs
                })
                .unwrap_or(false);
            if recent {
                out.push(path);
            }
        }
    }
}

fn now_epoch() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs() as i64
}

fn parse_ts(s: &str) -> i64 {
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .map(|d| d.timestamp())
        .unwrap_or(0)
}
