# README assets

## `demo.gif` — the hero terminal cast

The README links `docs/assets/demo.gif`. Record it once and drop it here. Two easy paths:

### Option A — [vhs](https://github.com/charmbracelet/vhs) (deterministic, scripted)

```bash
# install: brew install vhs   (or: go install github.com/charmbracelet/vhs@latest)
vhs docs/assets/demo.tape      # writes docs/assets/demo.gif
```

`demo.tape` (in this folder) drives a real `forge` session: a mesh-routed task, the live TUI, and a
`forge lattice impact` query. Tweak the prompts/timings to taste, then re-run.

### Option B — asciinema + agg (records a live session)

```bash
asciinema rec demo.cast        # do a short, snappy session, then exit
agg demo.cast docs/assets/demo.gif --font-size 22 --theme asciinema
```

Keep it **short (~15–25s)** and **snappy**: launch `forge chat`, run one task end-to-end so the
routing badge + live progress + a real edit are visible, then a `forge lattice impact` query. Target
width ~820px to match the README `<img>`.
