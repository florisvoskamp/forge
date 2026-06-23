//! `forge update`: self-update a standalone binary install to the latest GitHub release, or print
//! the right upgrade command for package-manager installs. Also powers the optional startup
//! "update now?" prompt (see [`crate::update_check`]).

use anyhow::{Context, Result};

const REPO_OWNER: &str = "florisvoskamp";
const REPO_NAME: &str = "forge";
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// How this `forge` was installed — decides whether we can safely swap the binary in place. We never
/// clobber a package-manager-owned file, which would desync its bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallMethod {
    /// Standalone binary (curl `install.sh` / manual download): safe to self-replace.
    Binary,
    /// Homebrew-managed: defer to `brew upgrade`.
    Homebrew,
    /// `cargo install`-managed: defer to `cargo install … --force`.
    Cargo,
}

/// Best-effort classification of the running binary's install method from its path.
pub fn detect_install_method() -> InstallMethod {
    let exe = std::env::current_exe().unwrap_or_default();
    let in_cargo_home = std::env::var_os("CARGO_HOME")
        .map(|c| exe.starts_with(std::path::Path::new(&c).join("bin")))
        .unwrap_or(false);
    classify_path(&exe.to_string_lossy(), in_cargo_home)
}

/// Pure path classifier (separated for testing). `in_cargo_home` is a caller-resolved hint that the
/// path sits under `$CARGO_HOME/bin` even when the literal `.cargo/bin` substring isn't present.
fn classify_path(raw_path: &str, in_cargo_home: bool) -> InstallMethod {
    let path = raw_path.replace('\\', "/").to_lowercase();
    if path.contains("/cellar/") || path.contains("/homebrew/") {
        return InstallMethod::Homebrew;
    }
    if path.contains("/.cargo/bin/") || in_cargo_home {
        return InstallMethod::Cargo;
    }
    InstallMethod::Binary
}

/// Run `forge update`. For a standalone binary install, download the latest release and swap it in
/// place; for brew/cargo, print the correct upgrade command. With `check_only`, just report whether a
/// newer release exists without changing anything.
pub fn run(check_only: bool) -> Result<()> {
    if check_only {
        return match latest_release_tag()? {
            Some(tag) if crate::update_check::is_newer(&tag, CURRENT) => {
                println!(
                    "a newer Forge is available: {tag} (you have {CURRENT}). Run `forge update`."
                );
                Ok(())
            }
            _ => {
                println!("forge is up to date ({CURRENT}).");
                Ok(())
            }
        };
    }
    match detect_install_method() {
        InstallMethod::Homebrew => {
            println!("forge was installed via Homebrew — update with:\n  brew upgrade forge");
            Ok(())
        }
        InstallMethod::Cargo => {
            println!(
                "forge was installed via cargo — update with:\n  cargo install --git https://github.com/{REPO_OWNER}/{REPO_NAME} forge-cli --force"
            );
            Ok(())
        }
        InstallMethod::Binary => self_replace(),
    }
}

/// The latest release tag (e.g. `v0.3.0`), or `None` if the lookup fails (offline, rate-limited).
fn latest_release_tag() -> Result<Option<String>> {
    let releases = self_update::backends::github::ReleaseList::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .build()
        .context("configuring release lookup")?
        .fetch()
        .context("fetching releases")?;
    Ok(releases.first().map(|r| r.version.clone()))
}

/// Download the latest GitHub release asset for this target and replace the running binary. No-op
/// (and says so) when already current — including a source/dev build whose version is ahead of the
/// latest release (self_update only applies a strictly-greater version, never a downgrade).
fn self_replace() -> Result<()> {
    // Our release archives lay the binary out as `forge-<target>/forge[.exe]`.
    let bin_in_archive = format!("forge-{{{{target}}}}/forge{}", std::env::consts::EXE_SUFFIX);
    let status = self_update::backends::github::Update::configure()
        .repo_owner(REPO_OWNER)
        .repo_name(REPO_NAME)
        .bin_name("forge")
        .bin_path_in_archive(&bin_in_archive)
        .current_version(CURRENT)
        .show_download_progress(true)
        .no_confirm(true)
        .build()
        .context("configuring the updater")?
        .update()
        .context("downloading/applying the update")?;
    if status.updated() {
        println!("✔ updated forge {CURRENT} → {}", status.version());
        println!("  restart forge to use the new version.");
    } else {
        println!("forge is already up to date ({CURRENT}).");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_path_distinguishes_install_methods() {
        // Homebrew (Apple Silicon + Intel Cellar layouts).
        assert_eq!(
            classify_path("/opt/homebrew/bin/forge", false),
            InstallMethod::Homebrew
        );
        assert_eq!(
            classify_path("/usr/local/Cellar/forge/0.2.0/bin/forge", false),
            InstallMethod::Homebrew
        );
        // Cargo, by path substring and by the resolved-CARGO_HOME hint.
        assert_eq!(
            classify_path("/home/me/.cargo/bin/forge", false),
            InstallMethod::Cargo
        );
        assert_eq!(
            classify_path("/custom/cargohome/bin/forge", true),
            InstallMethod::Cargo
        );
        // Standalone binary installs (curl install.sh / manual).
        assert_eq!(
            classify_path("/home/me/.local/bin/forge", false),
            InstallMethod::Binary
        );
        assert_eq!(
            classify_path("/usr/local/bin/forge", false),
            InstallMethod::Binary
        );
        // Windows path normalization.
        assert_eq!(
            classify_path("C:\\Users\\me\\.cargo\\bin\\forge.exe", false),
            InstallMethod::Cargo
        );
    }
}
