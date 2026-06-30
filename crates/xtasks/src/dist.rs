//! Generate distribution assets — shell completions + a man page — from forge-cli's clap tree.
//!
//! Run: `cargo run -p xtasks -- gen-dist [out-dir]` (default out-dir: `dist/assets`).
//!
//! Layout written:
//!   <out>/completions/forge.bash
//!   <out>/completions/_forge        (zsh)
//!   <out>/completions/forge.fish
//!   <out>/completions/_forge.ps1    (powershell)
//!   <out>/forge.1                    (man page)
//!
//! These are platform-independent text files, so the release workflow generates them once and
//! bundles the same set into every OS archive. install.sh and the Homebrew formula install them.

use std::path::{Path, PathBuf};

use anyhow::Context;
use clap_complete::Shell;

pub fn run() -> anyhow::Result<()> {
    let out: PathBuf = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "dist/assets".to_string())
        .into();
    let completions = out.join("completions");
    std::fs::create_dir_all(&completions)
        .with_context(|| format!("create {}", completions.display()))?;

    let bin = "forge";
    for shell in [Shell::Bash, Shell::Zsh, Shell::Fish, Shell::PowerShell] {
        let mut cmd = forge_cli::command();
        clap_complete::generate_to(shell, &mut cmd, bin, &completions)
            .with_context(|| format!("generate {shell} completion"))?;
    }

    write_man(&out, bin)?;

    eprintln!(
        "gen-dist: wrote completions + man page to {}",
        out.display()
    );
    Ok(())
}

fn write_man(out: &Path, bin: &str) -> anyhow::Result<()> {
    let cmd = forge_cli::command();
    let man = clap_mangen::Man::new(cmd);
    let mut buf: Vec<u8> = Vec::new();
    man.render(&mut buf).context("render man page")?;
    let path = out.join(format!("{bin}.1"));
    std::fs::write(&path, buf).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}
