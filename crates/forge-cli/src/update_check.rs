//! Startup update check: a throttled, best-effort GitHub-releases lookup that prints a one-line
//! notice when a newer Forge is available. Privacy-respecting (sends no data beyond the GET),
//! off-switchable (`[update] check = false` or `FORGE_NO_UPDATE_CHECK=1`), and never blocks or
//! fails a session — any error is silently ignored.

use std::sync::mpsc::Sender;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use forge_tui::UiMsg;

const CURRENT: &str = env!("CARGO_PKG_VERSION");
const REPO: &str = "Adulari/forge";
/// Don't hit the network more than once a day.
const THROTTLE_SECS: u64 = 24 * 3600;

/// Print an update notice if a newer release exists. Throttled + best-effort; returns immediately
/// when disabled, recently checked, or offline.
pub async fn maybe_notify(config: &forge_config::Config) {
    if !config.update.check || std::env::var("FORGE_NO_UPDATE_CHECK").is_ok() {
        return;
    }
    if !throttle_elapsed() {
        return;
    }
    touch_throttle(); // record the attempt now, so a hang/offline run doesn't retry every launch
    let Some(latest) = fetch_latest_tag().await else {
        return;
    };
    if !is_newer(&latest, CURRENT) {
        return;
    }
    // On an interactive terminal, offer to update right now; otherwise just print the notice. The
    // TTY gate keeps headless runs, pipes, and the `mcp-serve` bridge from ever blocking on input.
    use std::io::{IsTerminal, Write};
    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        print!("⚒ Forge {latest} is available (you have {CURRENT}). Update now? [y/N] ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_ok()
            && matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
        {
            // self_update is blocking — run it off the async runtime so we don't stall the reactor.
            match tokio::task::spawn_blocking(|| crate::update::run(false)).await {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("update failed: {e:#}"),
                Err(e) => eprintln!("update task panicked: {e}"),
            }
            return;
        }
    }
    println!(
        "⚒ Forge {latest} is available (you have {CURRENT}).\n  Update: run `forge update`, `brew upgrade forge`, or grab it from\n  https://github.com/{REPO}/releases/latest"
    );
}

/// TUI-mode update check: spawns the network fetch in background so it never blocks startup.
/// If a newer version is found, emits a Warning via the UiMsg channel instead of printing to
/// stdout (which would corrupt the TUI). The Sender is cloned before the TUI is built; it is
/// valid until the TUI exits. Any send error (TUI already gone) is silently ignored.
pub fn maybe_notify_background(config: &forge_config::Config, tx: Sender<UiMsg>) {
    if !config.update.check || std::env::var("FORGE_NO_UPDATE_CHECK").is_ok() {
        return;
    }
    if !throttle_elapsed() {
        return;
    }
    touch_throttle();
    tokio::spawn(async move {
        let Some(latest) = fetch_latest_tag().await else {
            return;
        };
        if !is_newer(&latest, CURRENT) {
            return;
        }
        let msg = format!(
            "⚒ Forge {latest} is available (you have {CURRENT}). Run `forge update` to upgrade."
        );
        let _ = tx.send(UiMsg::Event(forge_tui::PresenterEvent::Warning(msg)));
    });
}

fn throttle_path() -> Option<std::path::PathBuf> {
    forge_config::data_dir().map(|d| d.join(".last_update_check"))
}

fn throttle_elapsed() -> bool {
    let Some(p) = throttle_path() else {
        return true;
    };
    let last = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    now().saturating_sub(last) >= THROTTLE_SECS
}

fn touch_throttle() {
    if let Some(p) = throttle_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(&p, now().to_string());
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn fetch_latest_tag() -> Option<String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let resp = forge_provider::bundled_http_client()
        .get(&url)
        .header("User-Agent", format!("forge/{CURRENT}"))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(3))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
}

/// Whether `tag` (e.g. "v0.4.0") is a newer SemVer than `current` ("0.3.0"). Lenient: a malformed
/// tag is treated as not-newer (so we never nag on garbage).
pub(crate) fn is_newer(tag: &str, current: &str) -> bool {
    match (parse_semver(tag), parse_semver(current)) {
        (Some(a), Some(b)) => a > b,
        _ => false,
    }
}

fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    // Drop any pre-release/build suffix.
    let core = s.split(['-', '+']).next().unwrap_or(s);
    let mut it = core.split('.');
    let maj = it.next()?.parse().ok()?;
    let min = it.next().unwrap_or("0").parse().ok()?;
    let patch = it.next().unwrap_or("0").parse().ok()?;
    Some((maj, min, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_version_detected_across_components() {
        assert!(is_newer("v0.3.1", "0.3.0"));
        assert!(is_newer("0.4.0", "0.3.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("v0.2.0", "0.3.0"));
    }

    #[test]
    fn lenient_on_garbage_and_partial_tags() {
        assert!(!is_newer("not-a-version", "0.3.0"));
        assert_eq!(parse_semver("v2"), Some((2, 0, 0)));
        assert_eq!(parse_semver("0.3.0-rc.1"), Some((0, 3, 0)));
    }
}
