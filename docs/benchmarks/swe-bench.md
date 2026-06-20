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

## A/B-ing harness changes

Because the prediction step exercises Forge's real harness (system prompt, tools, agent loop,
context handling), re-running steps 2–3 before and after a harness change measures its effect on
task success — the ground truth for "does this make a given model perform better in Forge."
