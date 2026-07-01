# Stability Policy

Forge follows [SemVer](https://semver.org/). This document defines what "stable" means for each
public surface as Forge moves toward v2.0, and what a major/minor/patch bump may change.

## Versioned surfaces

| Surface | What's covered | Stability |
| --- | --- | --- |
| **CLI** | Subcommand names, their flags, and documented exit codes | Stable |
| **Config** (`config.toml`) | The documented sections + keys in [`docs/config-schema.json`](./config-schema.json) | Stable |
| **Output formats** | `--format json` / `--format sarif` for `forge assay`, and other documented machine-readable outputs | Stable |
| **Crate APIs** (`forge-*` on crates.io) | Public Rust items of the library crates | **Unstable** pre-2.0 (see below) |
| Internal logs, the TUI layout, human-readable stdout prose | — | Not covered; may change anytime |

## What each bump may do

- **Patch (`x.y.Z`)** — bug fixes only. No new flags, config keys, or output fields; no removals.
- **Minor (`x.Y.0`)** — additive only on stable surfaces:
  - new CLI subcommands/flags (existing ones keep working),
  - new config sections/keys (omitting them keeps the prior default),
  - new fields in JSON output (consumers must ignore unknown fields),
  - new crates or new public APIs.
- **Major (`X.0.0`)** — may remove or change behaviour of any stable surface. Breaking changes are
  collected and shipped together, with migration notes in `CHANGELOG.md`.

## CLI stability

- Subcommand and flag **names** are stable. Renames go through a deprecation period: the old name
  keeps working (often hidden) for at least one minor cycle and prints a deprecation note.
- **Exit codes** are part of the contract for scripting: `0` success; non-zero failure. Commands
  with severity gates (e.g. `forge assay --fail-on`) document their specific codes.
- New flags always have a default that preserves prior behaviour.

## Config stability

- The keys in [`config-schema.json`](./config-schema.json) are stable. A removed/renamed key goes
  through a deprecation cycle and continues to be read (with a warning) for at least one minor.
- **Unknown keys are tolerated**, not rejected, so a newer config opened by an older Forge — or a
  config written for a not-yet-installed feature — does not hard-fail. The JSON Schema is therefore
  permissive (`additionalProperties: true`) on partially-modelled sections; editors still flag
  typos within the fully-modelled ones.
- Defaults may be tuned in minor releases when the default is documented as advisory (e.g. mesh
  routing heuristics); a default that users depend on for correctness only changes in a major.

## Output-format stability

- `--format json` and `--format sarif` are append-only within a major: fields are added, never
  removed or repurposed. Always parse defensively and ignore unknown fields.
- Human-readable (default) output is **not** stable — do not scrape it; use a `--format` flag.

## Crate API stability (pre-2.0)

The `forge-*` library crates are published to crates.io (as `forge-agent-*`) to make the `forge`
binary installable (`cargo install forge-agent`), not as a stable embedding API. Until a crate
documents otherwise, its public Rust API may change in any minor release. Pin exact versions if you
depend on them directly.

## Deprecation process

1. Mark the surface deprecated in code and docs; keep it working.
2. Emit a one-time, non-fatal warning when it's used.
3. Remove no earlier than the next **major**, listing it under "Breaking changes" in `CHANGELOG.md`.
