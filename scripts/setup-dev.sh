#!/usr/bin/env bash
# One-time dev setup: enable the committed git hooks (auto-fmt on commit) so CI's fmt check
# never blocks a PR. Safe to re-run.
set -e
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
git -C "$REPO" config core.hooksPath .githooks
echo "✓ git hooks enabled (.githooks) — commits now auto-run 'cargo fmt'"
