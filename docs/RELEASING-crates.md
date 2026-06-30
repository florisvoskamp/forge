# Releasing to crates.io

Forge is a Cargo workspace published under the **`adforge`** brand (the `forge-*` package names are
already taken on crates.io by unrelated projects). Each crate's **package** name is `adforge-X`, but
its **lib** name is preserved as `forge_X`, so every `use forge_X::` import and `forge-X.workspace =
true` dependency key still works with zero source changes. The binary crate publishes as the bare
**`adforge`** and still builds a binary named `forge`. Installing:

```bash
cargo install adforge      # builds + installs the `forge` binary from crates.io
```

> **Naming note.** The package/lib split is intentional. Dependency KEYS in
> `[workspace.dependencies]` stay `forge-X` (with `package = "adforge-X"`) so dependents and source
> imports are untouched; only the published crate names change. The binary crate keeps
> `[[bin]] name = "forge"`, so `cargo install adforge` yields the `forge` command.

## Prerequisites

- A crates.io API token with publish rights (`cargo login`).
- A clean tree on a release tag; `Cargo.lock` committed and in sync (`cargo build --locked`).
- All internal crates share one version (`workspace.package.version`) and the
  `[workspace.dependencies]` `version` fields **match it** (see the comment in the root
  `Cargo.toml`). A mismatch makes `cargo publish` fail to select sibling crates.

## Publish order

Crates must be published leaf-first: a crate can only be published once every crate it depends on is
already on crates.io at the matching version. The valid topological order for this workspace (package
names):

1. `adforge-types`
2. `adforge-skills`
3. `adforge-store`
4. `adforge-config`
5. `adforge-index`
6. `adforge-lsp`
7. `adforge-mesh`
8. `adforge-mcp`
9. `adforge-tui`
10. `adforge-provider`
11. `adforge-tools`
12. `adforge-core`
13. `adforge` (the binary crate, published last)

(`xtasks` is `publish = false` and is never released.)

## Dry run first

Verify packaging for each crate without publishing:

```bash
cargo publish -p adforge-types  --dry-run
cargo publish -p adforge-config --dry-run
cargo publish -p adforge        --dry-run
# ...etc
```

`--dry-run` packages the crate and type-checks the packaged copy. **Only the pure leaf
(`adforge-types`) dry-runs cleanly in isolation** — it has no internal deps. Every other crate
depends on at least `adforge-types`, so its dry-run fails with `no matching package named
adforge-...` until those deps are actually published. That failure is expected pre-publish and does
not indicate a packaging problem; the real publish resolves each dep as it goes live in order.

## Publish

Run in the order above, waiting for each to be live (crates.io indexes within seconds) before the
next:

```bash
for crate in adforge-types adforge-skills adforge-store adforge-config adforge-index \
             adforge-lsp adforge-mesh adforge-mcp adforge-tui adforge-provider \
             adforge-tools adforge-core adforge; do
  cargo publish -p "$crate" --locked
  # give the index a moment so the next crate can resolve this one
  sleep 20
done
```

If a publish fails midway, fix it and resume from the failed crate — already-published crates can't
be re-published at the same version (bump the patch and retry the whole set if needed).

## After publishing

- `cargo install adforge` should now work on a clean machine (installs the `forge` binary).
- Tag + GitHub release (handled by `.github/workflows/release.yml`) provide the prebuilt binaries,
  Homebrew formula, AUR, and Scoop paths for users who don't build from source.
