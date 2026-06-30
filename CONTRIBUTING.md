# Contributing to Forge

Thanks for your interest in Forge — a fast, model-agnostic AI coding harness and CLI written in
Rust. Forge is past 1.0 and shipping toward 2.0; this document covers the workflow, the repo
layout, and the quality bar for contributions.

## Prerequisites

- **Rust** matching `rust-toolchain.toml` (currently the stable channel; MSRV `1.85`).
- A C toolchain (for bundled native deps like `rusqlite` and the tree-sitter grammars).
- Git. No API keys are needed to build or run the test suite.

## Repository layout

Forge is a Cargo workspace under `crates/`:

| Crate | Responsibility |
| --- | --- |
| `forge-types` | Shared domain types (messages, usage, routing, permissions) |
| `forge-config` | Layered config + secret resolution (keyring / encrypted-file) |
| `forge-store` | SQLite persistence (sessions, messages, costs, decisions) |
| `forge-skills` | Slash-command + skill catalog (discovery, frontmatter, templates) |
| `forge-index` | Lattice — tree-sitter code-intelligence graph |
| `forge-mesh` | Model Mesh: rule-based task routing (cost × capability) |
| `forge-provider` | Provider-agnostic model interface (backed by genai) |
| `forge-tools` | Tool trait + core coding tools (read/write/shell/search) |
| `forge-lsp` | LSP client for live diagnostics after edits |
| `forge-mcp` | MCP client (connect Forge to external MCP servers) |
| `forge-tui` | Presenter abstraction + ratatui renderers |
| `forge-core` | Session orchestrator: the agent loop + permission broker |
| `forge-cli` | The `forge` binary (composition root + subcommands) |
| `xtasks` | Dev tasks (benchmarks, `gen-dist` for completions/man page); not published |

Architecture decisions live in `docs/architecture/` (ADRs under `decisions/`); designs and RFCs in
`docs/rfcs/` and `docs/features/`. Forge is design-first — substantial changes get an ADR or RFC.

## Development workflow

1. **Fork & branch.** Create a topic branch off `main`. Never commit directly to `main`.
2. **Branch naming:** `feat/<slug>`, `fix/<slug>`, `refactor/<slug>`, `docs/<slug>`,
   `chore/<slug>`, `ci/<slug>`, `perf/<slug>`. Example: `feat/model-mesh-router`.
3. **Conventional Commits.** `feat:`, `fix:`, `refactor:`, `docs:`, `chore:`, `test:`, `perf:`,
   `ci:` — see [Conventional Commits](https://www.conventionalcommits.org/).
4. **Keep it green.** Run the local checks below before pushing. CI must pass to merge.
5. **Open a PR** into `main`, filling out the PR template. One approving review + green CI are
   required. PRs are squash-merged to keep `main` linear.

## Branching & release model

- `main` — always releasable, branch-protected. Squash-merge only, linear history.
- topic branches — short-lived, one logical change each, deleted after merge.
- release tags — `vMAJOR.MINOR.PATCH` ([SemVer](https://semver.org/)) cut from `main`. Tagging
  triggers `.github/workflows/release.yml`, which builds binaries for Linux (x86_64 + aarch64),
  macOS (Apple Silicon + Intel), and Windows, and updates the Homebrew formula.
- Public-surface stability rules are in [`docs/STABILITY.md`](docs/STABILITY.md).

## Local checks (run before every push)

These mirror CI (`.github/workflows/ci.yml`) exactly:

```bash
cargo fmt --all -- --check                                 # formatting
cargo clippy --locked --all-targets --all-features         # lints (CI runs with -D warnings)
cargo test --all --all-features                            # tests (no API keys required)
cargo build --release --locked --bin forge                 # release-profile smoke
```

CI additionally runs supply-chain checks (`.github/workflows/security.yml`): `cargo audit`
(RUSTSEC advisories) and `cargo deny check` (licenses + bans + sources, configured in `deny.toml`).
To run them locally:

```bash
cargo install cargo-audit cargo-deny
cargo audit
cargo deny check
```

## Code standards

- Comments explain **why**, not what. No comments where the code is self-evident; no docstrings on
  trivial functions.
- Prefer explicit over clever.
- New behaviour ships with tests. Bug fixes ship with a regression test where practical.
- Architecture-affecting changes update `docs/architecture/` and add an ADR under
  `docs/architecture/decisions/`.
- New config keys, CLI flags, or output fields are additive (see `docs/STABILITY.md`) and update
  `docs/config-schema.json` where relevant.

## Building distribution assets locally

Shell completions and the man page are generated from the CLI's clap definition (no runtime
subcommand):

```bash
cargo run -p xtasks -- gen-dist dist/assets
# -> dist/assets/completions/{forge.bash,_forge,forge.fish,_forge.ps1}, dist/assets/forge.1
```

The release workflow does this and bundles the output into every archive.

## Reporting bugs / proposing features

Open an issue using the relevant template. For substantial design changes, write an ADR or open a
discussion before a large PR. Security issues follow [`SECURITY.md`](SECURITY.md) — do **not** open
a public issue.
