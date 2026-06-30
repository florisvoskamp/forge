//! `forge migrate` — copy a full Forge install to another machine: config + skills + commands +
//! MCP servers + hooks (the user config dir), plus machine-agnostic model metadata. Session
//! history (`--include-sessions`) and API keys (`--include-keys`) are opt-in.
//!
//! The bundle is a compressed `.tar.gz` archive — single-file transfer, smaller on disk, and
//! fully inspectable. `--include-keys` writes secrets in PLAINTEXT; use a trusted channel and
//! delete the archive after import. `forge migrate push user@host` does export → `scp` →
//! remote `forge migrate import`. See docs/features/migrate.md and the README.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;

use crate::cli::args::MigrateCmd;

const MANIFEST_FILE: &str = "manifest.json";
const CONFIG_SUBDIR: &str = "config";
const METADATA_FILE: &str = "model-metadata.json";
const DB_FILE: &str = "forge.db";
const SECRETS_FILE: &str = "secrets.json";
const BUNDLE_DIR_NAME: &str = "forge-migrate-bundle";

/// Secret keys that aren't provider auth (search/analytics) but are still worth carrying.
const EXTRA_SECRET_KEYS: &[&str] = &["artificialanalysis", "brave"];

pub(crate) async fn migrate_cmd(cmd: MigrateCmd) -> Result<()> {
    match cmd {
        MigrateCmd::Export {
            dest,
            include_keys,
            include_sessions,
        } => export(&dest, include_keys, include_sessions),
        MigrateCmd::Import { src, force } => import(&src, force),
        MigrateCmd::Push {
            target,
            include_keys,
            include_sessions,
        } => push(&target, include_keys, include_sessions),
    }
}

// ----------------------------------------------------------------------------- export

fn export(dest: &Path, include_keys: bool, include_sessions: bool) -> Result<()> {
    let archive = archive_path(dest);

    let config_dir = forge_config::config_dir()
        .context("no config directory resolved on this system — nothing to export")?;
    if !config_dir.exists() {
        bail!(
            "config directory {} does not exist — set up Forge before exporting",
            config_dir.display()
        );
    }
    let data_dir = forge_config::data_dir();

    // Stage files into a temp dir, then pack into the archive.
    let staging = std::env::temp_dir().join(format!("forge-migrate-stage-{}", now_secs()));
    let stage_root = staging.join(BUNDLE_DIR_NAME);
    fs::create_dir_all(&stage_root)
        .with_context(|| format!("creating staging directory {}", stage_root.display()))?;

    // 1. Config tree (config + skills + commands + MCP + hooks) — always.
    let cfg_out = stage_root.join(CONFIG_SUBDIR);
    copy_dir_all(&config_dir, &cfg_out)
        .with_context(|| format!("copying config from {}", config_dir.display()))?;
    println!(
        "  ✓ config + skills + commands + MCP  ({})",
        config_dir.display()
    );

    // 2. Model metadata (health/context/pricing) — always, history-free.
    let mut metadata_rows = 0usize;
    let db_path = data_dir.as_ref().map(|d| d.join(DB_FILE));
    if let Some(db) = db_path.as_ref().filter(|p| p.exists()) {
        match forge_store::Store::open(db).and_then(|s| s.export_portable_metadata()) {
            Ok(json) => {
                metadata_rows = json.matches("\"model\"").count();
                fs::write(stage_root.join(METADATA_FILE), json)?;
                println!("  ✓ model metadata (health/context/pricing)");
            }
            Err(e) => println!("  ⚠ model metadata skipped: {e}"),
        }
    }

    // 3. Session history — opt-in.
    if include_sessions {
        match db_path.as_ref().filter(|p| p.exists()) {
            Some(db) => {
                fs::copy(db, stage_root.join(DB_FILE))
                    .with_context(|| format!("copying session db {}", db.display()))?;
                println!("  ✓ session history + usage (full db)");
            }
            None => println!("  ⚠ --include-sessions: no session db found, skipped"),
        }
    }

    // 4. API keys — opt-in, PLAINTEXT.
    let mut key_providers: Vec<String> = Vec::new();
    if include_keys {
        let mut secrets = serde_json::Map::new();
        for name in secret_key_names() {
            if let Some(v) = forge_config::secret_store::get(&name).filter(|v| !v.is_empty()) {
                secrets.insert(name.clone(), serde_json::Value::String(v));
                key_providers.push(name);
            }
        }
        fs::write(
            stage_root.join(SECRETS_FILE),
            serde_json::to_string_pretty(&serde_json::Value::Object(secrets))?,
        )?;
        eprintln!(
            "\n  ⚠ SECURITY: {} API key(s) written IN PLAINTEXT.\n    Move the bundle over a \
             trusted channel and DELETE it after import. Anyone who reads it gets your keys.\n",
            key_providers.len(),
        );
    }

    // 5. Manifest.
    let manifest = serde_json::json!({
        "kind": "forge-migrate-bundle",
        "schema": 1,
        "created_at": now_secs(),
        "source_host": hostname(),
        "includes": { "keys": include_keys, "sessions": include_sessions },
        "key_providers": key_providers,
        "metadata_rows": metadata_rows,
    });
    fs::write(
        stage_root.join(MANIFEST_FILE),
        serde_json::to_string_pretty(&manifest)?,
    )?;

    // 6. Pack into .tar.gz.
    if let Some(parent) = archive.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .with_context(|| format!("creating output directory {}", parent.display()))?;
        }
    }
    pack_bundle(&stage_root, &archive)?;
    let _ = fs::remove_dir_all(&staging);

    println!(
        "\n✓ exported Forge install to {}\n  copy it:  scp {} user@host:/tmp/\n  then on the other \
         machine:  forge migrate import {}",
        archive.display(),
        archive.display(),
        archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| archive.to_string_lossy().into_owned()),
    );
    Ok(())
}

// ----------------------------------------------------------------------------- import

fn import(src: &Path, force: bool) -> Result<()> {
    if src.to_string_lossy().ends_with(".tar.gz") {
        let temp = std::env::temp_dir().join(format!("forge-migrate-import-{}", now_secs()));
        fs::create_dir_all(&temp)
            .with_context(|| format!("creating temp dir {}", temp.display()))?;
        unpack_bundle(src, &temp)?;
        let bundle = temp.join(BUNDLE_DIR_NAME);
        let result = import_from_dir(&bundle, force);
        let _ = fs::remove_dir_all(&temp);
        result
    } else {
        import_from_dir(src, force)
    }
}

fn import_from_dir(src: &Path, force: bool) -> Result<()> {
    let manifest_path = src.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        bail!(
            "{} is not a forge-migrate bundle (no {MANIFEST_FILE})",
            src.display()
        );
    }

    // 1. Config tree — merge into the user config dir (same-named files overwritten).
    let config_dir = forge_config::config_dir().context("no config directory on this system")?;
    let cfg_in = src.join(CONFIG_SUBDIR);
    if cfg_in.exists() {
        fs::create_dir_all(&config_dir)?;
        copy_dir_all(&cfg_in, &config_dir)
            .with_context(|| format!("restoring config into {}", config_dir.display()))?;
        println!(
            "  ✓ config + skills + commands + MCP → {}",
            config_dir.display()
        );

        // Normalize imported skill/command content: replace claude→forge paths and binary names.
        let n = normalize_imported_skills(&config_dir);
        if n > 0 {
            println!("  ✓ normalized {n} skill file(s) (replaced claude→forge paths)");
        }
    }

    let data_dir = forge_config::data_dir().context("no data directory on this system")?;
    fs::create_dir_all(&data_dir)?;
    let target_db = data_dir.join(DB_FILE);

    // 2. Full session db — never clobber existing history without --force.
    let bundle_db = src.join(DB_FILE);
    let mut db_installed = false;
    if bundle_db.exists() {
        if !target_db.exists() {
            fs::copy(&bundle_db, &target_db)?;
            println!("  ✓ session history + usage → {}", target_db.display());
            db_installed = true;
        } else if force {
            fs::copy(&bundle_db, &target_db)?;
            println!(
                "  ✓ session history replaced (--force) → {}",
                target_db.display()
            );
            db_installed = true;
        } else {
            let aside = data_dir.join("forge.imported.db");
            fs::copy(&bundle_db, &aside)?;
            println!(
                "  ⚠ existing session db kept; bundle db saved to {} (use --force to replace)",
                aside.display()
            );
        }
    }

    // 3. Model metadata — upsert into the (possibly just-installed) db.
    let metadata = src.join(METADATA_FILE);
    if metadata.exists() && !db_installed {
        let json = fs::read_to_string(&metadata)?;
        match forge_store::Store::open(&target_db).and_then(|s| s.import_portable_metadata(&json)) {
            Ok(n) => println!("  ✓ model metadata: {n} row(s) merged"),
            Err(e) => println!("  ⚠ model metadata skipped: {e}"),
        }
    } else if metadata.exists() {
        println!("  ✓ model metadata included in restored db");
    }

    // 4. Secrets — restore into the keyring/secret store.
    let secrets_path = src.join(SECRETS_FILE);
    if secrets_path.exists() {
        let parsed: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&secrets_path)?).unwrap_or_default();
        let mut restored = Vec::new();
        if let Some(map) = parsed.as_object() {
            for (name, val) in map {
                if let Some(v) = val.as_str() {
                    if forge_config::secret_store::set(name, v).is_ok() {
                        restored.push(name.clone());
                    }
                }
            }
        }
        println!("  ✓ API keys restored: {}", restored.join(", "));
    }

    println!("\n✓ imported Forge install from {}", src.display());
    if secrets_path.exists() {
        println!("  reminder: delete the bundle now — it holds your API keys in plaintext.");
    }
    Ok(())
}

// ----------------------------------------------------------------------------- push (SSH)

fn push(target: &str, include_keys: bool, include_sessions: bool) -> Result<()> {
    let stamp = now_secs();
    let local = std::env::temp_dir().join(format!("forge-migrate-{stamp}.tar.gz"));
    export(&local, include_keys, include_sessions)?;

    let remote_path = format!("/tmp/forge-migrate-{stamp}.tar.gz");
    println!("\n→ copying bundle to {target}:{remote_path} (scp)…");
    run(
        "scp",
        &[&local.to_string_lossy(), &format!("{target}:{remote_path}")],
    )
    .context("scp failed — check SSH access to the target")?;

    println!("→ running remote import (forge must be installed on {target})…");
    // Try `forge` via PATH first; fall back to common install locations.
    let remote_cmd = format!(
        "command -v forge >/dev/null 2>&1 && forge migrate import {remote_path} \
         || ~/.local/bin/forge migrate import {remote_path} \
         || ~/.cargo/bin/forge migrate import {remote_path}"
    );
    run("ssh", &[target, &remote_cmd])
        .context("remote import failed — is `forge` installed on the target?")?;

    let _ = fs::remove_file(&local);
    println!(
        "\n✓ migrated to {target}. The remote bundle is at {remote_path} — delete it there if it \
         holds keys."
    );
    Ok(())
}

// ----------------------------------------------------------------------------- helpers

/// Returns `dest` unchanged if it already ends with `.tar.gz`, otherwise appends `.tar.gz`.
fn archive_path(dest: &Path) -> PathBuf {
    if dest.to_string_lossy().ends_with(".tar.gz") {
        dest.to_path_buf()
    } else {
        PathBuf::from(format!("{}.tar.gz", dest.to_string_lossy()))
    }
}

/// Packs the contents of `stage_root` into a `.tar.gz` archive at `archive_path`.
/// Inside the archive, all files are nested under `BUNDLE_DIR_NAME/`.
fn pack_bundle(stage_root: &Path, archive_path: &Path) -> Result<()> {
    let out = fs::File::create(archive_path)
        .with_context(|| format!("creating archive {}", archive_path.display()))?;
    let gz = GzEncoder::new(out, Compression::default());
    let mut builder = tar::Builder::new(gz);
    builder
        .append_dir_all(BUNDLE_DIR_NAME, stage_root)
        .with_context(|| format!("packing {}", stage_root.display()))?;
    let gz = builder.into_inner().context("finalizing tar archive")?;
    gz.finish().context("flushing gzip stream")?;
    Ok(())
}

/// Unpacks a `.tar.gz` archive into `dest`.
fn unpack_bundle(archive: &Path, dest: &Path) -> Result<()> {
    let file = fs::File::open(archive).with_context(|| format!("opening {}", archive.display()))?;
    let gz = GzDecoder::new(file);
    let mut ar = tar::Archive::new(gz);
    ar.unpack(dest)
        .with_context(|| format!("unpacking into {}", dest.display()))?;
    Ok(())
}

/// Walk `config_dir/skills/` and `config_dir/commands/` and normalize every `.md` file in-place.
/// Returns the total number of files changed.
fn normalize_imported_skills(config_dir: &Path) -> usize {
    use crate::cli::commands::import::normalize_md_dir;
    let mut n = 0;
    n += normalize_md_dir(&config_dir.join("skills"));
    n += normalize_md_dir(&config_dir.join("commands"));
    n
}

fn secret_key_names() -> Vec<String> {
    let mut names: Vec<String> = forge_config::known_key_providers()
        .map(str::to_string)
        .collect();
    for extra in EXTRA_SECRET_KEYS {
        if !names.iter().any(|n| n == extra) {
            names.push((*extra).to_string());
        }
    }
    names
}

fn copy_dir_all(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        let dst = to.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&entry.path(), &dst)?;
        } else if ft.is_file() {
            fs::copy(entry.path(), &dst)?;
        }
        // symlinks/other are skipped — a config tree shouldn't contain them.
    }
    Ok(())
}

fn run(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .status()
        .with_context(|| format!("spawning {cmd}"))?;
    if !status.success() {
        bail!("{cmd} exited with {status}");
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(p: &Path, s: &str) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, s).unwrap();
    }

    #[test]
    fn copy_dir_all_recurses() {
        let root = std::env::temp_dir().join(format!("forge-mig-test-{}", now_secs()));
        let src = root.join("src");
        let dst = root.join("dst");
        write(&src.join("a.txt"), "A");
        write(&src.join("skills/x.md"), "X");
        copy_dir_all(&src, &dst).unwrap();
        assert_eq!(fs::read_to_string(dst.join("a.txt")).unwrap(), "A");
        assert_eq!(fs::read_to_string(dst.join("skills/x.md")).unwrap(), "X");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn secret_key_names_includes_providers_and_extras() {
        let names = secret_key_names();
        assert!(names.iter().any(|n| n == "artificialanalysis"));
        assert!(names.iter().any(|n| n == "brave"));
        // at least one real provider from the known list
        assert!(names.len() > EXTRA_SECRET_KEYS.len());
    }

    #[test]
    fn round_trip_tarball() {
        let root = std::env::temp_dir().join(format!("forge-mig-rtt-{}", now_secs()));

        // Build a minimal bundle staging directory.
        let stage_root = root.join("stage");
        write(
            &stage_root.join(MANIFEST_FILE),
            r#"{"kind":"forge-migrate-bundle"}"#,
        );
        write(&stage_root.join(CONFIG_SUBDIR).join("settings.json"), "{}");

        // Pack.
        let archive = root.join("bundle.tar.gz");
        pack_bundle(&stage_root, &archive).expect("pack_bundle failed");
        assert!(archive.exists(), "archive should exist after pack");

        // Unpack.
        let extract = root.join("extract");
        fs::create_dir_all(&extract).unwrap();
        unpack_bundle(&archive, &extract).expect("unpack_bundle failed");

        // Verify known files appear under BUNDLE_DIR_NAME/.
        let manifest = extract.join(BUNDLE_DIR_NAME).join(MANIFEST_FILE);
        assert!(
            manifest.exists(),
            "manifest.json missing from extracted bundle"
        );
        let content = fs::read_to_string(&manifest).unwrap();
        assert!(
            content.contains("forge-migrate-bundle"),
            "manifest content mismatch"
        );

        let settings = extract
            .join(BUNDLE_DIR_NAME)
            .join(CONFIG_SUBDIR)
            .join("settings.json");
        assert!(
            settings.exists(),
            "config/settings.json missing from extracted bundle"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
