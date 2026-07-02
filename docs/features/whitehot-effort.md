# Feature: White-hot effort

> The effort scale is a heat scale — and white-hot is the forge at the temperature where the
> metal glows white. One notch above `xhigh`: the same maximum reasoning intensity, plus Forge
> automatically orchestrates substantive work through workflow scripts.

## What it does

`/effort whitehot` (aliases: `white-hot`, `ultra`, `max`) pins the session to the top of the
effort scale:

- **Reasoning**: providers are sent their highest reasoning setting (there is no knob above
  `xhigh` anywhere — the extra lift comes from orchestration, not a hidden parameter).
- **Auto-workflows**: a standing instruction is injected into the session (once per pin, not per
  turn) telling the model to decompose any task with real multi-part structure — three or more
  independent subtasks, several files/items to process, research → build → verify stages — into a
  `run_workflow` script: `phase()` per stage, `parallel()`/`pipeline()` fan-out, and a final
  verification phase where separate agents adversarially check important results. Every spawned
  agent is mesh-routed and renders in the dedicated workflow view.
- **Discipline, not spam**: the same instruction tells the model to answer single-step tasks
  directly (no orchestrated trivia), to write scripts defensively (one item per line from
  discovery agents, filter empty/`null` items, never `JSON.parse` free-form prose, always end
  with `return <result>`), and to verify claims with checking agents instead of recall.
- **Routing**: ranked like `xhigh` — benchmark scores dominate cost tie-breaks, and the minimum
  context window requirement is inflated 2× so small-window models don't get picked for deep
  work.

## Using it

```
/effort whitehot        # pin
/effort                 # clear (provider default)
```

- **Ctrl+R** opens the effort slider — white-hot is the fifth stop, rendered as the forge's own
  heat ramp: ember → flame → gold → white-hot, with a blinding pulse on the handle.
- The effort cycle keybind steps `low → medium → high → xhigh → white-hot → default`.
- The statusline shows `⚒ WHITE-HOT` while pinned.

## When to use it

Big, multi-part work where thoroughness beats latency: audits, migrations, multi-file
refactors, research synthesis, anything you'd otherwise babysit through several prompts. The
mesh still routes each spawned agent to the cheapest capable model, so white-hot multiplies
agents, not necessarily cost.
