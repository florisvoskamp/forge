# Feature: Workflow scripts

> Author a real JavaScript script that fans mesh-routed child agents out with genuine
> concurrency — loops, conditionals, and accumulation across rounds run for free inside the
> script instead of costing one model call per step. See
> [`docs/rfcs/forge-workflow.md`](../rfcs/forge-workflow.md) for the design rationale and the
> two GC-corruption bugs found while building it.

## Quick start

```
/workflow audit every crate for TODOs and summarize the riskiest ones
```

This runs a Complex-tier turn instructing the model to author a script and call the
`run_workflow` tool with it. You'll see live, phase-grouped rows in the activity panel as child
agents start and finish, exactly like `spawn_agents` — plus a `▶ <phase>` header wherever the
script's `phase()` calls change.

## The script API

A script is a sequence of JS statements running inside an async function — `return` a value to
make it the tool's result. Every function below returns a Promise and **must be awaited**; a bare
`log(...)` without `await` can race with the script finishing before it takes effect.

### `await agent(prompt, opts?) -> string`

Runs one mesh-routed child agent and returns its final answer as plain text.

```js
const summary = await agent("summarize crates/forge-mesh/src/lib.rs in 3 bullets");
```

`opts` (all optional):
- `agent` — a named agent type from `.forge/agents/<name>.md` (same convention as
  `spawn_agents`). Omit for the default read-only investigator.
- `phase` — a one-off phase label overriding the ambient `phase()` for this call only.

### `await parallel(thunks) -> any[]`

Runs an array of zero-arg thunks concurrently and returns their results in order.

```js
const [a, b, c] = await parallel([
    () => agent("review crates/forge-mesh for bugs"),
    () => agent("review crates/forge-tui for bugs"),
    () => agent("review crates/forge-store for bugs"),
]);
```

### `await pipeline(items, stage1, stage2, ...) -> any[]`

Runs every item through every stage in order, **independently** — item A can be on stage 3 while
item B is still on stage 1 (no barrier between items). Each stage is called as
`stage(prevResult, item, index)`.

```js
const files = ["a.rs", "b.rs", "c.rs"];
const results = await pipeline(
    files,
    (prev, file) => agent(`find TODOs in ${file}`, { phase: "scan" }),
    (findings, file) => agent(`is this TODO still relevant? ${findings}`, { phase: "triage" }),
);
```

`parallel` and `pipeline` are not special Rust code — they're plain JS built on top of `agent()`
(`Promise.all` and per-item async closures, respectively), so they share the exact same
concurrency budget as everything else in the script. A `parallel()` call in phase 1 and a
`pipeline()` in phase 2 draw from one real semaphore-bounded pool, not two independent ones.

### `await phase(title)`

Labels every subsequent `agent()` call (until the next `phase()` call) with `title`, so the
activity panel groups related rows under a `▶ title` header.

```js
await phase("research");
const findings = await parallel([...]);
await phase("fix");
const fixes = await pipeline(findings, ...);
```

### `await log(message)`

Writes a plain narrator note into the transcript — useful for surfacing an intermediate decision
the script makes (e.g. "3 of 8 findings were false positives, skipping those").

### `await workflow(name, args?) -> any`

Runs a saved script from `.forge/workflows/<name>.js` and returns whatever it returns. `name` must
be a plain filename with no path separators or `..` — this is a hard sandbox boundary, not a
convention. `args` (any JSON value) is exposed inside the saved script as a global `const args =
...;`. Bounded by the same recursion-depth guard as `spawn_agents`.

## Saved workflows (`.forge/workflows/`)

A script saved to `.forge/workflows/<name>.js` is a plain file — check it into your project's git
repo like any other source file, so a team can review, version, and share it.

```js
// .forge/workflows/audit.js
await phase("scan");
const files = args?.files ?? ["crates/forge-core/src", "crates/forge-tui/src"];
const findings = await parallel(files.map((f) => () => agent(`find bugs under ${f}`)));

await phase("verify");
const confirmed = await pipeline(findings, (prev, f) =>
    agent(`independently verify this finding is real, not a false positive: ${f}`)
);

return confirmed.join("\n\n");
```

Run it directly — no authoring turn, no model call to decide the script:

```
/workflow run audit
/workflow run audit {"files": ["crates/forge-mesh/src"]}
```

List what's saved:

```
/workflow list
```

## `/workflow` command reference

| Command | What it does |
|---|---|
| `/workflow <goal>` | Authors a new script for `<goal>` at Complex tier, then runs it. |
| `/workflow run <name> [args]` | Runs a saved script directly, in the background — no authoring turn. `args` (if present) is passed as a raw string, exposed to the script as a JSON string value. |
| `/workflow list` | Lists saved scripts in `.forge/workflows/`. |

`/wf` is a shorthand alias for `/workflow`.

`/workflow run <name>` runs as its own background task with the same busy/spinner/interrupt
semantics as a normal turn (Esc/Ctrl-C works the same way) — it does not block the input prompt,
and since there's no model in the loop to relay the result, the script's own return value is
printed as a note when it finishes.

## Config

```toml
[mesh.workflows]
max_total_agents = 200   # hard cap on total agent() calls per script run, including
                          # nested workflow() calls — a safety backstop against a
                          # pathological or accidentally-unbounded loop
```

Concurrency is shared with `spawn_agents` (`mesh.subagents.max_concurrency`,
`mesh.subagents.max_per_provider`) — one real budget governs both.

## Sandboxing

A script can only call the functions listed above — there is no ambient filesystem, network, or
process access from inside it. `workflow(name)` is additionally sandboxed to
`.forge/workflows/<name>.js` specifically (rejects any name containing `/`, `\`, or `..`). Every
`agent()` call is exactly as privileged as a `spawn_agents` child — read-only tools by default,
`Ask` resolves to `Deny`, no recursive `spawn_agents`/`run_workflow` access unless under the
configured depth cap.
