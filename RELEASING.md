# Releasing Forge

A fixed checklist for cutting a release. Follow it top to bottom — most release breakage has been
a *skipped step* (a stale Homebrew version that shipped the previous binary, an empty changelog on
a minor bump), not a hard problem. Do not improvise the order.

Replace `X.Y.Z` with the version below. Pick the bump per SemVer: **patch** for fixes only,
**minor** for new features or behaviour changes, **major** for breaking changes. A version with no
user-facing change should not be released at all.

## 1. Branch

```bash
git fetch origin
git switch -c release/vX.Y.Z origin/main   # always branch from origin/main, never a stale local
```

## 2. Bump the version in ALL THREE places (this is the step that gets missed)

1. `Cargo.toml` — workspace `version = "X.Y.Z"`.
2. `Cargo.lock` — run `cargo build` once; it rewrites every `forge-*` crate to `X.Y.Z`. Stage it.
3. `homebrew/forge.rb` — set `version "X.Y.Z"` **and** reset all three `sha256` lines to
   `0000…0000` (64 zeros). The real hashes are filled in step 6, *after* the binaries exist. If you
   forget this, `brew install` silently serves the previous release.

Verify nothing still references the old version:

```bash
grep -rn "X.Y.Z" Cargo.toml homebrew/forge.rb        # all present
grep -rn "<old version>" Cargo.toml homebrew/forge.rb # empty
```

## 3. Changelog

Add a `## [X.Y.Z] - YYYY-MM-DD` section to `CHANGELOG.md` with REAL entries (what changed and why,
with the touched file). A minor/major bump with only a "prepared the workspace" line is wrong —
either there is real content or it should not be a release. Update the compare links at the bottom:
add `[X.Y.Z]` and repoint `[Unreleased]` to `vX.Y.Z...HEAD`.

## 4. Pre-flight — all must be green (CI runs these too; do not rely on a hook)

```bash
cargo fmt --all
cargo clippy --workspace --all-targets
cargo test --workspace
```

## 5. PR and merge

Open a PR (`chore: prepare vX.Y.Z release`), let CI pass, merge to `main`. Do **not** tag the branch
— the tag goes on `main` after merge.

## 6. Tag, release, fill Homebrew sha

```bash
git switch main && git pull --ff-only origin main
git tag vX.Y.Z && git push origin vX.Y.Z          # triggers .github/workflows/release.yml
```

`release.yml` builds 4 targets + `checksums.txt` and publishes the GitHub Release (marked latest).
Wait for it to finish, then fill the formula:

```bash
gh release view vX.Y.Z --json assets               # confirm 4 binaries + checksums.txt
gh release download vX.Y.Z -p checksums.txt -O -    # copy the three sha256 values
```

Put aarch64-darwin / x86_64-darwin / x86_64-linux hashes into `homebrew/forge.rb`, open a second PR
(`chore: fill homebrew sha256 for vX.Y.Z`), merge. (Re-copy the hashes from *this* release — a
squash-merge race has previously carried the prior version's hashes forward.)

## 7. Verify

- `gh release view vX.Y.Z` shows latest with 4 assets + checksums.
- A pre-X.Y.Z binary's `forge update` self-replaces to X.Y.Z.
- `brew install` (or upgrade) resolves `version "X.Y.Z"` and the formula's sha256 are non-zero.
