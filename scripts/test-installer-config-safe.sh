#!/usr/bin/env bash
# Live E2E: prove the one-liner installer can be re-run to update/reinstall on any platform WITHOUT
# resetting the user's config, sessions, or API keys. Seeds a fake XDG config+data tree, runs
# install.sh twice into an isolated prefix, and asserts the seeded files are byte-for-byte untouched.
#
#   scripts/test-installer-config-safe.sh            # uses latest release
#   FORGE_VERSION=v0.3.0 scripts/test-installer-config-safe.sh
#
# Needs network (downloads the real release asset from GitHub). Exit 0 = config preserved.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
HOMEDIR="$(mktemp -d)"
trap 'rm -rf "$HOMEDIR"' EXIT

CFG="$HOMEDIR/.config/forge"
DATA="$HOMEDIR/.local/share/forge"
BIN="$HOMEDIR/.local/bin"
mkdir -p "$CFG" "$DATA" "$BIN"

# Seed user state the installer must never clobber.
printf 'default_model = "claude-cli::opus"\n[ui]\ntheme = "dark"\n' > "$CFG/config.toml"
printf 'SECRETKEYBYTES' > "$CFG/secret.key"
printf 'fake-sqlite-db-with-my-sessions' > "$DATA/forge.db"
before_cfg=$(sha256sum "$CFG/config.toml" | awk '{print $1}')
before_key=$(sha256sum "$CFG/secret.key" | awk '{print $1}')
before_db=$(sha256sum "$DATA/forge.db" | awk '{print $1}')

run_install() {
  env HOME="$HOMEDIR" \
      XDG_CONFIG_HOME="$HOMEDIR/.config" \
      XDG_DATA_HOME="$HOMEDIR/.local/share" \
      FORGE_INSTALL_DIR="$BIN" \
      ${FORGE_VERSION:+FORGE_VERSION="$FORGE_VERSION"} \
      sh "$REPO/install.sh"
}

echo "── install #1 (fresh) ──"
run_install
[ -x "$BIN/forge" ] || { echo "✗ binary not installed"; exit 1; }
echo "── install #2 (re-run = update/reinstall) ──"
run_install

after_cfg=$(sha256sum "$CFG/config.toml" | awk '{print $1}')
after_key=$(sha256sum "$CFG/secret.key" | awk '{print $1}')
after_db=$(sha256sum "$DATA/forge.db" | awk '{print $1}')

fail=0
[ "$before_cfg" = "$after_cfg" ] && echo "✓ config.toml preserved" || { echo "✗ config.toml CHANGED"; fail=1; }
[ "$before_key" = "$after_key" ] && echo "✓ secret.key preserved" || { echo "✗ secret.key CHANGED"; fail=1; }
[ "$before_db"  = "$after_db"  ] && echo "✓ forge.db preserved"   || { echo "✗ forge.db CHANGED"; fail=1; }
exit "$fail"
