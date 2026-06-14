# ADR-0007: Layered config (figment) and secrets (env + OS keyring)

- **Status:** Accepted
- **Date:** 2026-06-14
- **Deciders:** Floris Voskamp

## Context

FR-8 needs layered configuration (project + user) for providers, routing rules, budgets,
and tool permissions. FR-9/A-5 require provider API keys to come from environment
variables with optional secure storage in the OS keyring, and **never** plaintext secrets
in config files (NFR: security). Config and data must live in the right per-OS locations
on Linux, macOS, and Windows (portability).

Facts as of 2026-06: figment 0.10.19 merges layered config from multiple providers
(defaults → TOML files → environment) with serde. keyring 4.0.1 (2026-05-12) wraps the
native secret stores: secret-service/keyutils (Linux), Keychain (macOS), Credential
Manager (Windows). directories 6.0.0 resolves per-OS config/data paths.

## Options considered

- **Config:** figment (layered merge, serde-native) vs. the `config` crate vs. hand-rolled
  toml+serde merging. figment expresses "defaults ⊕ user file ⊕ project file ⊕ env" with
  least code; hand-rolling repeats that logic; `config` is comparable but figment composes
  layers more ergonomically.
- **Secrets:** env-only vs. env + keyring vs. a bespoke encrypted file. A bespoke
  encrypted file reinvents what the OS keyring already does securely; env-only fails the
  "secure storage" half of FR-9.

## Decision

- **Config:** `figment` for layered config in a `forge-config` crate. Precedence (low→high):
  built-in defaults → user config (`<config-dir>/forge/config.toml`) → project config
  (`./.forge/config.toml`) → environment variables (`FORGE_*`). Paths resolved via
  `directories`. **Secrets are not part of this config surface.**
- **Secrets:** API keys read from provider env vars (e.g. `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`) first; if absent, looked up in the OS keyring via `keyring`. A
  `forge auth` flow stores keys into the keyring. Keys are never written to TOML or logs.

## Rationale

figment gives the exact layered-precedence model FR-8 describes with minimal code, and
keeping secrets out of it enforces the no-plaintext-secrets NFR by construction. env +
keyring uses the platform's vetted secret store rather than a home-grown scheme, and works
across all three OSes. `directories` keeps us correct on per-OS path conventions.

## Consequences

- **Positive:** One clear precedence chain; secrets physically separated from config;
  native secure storage on every OS; correct platform paths.
- **Negative / trade-offs accepted:** figment 0.10 is stable but slower-moving (last
  release 2024) — acceptable for a stable, low-churn need; if it stalls, the layering is
  small enough to re-implement. keyring availability varies (e.g. headless Linux without a
  secret-service) — env vars remain the always-available fallback.
- **Follow-ups:** Define the config schema (serde structs) and the `FORGE_*` env mapping in
  `forge-config`; document the secret-resolution order; redact secrets in all logging.
