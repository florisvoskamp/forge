/// Read live usage stats from local Codex and Claude session files.
///
/// Codex: `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` — each turn emits
/// an `event_msg / token_count` line with `rate_limits.primary` (5h window)
/// and `rate_limits.secondary` (weekly) `used_percent` values.
///
/// Claude: `~/.claude/projects/**/*.jsonl` — each assistant turn has
/// `message.usage.{input,output,cache_read,cache_creation}_tokens`.
/// Claude doesn't embed rate-limit percentages, so we return raw token sums.
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local};
use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct BridgeStats {
    pub codex_5h_pct: Option<f64>,
    pub codex_weekly_pct: Option<f64>,
    pub claude_5h_in: u64,
    pub claude_5h_out: u64,
    pub claude_weekly_in: u64,
    pub claude_weekly_out: u64,
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

fn fetch_codex(stats: &mut BridgeStats, home: &PathBuf) {
    let root = home.join(".codex/sessions");
    let Some(path) = most_recent_jsonl_in_recent_days(&root, 2) else {
        return;
    };
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines().rev() {
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v["type"] != "event_msg" {
            continue;
        }
        if v["payload"]["type"] != "token_count" {
            continue;
        }
        let rl = &v["payload"]["rate_limits"];
        stats.codex_5h_pct = rl["primary"]["used_percent"].as_f64();
        stats.codex_weekly_pct = rl["secondary"]["used_percent"].as_f64();
        break;
    }
}

fn most_recent_jsonl_in_recent_days(root: &PathBuf, look_back: u32) -> Option<PathBuf> {
    let now = Local::now();
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
                .filter(|p| p.extension().map_or(false, |e| e == "jsonl"))
                .collect();
            files.sort();
            if let Some(f) = files.into_iter().last() {
                return Some(f);
            }
        }
    }
    None
}

// ── Claude ───────────────────────────────────────────────────────────────────

fn fetch_claude(stats: &mut BridgeStats, home: &PathBuf) {
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
        } else if path.extension().map_or(false, |e| e == "jsonl") {
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
