#!/bin/sh
# Forge installer. Downloads the right prebuilt binary from GitHub Releases.
#
#   curl -fsSL https://raw.githubusercontent.com/florisvoskamp/forge/main/install.sh | sh
#
# Overrides:
#   FORGE_VERSION      tag to install (default: latest release)
#   FORGE_INSTALL_DIR  where to put the binary (default: ~/.local/bin)
set -eu

REPO="florisvoskamp/forge"
INSTALL_DIR="${FORGE_INSTALL_DIR:-$HOME/.local/bin}"

err() { printf 'install: %s\n' "$1" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || err "required tool not found: $1"; }

need uname
need tar
if command -v curl >/dev/null 2>&1; then
  dl() { curl -fsSL "$1" -o "$2"; }
  fetch() { curl -fsSL "$1"; }
elif command -v wget >/dev/null 2>&1; then
  dl() { wget -qO "$2" "$1"; }
  fetch() { wget -qO - "$1"; }
else
  err "need curl or wget"
fi

os=$(uname -s)
arch=$(uname -m)
case "$os" in
  Linux)
    case "$arch" in
      x86_64|amd64) target="x86_64-unknown-linux-gnu" ;;
      *) err "unsupported Linux arch: $arch (prebuilt binaries: x86_64). Build from source: cargo install --path crates/forge-cli" ;;
    esac ;;
  Darwin)
    case "$arch" in
      arm64|aarch64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) err "unsupported macOS arch: $arch" ;;
    esac ;;
  *) err "unsupported OS: $os (Windows: download the .zip from the Releases page)" ;;
esac

version="${FORGE_VERSION:-}"
if [ -z "$version" ]; then
  version=$(fetch "https://api.github.com/repos/$REPO/releases/latest" \
    | grep '"tag_name"' | head -1 | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/')
  [ -n "$version" ] || err "could not resolve latest release tag"
fi

asset="forge-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$version/$asset"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

printf 'install: downloading %s %s...\n' "$asset" "$version" >&2
dl "$url" "$tmp/$asset" || err "download failed: $url"

# Verify against checksums.txt if present (best-effort).
if fetch "https://github.com/$REPO/releases/download/$version/checksums.txt" > "$tmp/checksums.txt" 2>/dev/null \
   && [ -s "$tmp/checksums.txt" ] && command -v sha256sum >/dev/null 2>&1; then
  want=$(grep " $asset\$" "$tmp/checksums.txt" | awk '{print $1}' | head -1)
  if [ -n "$want" ]; then
    got=$(sha256sum "$tmp/$asset" | awk '{print $1}')
    [ "$want" = "$got" ] || err "checksum mismatch for $asset"
    printf 'install: checksum ok\n' >&2
  fi
fi

tar xzf "$tmp/$asset" -C "$tmp"
mkdir -p "$INSTALL_DIR"
install -m 0755 "$tmp/forge-$target/forge" "$INSTALL_DIR/forge" 2>/dev/null \
  || { cp "$tmp/forge-$target/forge" "$INSTALL_DIR/forge" && chmod 0755 "$INSTALL_DIR/forge"; }

printf 'install: forge %s -> %s/forge\n' "$version" "$INSTALL_DIR" >&2
case ":$PATH:" in
  *":$INSTALL_DIR:"*) ;;
  *) printf 'install: add %s to your PATH:\n  export PATH="%s:$PATH"\n' "$INSTALL_DIR" "$INSTALL_DIR" >&2 ;;
esac
