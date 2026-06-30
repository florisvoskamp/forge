//! Tooling facade for `forge-cli`.
//!
//! The binary (`src/main.rs`) owns the real composition root and dispatch. This library exists
//! only so build tooling — the `xtasks` shell-completion + man-page generator — can obtain the
//! clap [`clap::Command`] tree WITHOUT a runtime CLI subcommand and without touching `cli/args.rs`.
//!
//! It re-includes `cli/args.rs` (which derives the `Cli`/`Command` clap tree) and supplies the one
//! crate-internal symbol that file needs — `bench::Agent` — via a tiny mirror below. Keep the
//! mirror's variants in lockstep with `bench::Agent` in `src/bench.rs`; the derive only contributes
//! `--agent`'s value set to the generated completions, nothing else.
//!
//! `cli/args.rs` carries helper methods (e.g. `FailOnSeverity::matches`) that only the binary's
//! command handlers call; compiled into this facade they read as dead code, so allow it crate-wide
//! here (the binary crate is compiled separately and still rejects real dead code under
//! `-D warnings`).
#![allow(dead_code)]

/// Minimal mirror of `crate::bench::Agent` so the re-included `cli/args.rs` resolves
/// `use crate::bench;`. The real enum lives in `src/bench.rs`; keep the variants in sync.
mod bench {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
    pub enum Agent {
        Forge,
        ClaudeCode,
        Codex,
    }
}

#[path = "cli/args.rs"]
mod args;

/// The full `forge` clap command tree, for completion + man-page generation.
pub fn command() -> clap::Command {
    <args::Cli as clap::CommandFactory>::command()
}
