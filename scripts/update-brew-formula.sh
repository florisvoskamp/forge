#!/usr/bin/env bash
# Update homebrew/forge.rb to a release's version + sha256 values, read from THAT release's
# checksums.txt — so the formula is never updated against assets that don't exist yet (the "sha
# race"). Run by release.yml after the assets are published, or by hand:
#   scripts/update-brew-formula.sh 0.4.67                 # fetch checksums.txt from the gh release
#   scripts/update-brew-formula.sh 0.4.67 path/to/checksums.txt   # use a local checksums file
#
# Asset-agnostic: every `sha256` line is matched to the `forge-<target>.tar.gz|zip` URL directly
# above it, so all four bottles — macOS arm64/x86_64 and Linux x86_64/aarch64 (on_arm) — are
# filled from checksums.txt with no per-target code here.
set -euo pipefail
VERSION="${1:?usage: update-brew-formula.sh <version-without-v> [checksums.txt]}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
FORMULA="$ROOT/homebrew/forge.rb"
CHECKSUMS="${2:-}"
if [ -z "$CHECKSUMS" ]; then
  CHECKSUMS="$(mktemp)"
  gh release download "v${VERSION}" --pattern checksums.txt --output "$CHECKSUMS" --clobber
fi

python3 - "$FORMULA" "$VERSION" "$CHECKSUMS" <<'PY'
import re, sys
formula, version, checksums = sys.argv[1:4]
# filename -> sha256, from "sha256␠␠filename" lines.
shas = {}
for line in open(checksums):
    parts = line.split()
    if len(parts) == 2:
        shas[parts[1]] = parts[0]
out, last_asset = [], None
for ln in open(formula).read().splitlines():
    m = re.search(r'forge-[\w.-]+\.(?:tar\.gz|zip)', ln)
    if m:
        last_asset = m.group(0)
    if ln.strip().startswith('version '):
        ln = re.sub(r'"[^"]+"', f'"{version}"', ln, count=1)
    elif ln.strip().startswith('sha256 ') and last_asset in shas:
        ln = re.sub(r'"[0-9a-f]+"', f'"{shas[last_asset]}"', ln, count=1)
    out.append(ln)
open(formula, 'w').write('\n'.join(out) + '\n')
print(f"updated {formula} -> v{version} ({len(shas)} checksums available)")
PY
