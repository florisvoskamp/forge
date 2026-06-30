//! Skills marketplace/registry, install, and update (docs/features/skills-system.md).
//!
//! Three persisted files under the user config dir:
//! - `marketplaces.toml` — a name → source registry (`forge plugin marketplace add/list/remove`).
//! - `installed-skills.toml` — a lockfile of installed packs (source, marketplace, pinned ref, the
//!   files written), so `forge plugin update` / `forge skill update` know what to refresh + from where.
//!
//! Fetching is git-backed (`git clone --depth 1 [--branch <ref>]`), which uniformly covers public
//! GitHub repos, private repos (via `GITHUB_TOKEN`), generic git URLs, subdirectory packages, and
//! `@ref`/tag pinning. The pure registry + lockfile helpers are path-injected so they're unit-tested
//! without touching the network.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// marketplaces.toml — the name → source registry
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct Marketplaces {
    #[serde(default)]
    pub(crate) marketplaces: BTreeMap<String, MarketplaceEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MarketplaceEntry {
    /// A GitHub `owner/repo` (top-level dirs = packages), or a full git URL.
    pub(crate) source: String,
    /// Optional pinned branch/tag for the whole marketplace.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ref")]
    pub(crate) git_ref: Option<String>,
}

pub(crate) fn load_marketplaces_at(path: &Path) -> Marketplaces {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_marketplaces_at(path: &Path, m: &Marketplaces) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = toml::to_string_pretty(m).context("serializing marketplaces.toml")?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// Add (or overwrite) a marketplace entry. Pure over the given path. Returns whether it replaced one.
pub(crate) fn add_marketplace_at(
    path: &Path,
    name: &str,
    source: &str,
    git_ref: Option<String>,
) -> Result<bool> {
    if name.trim().is_empty() {
        anyhow::bail!("marketplace name cannot be empty");
    }
    let mut m = load_marketplaces_at(path);
    let replaced = m
        .marketplaces
        .insert(
            name.to_string(),
            MarketplaceEntry {
                source: source.to_string(),
                git_ref,
            },
        )
        .is_some();
    save_marketplaces_at(path, &m)?;
    Ok(replaced)
}

/// Remove a marketplace entry. Returns whether one existed.
pub(crate) fn remove_marketplace_at(path: &Path, name: &str) -> Result<bool> {
    let mut m = load_marketplaces_at(path);
    let existed = m.marketplaces.remove(name).is_some();
    if existed {
        save_marketplaces_at(path, &m)?;
    }
    Ok(existed)
}

// ---------------------------------------------------------------------------
// installed-skills.toml — the install lockfile
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct InstalledSkills {
    #[serde(default)]
    pub(crate) skills: BTreeMap<String, InstalledEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct InstalledEntry {
    /// The repo/URL the pack was fetched from (the clone target).
    pub(crate) source: String,
    /// The marketplace it was resolved through, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) marketplace: Option<String>,
    /// The subdirectory within `source` holding this package (marketplace installs), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) subdir: Option<String>,
    /// The pinned branch/tag/ref, if any.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "ref")]
    pub(crate) git_ref: Option<String>,
    /// The skill file/dir names written into the skills dir (for update/removal).
    #[serde(default)]
    pub(crate) files: Vec<String>,
}

pub(crate) fn load_installed_at(path: &Path) -> InstalledSkills {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| toml::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_installed_at(path: &Path, lock: &InstalledSkills) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let body = toml::to_string_pretty(lock).context("serializing installed-skills.toml")?;
    std::fs::write(path, body).with_context(|| format!("writing {}", path.display()))
}

/// Record (or replace) an installed pack in the lockfile. Pure over the given path.
pub(crate) fn record_installed_at(path: &Path, name: &str, entry: InstalledEntry) -> Result<()> {
    let mut lock = load_installed_at(path);
    lock.skills.insert(name.to_string(), entry);
    save_installed_at(path, &lock)
}

/// Drop a pack from the lockfile (used by `forge plugin remove`). Returns whether it existed.
pub(crate) fn remove_installed_at(path: &Path, name: &str) -> Result<bool> {
    let mut lock = load_installed_at(path);
    let existed = lock.skills.remove(name).is_some();
    if existed {
        save_installed_at(path, &lock)?;
    }
    Ok(existed)
}

/// Drop a pack from the default lockfile.
pub(crate) fn remove_installed_entry(name: &str) -> Result<bool> {
    remove_installed_at(&installed_path()?, name)
}

// ---------------------------------------------------------------------------
// Default config paths
// ---------------------------------------------------------------------------

fn config_dir() -> Result<PathBuf> {
    forge_config::config_dir().context("no user config directory on this platform")
}
fn marketplaces_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("marketplaces.toml"))
}
fn installed_path() -> Result<PathBuf> {
    Ok(config_dir()?.join("installed-skills.toml"))
}
fn skills_dir() -> Result<PathBuf> {
    Ok(config_dir()?.join("skills"))
}

/// A private-repo / authenticated token, if the user exported one.
fn github_token() -> Option<String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .ok()
        .filter(|t| !t.is_empty())
}

// ---------------------------------------------------------------------------
// Marketplace commands
// ---------------------------------------------------------------------------

pub(crate) fn marketplace_add(name: &str, source: &str, git_ref: Option<String>) -> Result<()> {
    let path = marketplaces_path()?;
    let replaced = add_marketplace_at(&path, name, source, git_ref.clone())?;
    let pin = git_ref.map(|r| format!(" @{r}")).unwrap_or_default();
    if replaced {
        println!("✓ updated marketplace '{name}' → {source}{pin}");
    } else {
        println!("✓ added marketplace '{name}' → {source}{pin}");
    }
    println!("  install from it with: forge plugin install <pkg>@{name}");
    Ok(())
}

pub(crate) fn marketplace_list() -> Result<()> {
    let path = marketplaces_path()?;
    let m = load_marketplaces_at(&path);
    if m.marketplaces.is_empty() {
        println!("no marketplaces configured — add one with `forge plugin marketplace add <name> <source>`");
        return Ok(());
    }
    println!("configured marketplaces ({}):", m.marketplaces.len());
    for (name, entry) in &m.marketplaces {
        let pin = entry
            .git_ref
            .as_deref()
            .map(|r| format!(" @{r}"))
            .unwrap_or_default();
        println!("  {name}  →  {}{pin}", entry.source);
    }
    Ok(())
}

pub(crate) fn marketplace_remove(name: &str) -> Result<()> {
    let path = marketplaces_path()?;
    if remove_marketplace_at(&path, name)? {
        println!("✓ removed marketplace '{name}'");
    } else {
        println!("no marketplace '{name}' configured");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Resolution: turn a `pkg`/`pkg@marketplace`/`owner/repo[@ref]`/URL into a clone target
// ---------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// owner/repo or a full git URL to clone.
    pub(crate) clone_target: String,
    /// Subdirectory within the repo holding the package (marketplace installs), if any.
    pub(crate) subdir: Option<String>,
    /// Pinned branch/tag/ref, if any.
    pub(crate) git_ref: Option<String>,
    /// The lockfile key / display name for the pack.
    pub(crate) name: String,
    /// The marketplace it resolved through, if any.
    pub(crate) marketplace: Option<String>,
}

/// Derive a short pack name from an `owner/repo` or git URL (the repo's last path segment).
fn derive_pkg_name(target: &str) -> String {
    target
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(target)
        .trim_end_matches(".git")
        .to_string()
}

/// Pure resolution against a registry. `pkg` accepts:
/// - `owner/repo` / `owner/repo@ref` / a full git URL → install the whole repo.
/// - `pkg@marketplace` (where `marketplace` is registered) → the `pkg` subdir of that marketplace.
/// - a bare `pkg` together with `marketplace_flag` → the same, marketplace from the flag.
pub(crate) fn resolve(
    pkg: &str,
    marketplace_flag: Option<&str>,
    registry: &Marketplaces,
) -> Result<Resolved> {
    let pkg = pkg.trim();
    // A full URL never gets @-split (it has no marketplace/ref suffix in our grammar).
    let is_url = pkg.contains("://") || pkg.starts_with("git@");
    let (base, suffix) = if is_url {
        (pkg.to_string(), None)
    } else {
        match pkg.rsplit_once('@') {
            Some((b, s)) => (b.to_string(), Some(s.to_string())),
            None => (pkg.to_string(), None),
        }
    };

    let mut marketplace = marketplace_flag.map(str::to_string);
    let mut git_ref = None;
    if let Some(s) = suffix {
        if marketplace.is_none() && registry.marketplaces.contains_key(&s) {
            marketplace = Some(s); // pkg@marketplace
        } else {
            git_ref = Some(s); // pkg@ref (possibly alongside --marketplace)
        }
    }

    if let Some(mname) = marketplace {
        let entry = registry.marketplaces.get(&mname).with_context(|| {
            format!("no marketplace '{mname}' — add it with `forge plugin marketplace add {mname} <source>`")
        })?;
        Ok(Resolved {
            clone_target: entry.source.clone(),
            subdir: Some(base.clone()),
            git_ref: git_ref.or_else(|| entry.git_ref.clone()),
            name: base,
            marketplace: Some(mname),
        })
    } else {
        let name = derive_pkg_name(&base);
        Ok(Resolved {
            clone_target: base,
            subdir: None,
            git_ref,
            name,
            marketplace: None,
        })
    }
}

/// Build the git clone URL for a clone target, injecting a token for private GitHub repos.
fn clone_url(target: &str, token: Option<&str>) -> String {
    let with_token = |host_path: &str| match token {
        Some(t) => format!("https://x-access-token:{t}@{host_path}"),
        None => format!("https://{host_path}"),
    };
    if target.contains("://") || target.starts_with("git@") {
        // A full URL: inject the token into an https github URL when we have one.
        if let (Some(t), Some(rest)) = (token, target.strip_prefix("https://github.com/")) {
            return format!("https://x-access-token:{t}@github.com/{rest}");
        }
        return target.to_string();
    }
    // owner/repo shorthand → GitHub.
    let repo = target.trim_end_matches('/').trim_end_matches(".git");
    with_token(&format!("github.com/{repo}.git"))
}

// ---------------------------------------------------------------------------
// Fetch (git) + install into the skills dir
// ---------------------------------------------------------------------------

/// A best-effort temp dir removed on drop.
struct TempDir(PathBuf);
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn git_clone(target: &str, git_ref: Option<&str>, token: Option<&str>) -> Result<TempDir> {
    let dir = std::env::temp_dir().join(format!("forge-pkg-{}", forge_types::new_id()));
    let url = clone_url(target, token);
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("clone").arg("--depth").arg("1");
    if let Some(r) = git_ref {
        cmd.arg("--branch").arg(r);
    }
    cmd.arg(&url).arg(&dir);
    let out = cmd
        .output()
        .await
        .context("running `git clone` — is git installed and on PATH?")?;
    if !out.status.success() {
        // Redact the token from any echoed URL in the error.
        let stderr = String::from_utf8_lossy(&out.stderr);
        let safe = match token {
            Some(t) => stderr.replace(t, "***"),
            None => stderr.into_owned(),
        };
        anyhow::bail!("git clone failed for '{target}': {}", safe.trim());
    }
    Ok(TempDir(dir))
}

/// Choose the directory inside a clone that holds the skills: an explicit subdir, else a `skills/`
/// subdir if present, else the repo root.
fn pick_root(clone: &Path, subdir: Option<&str>) -> PathBuf {
    if let Some(sub) = subdir {
        return clone.join(sub);
    }
    let skills = clone.join("skills");
    if skills.is_dir() {
        skills
    } else {
        clone.to_path_buf()
    }
}

/// Install every skill found in `root` (top-level `*.md` files and any directory containing a
/// `SKILL.md`) into `skills_dir`, normalizing path/binary references. `overwrite` replaces an
/// existing skill of the same name (used by update); otherwise existing ones are kept + reported.
/// Returns the names written.
fn install_from(root: &Path, skills_dir: &Path, overwrite: bool) -> Result<Vec<String>> {
    use crate::cli::commands::import::copy_dir;
    std::fs::create_dir_all(skills_dir).ok();
    let mut installed = Vec::new();
    let entries = std::fs::read_dir(root)
        .with_context(|| format!("reading package contents at {}", root.display()))?;
    for entry in entries.flatten() {
        let from = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if from.is_dir() {
            // A skill bundle: a directory with a SKILL.md.
            if !from.join("SKILL.md").is_file() {
                continue;
            }
            let dest = skills_dir.join(&name);
            if dest.exists() {
                if !overwrite {
                    continue;
                }
                std::fs::remove_dir_all(&dest).ok();
            }
            if copy_dir(&from, &dest).is_ok() {
                normalize_in_place(&dest);
                installed.push(name);
            }
        } else if from.extension().and_then(|e| e.to_str()) == Some("md") {
            let dest = skills_dir.join(&name);
            if dest.exists() && !overwrite {
                continue;
            }
            if let Ok(raw) = std::fs::read_to_string(&from) {
                let content = forge_skills::normalize_skill_content(
                    &raw.replace("~/.claude/", "~/.config/forge/"),
                );
                if std::fs::write(&dest, content).is_ok() {
                    installed.push(name);
                }
            }
        }
    }
    Ok(installed)
}

/// Normalize every `.md` under a freshly-copied skill directory in place.
fn normalize_in_place(dir: &Path) {
    crate::cli::commands::import::normalize_md_dir(dir);
}

// ---------------------------------------------------------------------------
// Public install / update entry points
// ---------------------------------------------------------------------------

pub(crate) async fn install_plugin(pkg: &str, marketplace_flag: Option<String>) -> Result<()> {
    let registry = load_marketplaces_at(&marketplaces_path()?);
    let resolved = resolve(pkg, marketplace_flag.as_deref(), &registry)?;
    let token = github_token();
    let skills_dir = skills_dir()?;

    println!(
        "fetching {} (git clone{})…",
        resolved.name,
        resolved
            .git_ref
            .as_deref()
            .map(|r| format!(" @{r}"))
            .unwrap_or_default()
    );
    let clone = git_clone(
        &resolved.clone_target,
        resolved.git_ref.as_deref(),
        token.as_deref(),
    )
    .await?;
    let root = pick_root(&clone.0, resolved.subdir.as_deref());
    if !root.exists() {
        anyhow::bail!(
            "package path '{}' not found in {}",
            resolved.subdir.as_deref().unwrap_or("."),
            resolved.clone_target
        );
    }
    let installed = install_from(&root, &skills_dir, false)?;
    if installed.is_empty() {
        anyhow::bail!(
            "no skills found in {} (looked for *.md files and SKILL.md directories)",
            resolved.name
        );
    }

    record_installed_at(
        &installed_path()?,
        &resolved.name,
        InstalledEntry {
            source: resolved.clone_target.clone(),
            marketplace: resolved.marketplace.clone(),
            subdir: resolved.subdir.clone(),
            git_ref: resolved.git_ref.clone(),
            files: installed.clone(),
        },
    )?;

    println!(
        "✓ installed '{}' ({} skill(s)) into {}",
        resolved.name,
        installed.len(),
        skills_dir.display()
    );
    println!("  update later with: forge plugin update {}", resolved.name);
    Ok(())
}

pub(crate) async fn update_installed(name: Option<&str>) -> Result<()> {
    let lock_path = installed_path()?;
    let lock = load_installed_at(&lock_path);
    if lock.skills.is_empty() {
        println!("no installed skill packs to update (install one with `forge plugin install`).");
        return Ok(());
    }
    let targets: Vec<(String, InstalledEntry)> = match name {
        Some(n) => {
            let entry =
                lock.skills.get(n).cloned().with_context(|| {
                    format!("no installed pack '{n}' — see `forge plugin list`")
                })?;
            vec![(n.to_string(), entry)]
        }
        None => lock
            .skills
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
    };

    let token = github_token();
    let skills_dir = skills_dir()?;
    let mut updated = 0usize;
    for (name, entry) in targets {
        println!("updating {name} from {}…", entry.source);
        let clone = match git_clone(&entry.source, entry.git_ref.as_deref(), token.as_deref()).await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("  ✖ {name}: {e}");
                continue;
            }
        };
        let root = pick_root(&clone.0, entry.subdir.as_deref());
        match install_from(&root, &skills_dir, true) {
            Ok(files) if !files.is_empty() => {
                record_installed_at(
                    &lock_path,
                    &name,
                    InstalledEntry {
                        files,
                        ..entry.clone()
                    },
                )?;
                updated += 1;
                println!("  ✓ {name} updated");
            }
            Ok(_) => eprintln!("  ✖ {name}: no skills found after re-fetch"),
            Err(e) => eprintln!("  ✖ {name}: {e}"),
        }
    }
    println!("updated {updated} pack(s).");
    Ok(())
}

/// List installed packs (lockfile) + registered marketplaces.
pub(crate) fn list_installed_and_marketplaces() -> Result<()> {
    let lock = load_installed_at(&installed_path()?);
    if lock.skills.is_empty() {
        println!("no skill packs installed (install one with `forge plugin install <pkg>`).");
    } else {
        println!("installed skill packs ({}):", lock.skills.len());
        for (name, entry) in &lock.skills {
            let pin = entry
                .git_ref
                .as_deref()
                .map(|r| format!(" @{r}"))
                .unwrap_or_default();
            let via = entry
                .marketplace
                .as_deref()
                .map(|m| format!(" via {m}"))
                .unwrap_or_default();
            println!(
                "  {name}  ←  {}{pin}{via}  ({} file(s))",
                entry.source,
                entry.files.len()
            );
        }
    }
    let m = load_marketplaces_at(&marketplaces_path()?);
    if !m.marketplaces.is_empty() {
        println!("\nmarketplaces ({}):", m.marketplaces.len());
        for (name, entry) in &m.marketplaces {
            println!("  {name}  →  {}", entry.source);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("forge-mkt-{}-{}", name, forge_types::new_id()))
    }

    #[test]
    fn marketplace_add_list_remove_round_trips() {
        let path = tmp("reg").join("marketplaces.toml");
        // add two.
        assert!(!add_marketplace_at(&path, "community", "anthropics/skills", None).unwrap());
        assert!(!add_marketplace_at(
            &path,
            "internal",
            "https://git.corp/ai.git",
            Some("main".into())
        )
        .unwrap());
        let m = load_marketplaces_at(&path);
        assert_eq!(m.marketplaces.len(), 2);
        assert_eq!(m.marketplaces["community"].source, "anthropics/skills");
        assert_eq!(m.marketplaces["internal"].git_ref.as_deref(), Some("main"));
        // overwrite returns true.
        assert!(add_marketplace_at(&path, "community", "other/repo", None).unwrap());
        assert_eq!(
            load_marketplaces_at(&path).marketplaces["community"].source,
            "other/repo"
        );
        // remove.
        assert!(remove_marketplace_at(&path, "community").unwrap());
        assert!(!remove_marketplace_at(&path, "community").unwrap());
        assert_eq!(load_marketplaces_at(&path).marketplaces.len(), 1);
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn install_records_a_lockfile_entry_and_update_detects_it() {
        let path = tmp("lock").join("installed-skills.toml");
        let entry = InstalledEntry {
            source: "anthropics/skills".into(),
            marketplace: Some("community".into()),
            subdir: Some("pirate-pack".into()),
            git_ref: Some("v1.2.0".into()),
            files: vec!["pirate-pack".into()],
        };
        record_installed_at(&path, "pirate-pack", entry.clone()).unwrap();
        // update's detection step: the named pack is found with its recorded source + pin.
        let lock = load_installed_at(&path);
        assert_eq!(lock.skills.len(), 1);
        let got = lock.skills.get("pirate-pack").expect("pack recorded");
        assert_eq!(got, &entry);
        assert_eq!(got.git_ref.as_deref(), Some("v1.2.0"));
        assert_eq!(got.subdir.as_deref(), Some("pirate-pack"));
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn resolve_plain_owner_repo() {
        let r = resolve("anthropics/forge-skills", None, &Marketplaces::default()).unwrap();
        assert_eq!(r.clone_target, "anthropics/forge-skills");
        assert_eq!(r.subdir, None);
        assert_eq!(r.name, "forge-skills");
        assert_eq!(r.marketplace, None);
    }

    #[test]
    fn resolve_owner_repo_with_ref() {
        let r = resolve("owner/repo@v2", None, &Marketplaces::default()).unwrap();
        assert_eq!(r.clone_target, "owner/repo");
        assert_eq!(r.git_ref.as_deref(), Some("v2"));
        assert_eq!(r.subdir, None);
    }

    #[test]
    fn resolve_pkg_at_marketplace() {
        let mut reg = Marketplaces::default();
        reg.marketplaces.insert(
            "community".into(),
            MarketplaceEntry {
                source: "anthropics/forge-marketplace".into(),
                git_ref: Some("main".into()),
            },
        );
        let r = resolve("pirate-pack@community", None, &reg).unwrap();
        assert_eq!(r.clone_target, "anthropics/forge-marketplace");
        assert_eq!(r.subdir.as_deref(), Some("pirate-pack"));
        assert_eq!(r.name, "pirate-pack");
        assert_eq!(r.marketplace.as_deref(), Some("community"));
        // marketplace ref inherited when none on the pkg.
        assert_eq!(r.git_ref.as_deref(), Some("main"));
    }

    #[test]
    fn resolve_bare_pkg_with_marketplace_flag() {
        let mut reg = Marketplaces::default();
        reg.marketplaces.insert(
            "internal".into(),
            MarketplaceEntry {
                source: "corp/skills".into(),
                git_ref: None,
            },
        );
        let r = resolve("auth-pack", Some("internal"), &reg).unwrap();
        assert_eq!(r.clone_target, "corp/skills");
        assert_eq!(r.subdir.as_deref(), Some("auth-pack"));
    }

    #[test]
    fn resolve_unknown_marketplace_errors() {
        assert!(resolve("pkg@nope", None, &Marketplaces::default())
            .map(|r| r.git_ref)
            // `nope` isn't registered → treated as a git ref, NOT an error.
            .unwrap()
            .is_some());
        // but a bare pkg with an unknown --marketplace flag IS an error.
        assert!(resolve("pkg", Some("nope"), &Marketplaces::default()).is_err());
    }

    #[test]
    fn resolve_full_url_not_at_split() {
        let r = resolve(
            "https://git.corp/team/skills.git",
            None,
            &Marketplaces::default(),
        )
        .unwrap();
        assert_eq!(r.clone_target, "https://git.corp/team/skills.git");
        assert_eq!(r.subdir, None);
        assert_eq!(r.name, "skills");
    }

    #[test]
    fn clone_url_injects_token_for_private_github() {
        assert_eq!(
            clone_url("owner/repo", Some("ghp_x")),
            "https://x-access-token:ghp_x@github.com/owner/repo.git"
        );
        assert_eq!(
            clone_url("owner/repo", None),
            "https://github.com/owner/repo.git"
        );
        assert_eq!(
            clone_url("https://github.com/owner/repo.git", Some("ghp_x")),
            "https://x-access-token:ghp_x@github.com/owner/repo.git"
        );
        // Non-GitHub URL is left untouched (token rides via the user's git credential helper).
        assert_eq!(
            clone_url("https://git.corp/team/x.git", Some("ghp_x")),
            "https://git.corp/team/x.git"
        );
    }
}
