# Security Policy

## Supported versions

Forge is post-1.0 and releases frequently. Security fixes land on `main` and ship in the next
release. We support the **latest released minor series** with security patches:

| Version | Supported |
| --- | --- |
| Latest released `1.x` (current: `1.8.x`) | Yes — security + bug fixes |
| Older `1.x` | No — upgrade to the latest `1.x` |
| `0.x` | No — pre-1.0, unsupported |

Always upgrade to the latest release before reporting (`forge --version`, or re-run the installer /
`brew upgrade forge`). Stability guarantees for the CLI, config, and output formats are documented
in [`docs/STABILITY.md`](docs/STABILITY.md).

## Reporting a vulnerability

**Please do not open a public issue for security vulnerabilities.**

Report privately via GitHub's
[private vulnerability reporting](https://github.com/Adulari/forge/security/advisories/new)
("Report a vulnerability" under the repository's **Security** tab). If that is unavailable to you,
email the maintainer at **fvoskamp2005@gmail.com** with `[forge-security]` in the subject.

Please include:

- a description of the issue and its impact,
- steps to reproduce (a minimal repro helps a lot),
- the affected version (`forge --version`) and your OS,
- any suggested remediation, if you have one.

### What to expect

- **Acknowledgement** within 3 business days.
- An initial assessment (severity + whether we can reproduce) within about a week.
- A fix released as quickly as is practical for the severity, with disclosure coordinated with you.
  We credit reporters in the release notes unless you prefer to remain anonymous.

## Supply-chain & dependencies

- Dependencies are monitored continuously: `cargo audit` (RUSTSEC advisories) and `cargo deny`
  (licenses, banned/duplicate crates, source pinning) run in CI on every PR and weekly
  (`.github/workflows/security.yml`, `deny.toml`).
- Dependency updates are proposed automatically via Dependabot (`.github/dependabot.yml`).

## Scope & handling of secrets

Forge handles credentials, so a few notes on the security model:

- **API keys and OAuth tokens** are stored in the OS keyring (with an encrypted-file fallback),
  never in config files or logs (ADR-0007).
- The **shell tool** runs behind a permission broker with an unoverridable denylist; an opt-in
  OS sandbox (Linux Landlock) is available via `[shell] sandbox`.
- **Web tools** are SSRF-guarded. **MCP** servers connect behind an allowlist.
- Forge transmits your code and keys only to the model/provider endpoints you configure, plus
  (opt-in) a GitHub release check that sends no data.

If you find a gap in any of the above, that's exactly the kind of report we want.
