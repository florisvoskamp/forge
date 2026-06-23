#!/usr/bin/env bash
# Drive `forge chat` in a real full-screen TUI inside tmux, send a scripted sequence of
# keystrokes, and capture the rendered screen — so the actual full-screen UX (task panel, plan
# card, scrolling, pickers) can be exercised and asserted on. tmux is the vt100 emulator; its
# `capture-pane -p` dumps the alternate-screen grid as plain text (which a raw PTY byte-stream
# can't, because of cursor moves and redraws).
#
# Usage:
#   scripts/tui-drive.sh [--real] [--cols N] [--rows N] <script-file>
#
# By default it runs `forge chat --mock` (deterministic, no API): the mock provider is
# plan/task-aware, so `/plan ...` renders a real plan card + seeds the task panel, and a prompt
# mentioning "task list" renders the sticky task panel — no CLI bridge or network needed.
# Pass --real to drive a live `forge chat` (real mesh/providers/bridges) instead.
#
# Script commands (one per line; '#' comments and blank lines ignored):
#   send <text>      type <text> then Enter
#   type <text>      type <text> WITHOUT Enter
#   key  <Name>      send a named key: Enter Escape Up Down Left Right PageUp PageDown Home End Tab BTab
#   sleep <ms>       wait <ms> milliseconds
#   capture [label]  dump the current screen (blank lines trimmed), with an optional label
#   expect <substr>  fail the run if the current screen does NOT contain <substr>
#   reject <substr>  fail the run if the current screen DOES contain <substr>
#
# Exit code is nonzero if any expect/reject assertion fails. The forge binary is built (release)
# if missing.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
FORGE="$REPO/target/release/forge"
COLS=200
ROWS=50
REAL=0

while [[ "${1:-}" == --* ]]; do
  case "$1" in
    --real) REAL=1; shift ;;
    --cols) COLS="$2"; shift 2 ;;
    --rows) ROWS="$2"; shift 2 ;;
    *) echo "unknown flag: $1" >&2; exit 2 ;;
  esac
done

SCRIPT="${1:-}"
[[ -z "$SCRIPT" || ! -f "$SCRIPT" ]] && { echo "usage: $0 [--real] [--cols N] [--rows N] <script-file>" >&2; exit 2; }
command -v tmux >/dev/null || { echo "tmux is required (the vt100 emulator)" >&2; exit 2; }
[[ -x "$FORGE" ]] || { echo "building forge (release)…" >&2; (cd "$REPO" && cargo build --release -p forge-cli) || exit 1; }

SESSION="forge-drive-$$"
WORK="$(mktemp -d)"
DB="$WORK/forge.db"
FAILED=0
cleanup() { tmux kill-session -t "$SESSION" 2>/dev/null; rm -rf "$WORK"; }
trap cleanup EXIT

MODE_ARGS="--mock"
[[ "$REAL" == 1 ]] && MODE_ARGS=""

tmux new-session -d -s "$SESSION" -x "$COLS" -y "$ROWS"
# Run in an isolated cwd + DB so the dev's real sessions/usage are untouched.
tmux send-keys -t "$SESSION" "cd '$WORK' && FORGE_DB='$DB' '$FORGE' chat $MODE_ARGS" Enter
sleep 3

screen() { tmux capture-pane -t "$SESSION" -p; }

while IFS= read -r line || [[ -n "$line" ]]; do
  line="${line%$'\r'}"
  [[ -z "${line// }" || "${line:0:1}" == "#" ]] && continue
  cmd="${line%% *}"; arg="${line#"$cmd"}"; arg="${arg# }"
  case "$cmd" in
    # Type the text, pause, THEN Enter as a separate keystroke. Sending "text Enter" in one call
    # can insert a newline instead of submitting when the slash-command palette is open (a `/cmd`
    # line), so the two are split with a beat between them.
    send) tmux send-keys -t "$SESSION" "$arg"; sleep 0.4; tmux send-keys -t "$SESSION" Enter ;;
    type) tmux send-keys -t "$SESSION" "$arg" ;;
    key)  tmux send-keys -t "$SESSION" "$arg" ;;
    sleep) sleep "$(awk "BEGIN{print $arg/1000}")" ;;
    capture)
      echo "──── screen${arg:+ ($arg)} ────"
      screen | grep -v '^[[:space:]]*$'
      echo "────────────────────"
      ;;
    expect)
      if screen | grep -qF -- "$arg"; then echo "✓ expect: $arg"
      else echo "✗ EXPECT FAILED: $arg"; FAILED=1; fi
      ;;
    reject)
      if screen | grep -qF -- "$arg"; then echo "✗ REJECT FAILED (present): $arg"; FAILED=1
      else echo "✓ reject: $arg"; fi
      ;;
    *) echo "unknown command: $cmd" >&2; FAILED=1 ;;
  esac
done < "$SCRIPT"

exit "$FAILED"
