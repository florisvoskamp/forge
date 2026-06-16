# Known issues & deferred work

Tracked limitations and intentionally-deferred features. Each entry: symptom, what
we know, and the planned fix.

## Auto-edit (AcceptEdits) temper — file edits auto-allowed (verified)

**Symptom (reported):** in the auto-edit temper, Forge seems to still ask for permission on
actions the user expects to be auto-approved.

**What we know (verified in code):** `permission::decide_mode` for `AcceptEdits` auto-allows
`Write` side effects and **gates `Shell` with a prompt by design**; read-only never prompts. The
`ask_user` virtual tool always prompts regardless of temper (it's a question to the user, not a
side effect). So a turn that runs shell commands or calls `ask_user` will still prompt in
auto-edit — that part is expected.

**Verified (file edits do NOT prompt):** the end-to-end test
`auto_edit_allows_file_writes_without_prompting` (forge-core) drives a live `AcceptEdits` session
whose model calls `write_file` with a presenter that *denies* any prompt; the file is still
written, proving the write was auto-allowed without a confirm. `--mode` sticks
(`build_session_with`: `config.permission_mode = m.into()` → `Session.mode`), and with no
matching allow/ask rule `decide` falls back to `decide_mode(AcceptEdits, Write) = Allow`.

**Residual (by design, not a bug):** a live SHIFT+TAB temper switch applies on the **next** turn,
not the in-flight one — the turn task holds the `Session` mutex for its duration, so the switch
can't mutate `Session.mode` mid-turn. A configured `ask`/`deny` rule for `write_file` also still
prompts (rules outrank the mode by design).

**Status:** common case verified + regression-tested; only the by-design residual remains.

## No way to remove / disable a provider key or model

**Symptom:** Once a provider key is set (env or keyring) there is no command to remove
it or to disable a specific provider/model. Workaround used in practice: set the key
to a junk value so auth fails and the mesh benches/avoids it.

**Planned fix (deferred):**
- `forge auth --remove <provider>` (delete the keyring entry).
- A config switch to disable a provider or specific model ids from discovery/routing
  (e.g. `[mesh] disabled = ["openai", "gemini::antigravity-preview-05-2026"]`).
- A `forge models --clear` / `/health clear` to wipe stale benches.

**Status:** documented; build later.

## Shell tool is POSIX-only (Windows gap)

**Symptom:** The `shell` tool runs `sh -c <command>` and the permission deny-list parser
assumes POSIX command syntax. On Windows (no `sh` by default) shell commands won't run.

**What we know:** This predates the cross-platform mandate
([cross-platform.md](architecture/cross-platform.md)). All other subsystems (config/secret
locations, keyring, TUI, MCP client) are portable; the shell tool is the main exception.

**Planned fix (deferred):** branch to `cmd /C` / PowerShell on Windows (or require a `sh`
on PATH and document it), and make the deny-list command parsing OS-aware.

**Status:** documented; fix deferred. Tracked on the cross-platform watch-list.

**Related:** the hooks system ([hooks.md](features/hooks.md)) also runs `sh -c`, so user hooks
are POSIX-only for the same reason; the same OS-aware fix covers both.
</content>
