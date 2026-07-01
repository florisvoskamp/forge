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

## Racy startup hang with a real provider in a minimal container (fixed)

**Was:** in a fresh/minimal container (Docker, no desktop), `forge run` with a REAL provider
occasionally printed only `● session <id>` then hung until killed. Did NOT reproduce with `--mock`
(completes, rc=0), did NOT reproduce on a full host or a fresh-HOME host, and **vanished under
`strace`** (the run then exited 0) — the classic signature of a CPU-scheduling-sensitive race.

**Root cause:** the background lattice auto-index at `forge-cli/src/cli/commands/run.rs` ran the
**synchronous, CPU-bound** `Lattice::update()` (walks the repo, tree-sitter-parses every file,
writes SQLite) inside a plain `tokio::spawn`. That occupies a tokio *worker* thread for the whole
walk. On a machine with few cores the multi-thread runtime is sized to `num_cpus`, so the indexer
starved the executor and the first turn's `route_hinted` never got scheduled → the hang right after
`● session`. `strace` perturbed scheduling enough to let the tasks interleave, hence the "vanishes
under strace" tell. Amplified by `forge-store`'s then-single blocking `Mutex<Connection>` (since replaced by an
`r2d2` pool, #308; see [backlog](#deferred-store-connection-pool)).

**Fixed:** the indexer now runs on the blocking pool via `tokio::task::spawn_blocking`, so worker
threads stay free for the agent turn regardless of core count. `scripts/e2e-docker.sh` keeps the
`E2E_REAL=1` probe to guard against regressions.

## Panic when the system has no CA certificates (fixed)

**Was:** on a stripped system/container with no `ca-certificates` installed, the genai/reqwest
HTTPS client build panicked: `Failed to build reqwest client: … No CA certificates were loaded from
the system`. A user on such a system saw a raw panic, not a clear error.

**Fixed:** `build_reqwest_client()` in `forge-provider/src/genai_provider.rs` now builds a
`reqwest::Client` with `tls_certs_only()` seeded from the bundled `webpki-root-certs` crate
(Mozilla root CAs compiled into the binary) and passes it to genai via `Client::builder()
.with_reqwest(…)`. The platform verifier (`rustls-platform-verifier`) is bypassed entirely, so
HTTPS no longer depends on the OS certificate store. Both `build_client()` (the main provider
client) and `list_models()` (auto-discovery) use this path.

Hardened further: (1) `GenAiProvider`'s derived `Default` was a latent landmine — it built
genai's *own* default client (which calls `rustls-platform-verifier` and panics on a CA-less host);
`Default` now routes through `GenAiProvider::new()` so every Forge-constructed genai client uses the
bundled-roots path. (2) A reusable `forge_provider::bundled_http_client()` was exported and the
remaining `reqwest::Client::new()` HTTPS sites in the CLI (update-check, balance, context-windows,
benchmarks, MCP, remote, local) now use it, so secondary commands no longer panic on a bare system
either.

**Update — gap closed:** `forge-index/src/embed.rs` now has its own `bundled_ca_client()`
(`webpki-root-certs`), and `forge-mcp/src/transport.rs` has `bundled_client_builder()`, used by both
the streamable-HTTP transport and the OAuth flow (`forge-mcp/src/oauth.rs`). No remaining
`reqwest::Client::new()` sites in either crate.

<a id="deferred-store-connection-pool"></a>
**Related — store connection contention (RESOLVED, #308, v0.4.67):** `forge-store` used to wrap a
single SQLite connection in one blocking `std::sync::Mutex`, shared by the agent turn, the background
indexer, and the file watcher — serializing those actors and amplifying the startup hang above. It is
now an **`r2d2` connection pool**: WAL-backed file DBs serve concurrent reads from separate pooled
connections (writes still serialize on SQLite's one-writer rule, waiting on `busy_timeout`); the
in-memory store is pinned to one connection. Covered by an 8-thread concurrency test.

**Status:** fixed + full workspace builds clean; clippy clean; 286 forge-core/forge-provider tests
pass.
</content>
