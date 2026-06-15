# Known issues & deferred work

Tracked limitations and intentionally-deferred features. Each entry: symptom, what
we know, and the planned fix.

## Auto-edit (AcceptEdits) temper still prompts

**Symptom:** In the auto-edit temper, Forge still asks for permission on actions the
user expects to be auto-approved.

**What we know (verified in code):** `permission::decide_mode` for `AcceptEdits`
auto-allows `Write` side effects and **gates `Shell` with a prompt by design**;
read-only never prompts. The `ask_user` virtual tool always prompts regardless of
temper (it's a question to the user, not a side effect). So a turn that runs shell
commands or calls `ask_user` will still prompt in auto-edit — that part is expected.

**Unverified / to investigate:** whether *file edits* (`write_file`/`edit_file`,
`Write` side effect) still prompt in auto-edit. If they do, the bug is most likely
that the selected temper isn't reaching `Session.mode` at decision time (e.g. a live
SHIFT+TAB switch not applied to the in-flight turn, or `--mode` not sticking) rather
than in `decide_mode` itself. Needs a reproduction + a permission-decision test that
asserts auto-edit allows `write_file` end-to-end through the live session.

**Status:** documented; fix deferred. Do not assert a root cause until reproduced.

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
</content>
