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

**Shipped:**
- `forge auth --remove <provider>` deletes the keyring entry (idempotent — reports if nothing
  was stored).
- `[mesh] disabled = ["openai", "gemini::antigravity-preview-05-2026"]` excludes a provider
  (bare prefix → all its `provider::*`) or an exact model id from discovery + routing, so the
  mesh never routes to or fails over onto it. Filtered in `discover_catalog` via
  `forge_config::is_model_disabled`; an explicit `--model` pin still overrides (deliberate).
- `forge models --clear` wipes all stale model benches (`Store::clear_all_model_health`).

**Status:** shipped + tested (`is_model_disabled`, `clear_all_model_health`).

## Shell tool: Windows execution (fixed) — denylist OS-awareness (fixed)

**Was:** the `shell` tool ran `sh -c <command>`, which doesn't exist on Windows, so shell
commands wouldn't run there at all.

**Fixed:** `shell` now selects the OS shell — `sh -c` on Unix, `cmd /C` on Windows
(`shell_invocation()` in `forge-tools/src/shell.rs`). The rest of the path (null stdin, capture,
timeout-kill) was already cross-platform. Windows exec tests (`mod exec_windows`) run on the
`windows-latest` CI runner: echo+exit, non-zero exit, timeout-kill (`ping -n`), bad-cwd spawn
failure.

**Also fixed:** the catastrophic denylist now includes Windows-specific dangerous commands:
`del /s`, `del /f /s`, `rd /s`, `rmdir /s`, `format ?:*` — added to `builtin_deny_rules()` in
`forge-config/src/lib.rs`. The `inner_script` unwrapper in `permission.rs` also handles
`cmd /C "<command>"` so patterns are checked recursively inside cmd-wrapped calls.

**Also fixed:** the hooks system now uses the same OS-appropriate shell as the shell tool
(`hook_shell()` in `forge-core/src/hooks.rs`: `sh -c` on Unix, `cmd /C` on Windows).

**Status:** all three items shipped + tested.
</content>
