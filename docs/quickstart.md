# Forge — Quick Setup Guide

Get from zero to your first AI coding session in about 5 minutes.
No credit card required — Forge works great on free provider tiers.

---

## Step 1 — Install Forge

**macOS / Linux:**
```bash
curl -fsSL https://raw.githubusercontent.com/Adulari/forge/main/install.sh | sh
```

**Windows (PowerShell):**
```powershell
irm https://raw.githubusercontent.com/Adulari/forge/main/install.ps1 | iex
```

**Homebrew:**
```bash
brew tap Adulari/forge https://github.com/Adulari/forge
brew install forge
```

**Cargo:**
```bash
cargo install adforge
```

After install, verify it works:
```bash
forge --version
```

---

## Step 2 — Connect a free AI provider

Forge works with many providers. The easiest way to get started for free:

### Option A: Groq (recommended — fast, free, no credit card)

1. Go to [console.groq.com/keys](https://console.groq.com/keys) and sign up (free)
2. Create an API key
3. Connect it to Forge:
   ```bash
   forge auth groq
   # Paste your key when prompted — it's stored in your OS keyring, not a file
   ```

### Option B: Already have Claude Code or Codex CLI installed?

Run Forge on top of your existing subscription — no new key needed:
```bash
forge run --model claude-cli::sonnet "hello"   # uses your Claude subscription
forge run --model codex-cli::o4-mini "hello"   # uses your Codex subscription
```

### Option C: Run fully offline (no key, no internet)

```bash
forge run --mock "hello"   # offline deterministic provider, great for testing
```

### Want more free providers?

Once you're up and running, connect a couple more for better coverage:
```bash
forge auth nvidia      # 100+ models, ~40 RPM free
forge auth gemini      # Gemini Flash — 1M context window, free tier
```

Full list: [Free providers →](../README.md#free-providers)

---

## Step 3 — Run the setup wizard

```bash
forge setup
```

This walks you through:
- Confirming your connected providers
- Optionally setting up a local LLM (via Ollama — runs on your machine, fully private)
- Basic preferences

You can skip any step and come back later. Run `forge setup` again anytime.

---

## Step 4 — Start your first chat

```bash
forge chat
```

This opens Forge's interactive TUI. You'll see the FORGE logo, your connected providers, and a prompt at the bottom. Type a message and press Enter.

**Try something like:**
- `explain what this codebase does` — Forge reads your project and gives an overview
- `add a README to this project` — Forge writes files, shows diffs, asks before saving
- `find all the TODO comments in this repo` — searches across all files

**Useful shortcuts in chat:**
- `Esc` — cancel / stop a running task
- `Ctrl+O` — open the activity viewer (see what Forge is doing)
- `↑ / ↓` — scroll through the transcript
- `/model` — switch models mid-session
- `/` — open the command palette (all slash commands)
- `y / n / a` — allow / deny / always-allow a permission prompt

**Prefer a simpler view?** Use inline mode — output flows in your terminal's native scrollback:
```bash
forge chat --inline
```

---

## Step 5 — Run a one-shot task

Don't need an interactive session? Just describe the task:

```bash
forge run "add input validation to the registration form"
forge run "write unit tests for the payment service"
forge run "find and fix the N+1 query in UserRepository"
```

Forge reads your project, makes the changes, and exits. Add `--tui` for a live progress view.

---

## What's next?

Once you're comfortable with the basics:

| What | How |
|------|-----|
| See all discovered models | `forge models` |
| Check routing & health | `forge models --probe` |
| Index your codebase for smarter context | `forge lattice update .` |
| Set a persistent goal | `/goal <objective>` in chat |
| Run a quality audit | `/assay` in chat |
| Import your Claude Code skills | `forge import claude` |
| Connect MCP servers | `forge mcp add ...` or `forge mcp import` |
| Move Forge to another machine | `forge migrate export ./bundle.tar.gz` |

**Full CLI reference:** [README →](../README.md#cli-reference)
**Configuration options:** [README →](../README.md#configuration)
**All free providers:** [README →](../README.md#free-providers)

---

## Troubleshooting

**`forge: command not found`**
Add `~/.local/bin` to your PATH:
```bash
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc && source ~/.bashrc
# or for zsh:
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.zshrc && source ~/.zshrc
```

**No models available / routing errors**
Run `forge doctor` — it checks your config, keys, providers, and Ollama in one go.

**Provider rate-limited**
Forge handles this automatically (waits out the reset and retries the best model). If it keeps happening, add a second free provider: `forge auth nvidia` or `forge auth gemini`.

**Permission prompts blocking everything**
In chat, answer `a` (always) to a prompt to silence it for the session, or run with `--mode accept-edits` to auto-approve file writes.

---

> **Questions?** Open an issue at [github.com/Adulari/forge](https://github.com/Adulari/forge/issues).
