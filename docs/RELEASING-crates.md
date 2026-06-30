# Releasing to crates.io

Forge is a Cargo workspace of `forge-*` crates plus the `forge-cli` binary crate. The binary is
named `forge`; the crate that ships it is `forge-cli`. Publishing makes Forge installable with:

```bash
cargo install forge-cli      # builds + installs the `forge` binary from crates.io
```

> **Naming note.** `cargo install forge` (without `-cli`) needs a crates.io crate literally named
> `forge`. We deliberately keep the package `forge-cli` (renaming it would break the in-repo build
> scripts that reference `-p forge-cli`, and a thin `forge` shim can't re-export the binary's `main`
> without restructuring `forge-cli/src/main.rs`). The supported install command is therefore
> `cargo install forge-cli`. Reserving the bare `forge` name on crates.io is tracked separately.

## Prerequisites

- A crates.io API token with publish rights (`cargo login`).
- A clean tree on a release tag; `Cargo.lock` committed and in sync (`cargo build --locked`).
- All internal crates share one version (`workspace.package.version`) and the
  `[workspace.dependencies]` `version` fields **match it** (see the comment in the root
  `Cargo.toml`). A mismatch makes `cargo publish` fail to select sibling crates.

## Publish order

Crates must be published leaf-first: a crate can only be published once every crate it depends on is
already on crates.io at the matching version. The valid topological order for this workspace:

1. `forge-types`
2. `forge-skills`
3. `forge-store`
4. `forge-config`
5. `forge-index`
6. `forge-lsp`
7. `forge-mesh`
8. `forge-mcp`
9. `forge-tui`
10. `forge-provider`
11. `forge-tools`
12. `forge-core`
13. `forge-cli`

(`xtasks` is `publish = false` and is never released.)

## Dry run first

Verify packaging for each crate without publishing:

```bash
cargo publish -p forge-types  --dry-run
cargo publish -p forge-config --dry-run
cargo publish -p forge-cli    --dry-run
# ...etc
```

`--dry-run` packages the crate and type-checks the packaged copy. For non-leaf crates it needs its
dependencies resolvable — either already published, or use `--allow-dirty`/path resolution within
the workspace. Leaf crates (`forge-types`) dry-run cleanly on their own.

## Publish

Run in the order above, waiting for each to be live (crates.io indexes within seconds) before the
next:

```bash
for crate in forge-types forge-skills forge-store forge-config forge-index \
             forge-lsp forge-mesh forge-mcp forge-tui forge-provider \
             forge-tools forge-core forge-cli; do
  cargo publish -p "$crate" --locked
  # give the index a moment so the next crate can resolve this one
  sleep 20
done
```

If a publish fails midway, fix it and resume from the failed crate — already-published crates can't
be re-published at the same version (bump the patch and retry the whole set if needed).

## After publishing

- `cargo install forge-cli` should now work on a clean machine.
- Tag + GitHub release (handled by `.github/workflows/release.yml`) provide the prebuilt binaries,
  Homebrew formula, AUR, and Scoop paths for users who don't build from source.
