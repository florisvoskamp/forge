#!/bin/bash
# Build the forge release binary and restart any running `forge mcp agent`/`forge mcp-serve`
# process so it picks up the new code immediately.
#
# Why this exists: `cargo build` replaces the binary file but does NOT affect an already-running
# process — Unix lets a process keep executing a deleted/replaced inode (visible as
# `/proc/PID/exe -> .../forge (deleted)`). A long-lived `forge mcp agent` session silently kept
# running pre-fix routing logic in memory across multiple rebuilds because nothing ever told it
# to restart. This script makes "rebuilt" and "running the new code" the same event.
#
# Usage: scripts/rebuild.sh [extra cargo build args]
set -euo pipefail
cd "$(dirname "$0")/.."

echo "stopping forge mcp processes before rebuild..."
pkill -f "forge mcp" 2>/dev/null || true

cargo build --release -p forge-agent "$@"

echo "stopping any forge mcp process still holding the old binary..."
pkill -f "forge mcp" 2>/dev/null || true

echo "done — forge mcp will respawn fresh (new binary) on its next use"
