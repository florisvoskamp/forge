#!/usr/bin/env bash
# Real-workload E2E battery: run the same agentic workload across diverse models + mesh routing,
# capture each run's final screen AND its persisted transcript, and scan for instability signatures.
# Goal: surface real bugs (mid-turn stops, perms hallucination, nudge spam, panics, leaks, doom-loops)
# so we can harden the harness. NOT a unit test — drives the real binary over the network.
#
#   scripts/e2e-battery.sh                 # full battery
#   MODELS="codex-cli::gpt-5.5" scripts/e2e-battery.sh   # subset
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
OUT="${OUT:-$REPO/scripts/.e2e-out}"
SCRIPT="$REPO/scripts/tui-scripts/workload-bugfix-loop.txt"
mkdir -p "$OUT"

# Diverse coverage: real mesh routing, both subscription bridges, a free OpenRouter coder, a paid
# frontier via OpenRouter (direct-API path, not a bridge). Empty model = let the mesh route.
DEFAULT_MODELS=(
  ""                                       # mesh-unpinned (real routing)
  "claude-cli::opus"                       # subscription bridge (Anthropic)
  "codex-cli::gpt-5.5"                     # subscription bridge (OpenAI)
  "openrouter::qwen/qwen3-coder:free"      # free coder (weak-ish, no bridge)
  "openrouter::deepseek/deepseek-v3.2"     # paid frontier, direct-API tool loop
)
IFS=' ' read -r -a MODELS_ARR <<< "${MODELS:-}"
[ "${#MODELS_ARR[@]}" -eq 0 ] && MODELS_ARR=("${DEFAULT_MODELS[@]}")

scan() {  # $1=logfile  → print PASS/FAIL + flagged signatures
  local f="$1" flags=""
  grep -qiF 'panic' "$f"                       && flags="$flags panic"
  grep -qF  '<invoke' "$f"                      && flags="$flags native-tool-hallucination"
  grep -qiF 'denied by Forge permission' "$f"   && flags="$flags perms-hallucination"
  grep -qiF 'continuing it' "$f"                && flags="$flags nudge-spam"
  grep -qiF 'thread '\''' "$f"                  && flags="$flags thread-panic"
  # mid-turn stop heuristic: no completion marker AND no all-done task summary
  if ! grep -qiE 'tests? passed|pass/fail|all tests|✔|done\.|0 open' "$f"; then
    flags="$flags possible-mid-turn-stop"
  fi
  if [ -n "$flags" ]; then echo "FAIL:$flags"; else echo "PASS"; fi
}

echo "battery: ${#MODELS_ARR[@]} runs → $OUT"
i=0
for m in "${MODELS_ARR[@]}"; do
  i=$((i+1))
  label="${m:-mesh-unpinned}"; safe="${label//[^a-zA-Z0-9]/_}"
  log="$OUT/$safe.log"
  echo "── [$i/${#MODELS_ARR[@]}] $label ──"
  args=(--real --keep --mode bypass)
  [ -n "$m" ] && args+=(--model "$m")
  "$REPO/scripts/tui-drive.sh" "${args[@]}" "$SCRIPT" > "$log" 2>&1
  verdict="$(scan "$log")"
  printf '%s\t%s\n' "$verdict" "$label" | tee -a "$OUT/summary.txt"
done
echo "── summary ──"; cat "$OUT/summary.txt"
