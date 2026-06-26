# SWE-bench

Measure Forge's coding harness on real GitHub issue fixes, with numbers comparable to the
published results of other agents. The flow is two steps:

1. **Generate predictions** — Forge solves each instance; we capture the patch. (This repo.)
2. **Score predictions** — the official `swebench` Docker evaluator applies each patch in a
   hermetic environment and runs the instance's tests. (Upstream tool.)

We deliberately do **not** reimplement step 2: SWE-bench's value is its standardized,
reproducible test harness, and re-rolling it would make our numbers non-comparable.

## 1. Get a dataset

Any SWE-bench dataset works (`SWE-bench_Lite`, `SWE-bench_Verified`, the full set). Export it to
JSONL or a JSON array with at least `instance_id`, `repo`, `base_commit`, `problem_statement`:

```bash
python - <<'PY'
from datasets import load_dataset
ds = load_dataset("princeton-nlp/SWE-bench_Lite", split="test")
ds.to_json("swe-lite.jsonl")
PY
```

## 2. Generate predictions with Forge

```bash
# Smoke test on the first 5 instances, pinning a model:
forge bench swe --dataset swe-lite.jsonl --out predictions.jsonl --limit 5 --model anthropic::claude-opus-4-8

# Full run, mesh-routed:
forge bench swe --dataset swe-lite.jsonl --out predictions.jsonl
```

For each instance Forge clones `repo` at `base_commit` under `--workdir` (default
`.forge/swe-bench/`, clones reused across runs), runs **one headless turn** on the
`problem_statement` in **Bypass** mode (no permission prompts), and records the working-tree diff
as `model_patch`. The output `predictions.jsonl` has one row per instance:

```json
{"instance_id": "...", "model_name_or_path": "forge", "model_patch": "diff --git ..."}
```

A failed instance records an empty patch (counts as unresolved) instead of aborting the sweep.

> Network + disk + API spend: a full run clones many repos and makes real model calls. Start with
> `--limit`.

## 3. Score with the official evaluator

```bash
pip install swebench
python -m swebench.harness.run_evaluation \
  --dataset_name princeton-nlp/SWE-bench_Lite \
  --predictions_path predictions.jsonl \
  --max_workers 8 \
  --run_id forge-lite
```

It writes a report with the **resolved rate** (the headline SWE-bench number) and per-instance
results. Requires Docker.

## Comparing against Claude Code / Codex

To answer "does a model do as well in Forge as in its native CLI", run the **same** instances
through each agent and score all three. `--agent` swaps the harness; everything else (repo state,
task, evaluator) is identical.

```bash
# Forge's harness (mesh-routed, or pin --model provider::model)
forge bench swe --dataset swe-lite.jsonl --agent forge       --out preds-forge.jsonl  --limit 20

# Claude Code's own harness (claude -p --dangerously-skip-permissions)
forge bench swe --dataset swe-lite.jsonl --agent claude-code --out preds-claude.jsonl --limit 20 --model opus

# Codex's own harness (codex exec --full-auto)
forge bench swe --dataset swe-lite.jsonl --agent codex       --out preds-codex.jsonl  --limit 20 --model gpt-5-codex
```

Each external agent runs **fully autonomous** in the freshly-reset clone (it must edit files + run
commands unattended), so `claude` / `codex` must be installed, on `PATH`, and authenticated. Then
score each predictions file with the evaluator (step 3) and compare the resolved rates:

```bash
for a in forge claude codex; do
  python -m swebench.harness.run_evaluation --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path preds-$a.jsonl --run_id $a
done
```

Use the **same model family** across agents (e.g. Forge pinned to `anthropic::claude-…` vs
`--agent claude-code --model …`) to isolate the harness from the model.

## Efficiency: resolve rate AND tokens-per-success

Resolve rate alone doesn't capture the goal "Forge gets **more out of a subscription** than the
native CLI". Every `bench swe` run also writes a metrics sidecar `<out>.metrics.jsonl` — one row
per instance with `input_tokens`, `output_tokens`, `cost_usd`, `wall_secs`, `patched`, and
`metrics_complete`:

- **Forge agent:** tokens/cost are read from Forge's own usage DB (`session_usage_db`) — reliable,
  and it captures **bridge** usage too (subscription turns).
- **External CLI:** best-effort from the CLI's machine output (`claude --output-format json`;
  `codex --json`). When nothing parseable comes back, the row is flagged `metrics_complete=false`
  so the report never invents a number.

Join the metrics with the official eval reports to get the headline table:

```bash
forge bench report \
  --metrics preds-forge.metrics.jsonl --metrics preds-claude.metrics.jsonl \
  --eval forge.forge.json            --eval claude.claude.json
```

```
agent              n  patched  resolved   tok/success   mean cost    mean s
forge             20       18  13 (65%)         48210     $0.0210      52.3
claude-code       20       17  13 (65%)         71840     $0.0312      61.0
```

`tok/success` = total tokens spent across the run ÷ resolved instances (lower is better). The
**bridge-superiority claim** is proven when, for the *same underlying model*, `--agent forge
--model <bridge-id>` matches-or-beats `--agent claude-code`'s resolve rate at a **lower
tok/success**. Omit `--eval` to print token/patch stats before you've scored.

## Multiple seeds / pass@k (smoothing model variance)

The models are non-deterministic, so a single run jitters ±1 on a small set. Run several seeds and
aggregate:

```bash
# 3 seeds → preds.seed1.jsonl, preds.seed2.jsonl, preds.seed3.jsonl
forge bench swe --dataset swe-lite.jsonl --agent forge --model claude-cli::sonnet \
  --out preds-forge.jsonl --limit 20 --attempts 3

# score each seed
for s in 1 2 3; do
  python -m swebench.harness.run_evaluation --dataset_name princeton-nlp/SWE-bench_Lite \
    --predictions_path preds-forge.seed$s.jsonl --run_id forge_s$s
done

# pass@k = solved by ANY seed, plus each seed's own rate (variance visible)
forge bench passk forge.forge_s1.json forge.forge_s2.json forge.forge_s3.json
```

Compare the same `--attempts` budget across agents (same model) for a fair pass@k head-to-head.

## A/B-ing harness changes

Because the prediction step exercises Forge's real harness (system prompt, tools, agent loop,
context handling), re-running steps 2–3 before and after a harness change measures its effect on
task success — the ground truth for "does this make a given model perform better in Forge."
