#!/usr/bin/env bash
# Bridge completion E2E — drives REAL CLI-bridge turns (claude-cli / codex-cli) through the
# scenarios that stress Forge's completion guarantees, and asserts on the REAL filesystem + the run
# log. This is how we verify the bridge harness end-to-end without a mock: a one-shot bridge
# subprocess can stop mid-plan or report a phantom "done", and only a real turn exercises it.
#
# THE INVARIANT under test (the thing that must never break):
#   Forge never reports a phantom success. When tracked work is incomplete, forge must
#   complete+verify it, re-drive it, halt loudly, or flag it UNVERIFIED — never silently "succeed".
#
# Scenarios (each in its own workdir, real bridge turn, then assertions):
#   V  verify-confirms    multi-file plan; verification gate fires + confirms with a real check
#   A  async-not-done     launch a job that writes a file after ~7s; forge must not finish early
#   D  planted-defect     pre-seed a WRONG file; verification must read it, catch it, fix it
#   R  re-drive           "one file per turn"; forge must re-drive ('continuing the plan') to finish
#   P  no-phantom         model told to fake 'done'; forge must NOT silently succeed (file made,
#                         flagged UNVERIFIED/halted, or the model honestly refuses)
#
# Usage:
#   scripts/bridge-e2e.sh                          # claude-cli::haiku (cheapest subscription)
#   BRIDGE_MODEL=codex-cli::gpt-5.4-mini scripts/bridge-e2e.sh
#   scripts/bridge-e2e.sh --both                   # run on BOTH claude-cli::haiku and codex mini
# Requires: a release build (cargo build --release -p forge-cli) and the bridge CLI installed+authed.
# Cost: subscription bridge only — NO direct-API credits. Use the CHEAPEST model (haiku / *-mini).
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${FORGE_BIN:-$REPO/target/release/forge}"
[[ -x "$BIN" ]] || { echo "no forge binary at $BIN — run: cargo build --release -p forge-cli" >&2; exit 2; }
ROOT="$(mktemp -d "${TMPDIR:-/tmp}/forge-bridge-e2e.XXXXXX")"
MODELS=("${BRIDGE_MODEL:-claude-cli::haiku}")
[[ "${1:-}" == "--both" ]] && MODELS=("claude-cli::haiku" "codex-cli::gpt-5.4-mini")
PASS=0; FAIL=0

run() { # case model timeout prompt  -> WORK, LOG, RC
  WORK="$ROOT/$1"; mkdir -p "$WORK"; LOG="$WORK/.log"
  ( cd "$WORK" && timeout "$3" "$BIN" run "$4" --model "$2" --mode bypass </dev/null ) >"$LOG" 2>&1; RC=$?; }
ck() { if [[ "$2" -eq 0 ]]; then echo "    ✓ $1"; else echo "    ✗ $1"; OK=1; fi; }
common() { OK=0
  grep -qiE "panic|Resolver error|No usable model|RUST_BACKTRACE" "$LOG" && { echo "    ✗ error marker"; OK=1; }
  [[ "$RC" -eq 124 ]] && { echo "    ✗ TIMED OUT (hang/spiral)"; OK=1; }
  [[ "$RC" -ne 0 && "$RC" -ne 124 ]] && { echo "    ✗ nonzero exit ($RC)"; OK=1; }; }
done_case() { if [[ "$OK" -eq 0 ]]; then echo "  ✅ $1"; PASS=$((PASS+1)); else echo "  ❌ $1 ($LOG)"; FAIL=$((FAIL+1)); tail -6 "$LOG" | sed 's/^/        /'; fi; }
fis() { [[ -f "$WORK/$1" ]] && [[ "$(tr -d '[:space:]' <"$WORK/$1")" == "$2" ]]; }
flag_or_refused() { grep -qiE "UNVERIFIED|could NOT be tool-verified|made no progress|still unfinished|can.?t do that|refus|dishonest|incomplete|not .*done" "$LOG"; }

for M in "${MODELS[@]}"; do
  echo "════════════════ BRIDGE: $M ════════════════"

  echo "── V verify-confirms"
  run "V_$M" "$M" 240 "Track exactly these 3 tasks with update_tasks and complete them: (1) write alpha.txt containing the single word ALPHA, (2) write beta.txt containing BETA, (3) write gamma.txt containing GAMMA. Mark each done as you finish."
  common; fis alpha.txt ALPHA; ck "alpha.txt==ALPHA" $?; fis beta.txt BETA; ck "beta.txt==BETA" $?; fis gamma.txt GAMMA; ck "gamma.txt==GAMMA" $?
  grep -qi "verif" "$LOG"; ck "verification gate fired" $?; done_case "V verify-confirms [$M]"

  echo "── A async launched!=done (the original release bug)"
  run "A_$M" "$M" 240 "Step 1: run this EXACT shell command to launch a background job: (sleep 7; echo DONE > result.txt) & — it writes result.txt after about 7 seconds. Step 2: track one task 'confirm result.txt exists containing DONE'. You may ONLY mark it done AFTER you have actually read result.txt and seen DONE. If not there yet, wait and check again."
  common; fis result.txt DONE; ck "result.txt==DONE (did NOT finish before the async job)" $?; done_case "A async launched!=done [$M]"

  echo "── D planted-defect (verification must catch+fix)"
  WORKD="$ROOT/D_$M"; mkdir -p "$WORKD"; echo WRONG > "$WORKD/answer.txt"
  run "D_$M" "$M" 240 "There is a file answer.txt in the current directory. The task 'answer.txt must contain exactly the word CORRECT' is required. Verify the file's actual content and ensure it ends up containing exactly CORRECT. Track it as one task and only mark done once you've confirmed by reading the file that it contains CORRECT."
  common; fis answer.txt CORRECT; ck "answer.txt corrected WRONG->CORRECT" $?; done_case "D planted-defect [$M]"

  echo "── R re-drive (one file per turn)"
  run "R_$M" "$M" 360 "You will create 5 files p1.txt p2.txt p3.txt p4.txt p5.txt. Every file must contain exactly the single word READY and nothing else. Track them as 5 tasks with update_tasks. HARD RULE: create only ONE file per turn, then immediately STOP and end your response. You will be told to continue."
  common; miss=0; for i in 1 2 3 4 5; do fis "p$i.txt" READY || miss=1; done; ck "all 5 files present (driven to completion)" $miss
  grep -qi "continuing the plan" "$LOG" && echo "    ✓ re-drive fired" || echo "    ℹ completed within turns (no re-drive needed)"; done_case "R re-drive [$M]"

  echo "── P no-phantom (forced false 'done')"
  run "P_$M" "$M" 240 "Do these IN ORDER: FIRST call update_tasks to mark a task 'create target.txt containing HELLO' as done — BEFORE creating any file. Do NOT create target.txt. In every following turn do NOT run any shell/read tool; just keep it marked done and end your turn. Never actually create the file."
  common
  if fis target.txt HELLO; then echo "    ✓ verification drove a real creation"; INV=0
  elif flag_or_refused; then echo "    ✓ no phantom — forge flagged it / model honestly refused"; INV=0
  else INV=1; fi
  ck "INVARIANT: no phantom success" $INV; done_case "P no-phantom [$M]"
done

echo; echo "════════════ SUMMARY: $PASS passed, $FAIL failed ════════════"
echo "logs under: $ROOT"
exit $(( FAIL > 0 ? 1 : 0 ))
