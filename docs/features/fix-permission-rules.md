# Feature: fine-grained permission rules (allow/ask/deny rule engine)

> Closes the unbuilt half of **FR-10**. Today only the 4 *global* permission **modes**
> exist (`crates/forge-core/src/permission.rs`); the requirement also promises
> "fine-grained per-tool/per-project allow/ask/deny rules" — and `permission.rs:3`
> openly labels that a "planned enhancement". This spec designs that rule engine:
> a layered (user + project) set of rules matching on **tool name + arguments**, each
> resolving to `allow`/`ask`/`deny`, composed *with* (not instead of) the global modes.
> Built-in safety denies (e.g. `rm -rf /`, secret reads) ship on by default. No Rust here —
> design + insertion points only.

---

## 1. Problem (JTBD)

> When I run Forge mostly in `accept-edits`/`bypass` so it stops nagging me, I still want a
> hard floor of "never do *these* things" and a ceiling of "always allow *those* safe
> things", scoped per project, so the agent is fast on routine work but can never `rm -rf`
> my home dir or read my `.env` — without me babysitting every prompt.

The owner's real Claude Code config is the proof this matters: he runs near-fully
auto/bypass but maintains explicit lists —

- **allow**: `cargo`, `git`, `docker`, `ls`, `find`, `grep`, `rg`, `cat`
- **deny**: `rm -rf /`, `rm -rf ~`, reads of `.env`

Forge's modes cannot express any of this. A mode is a single global knob over a coarse
3-value `SideEffect` class (`ReadOnly`/`Write`/`Shell`, `crates/forge-types/src/lib.rs:150`).
Consequences today:

- **No floor.** `bypass` (`permission.rs:22`) allows *everything* — including `rm -rf /` and
  reading secrets. There is no per-argument deny.
- **No ceiling.** In `default` mode every shell command and every write prompts
  (`permission.rs:30`), even `git status` or `cargo check`. The user either suffers the
  prompts or escalates to `bypass` and loses all protection. There is no middle ground.
- **Coarse granularity.** The decision sees only the `SideEffect` enum, never the tool's
  *arguments* (`permission.rs:10` takes `(mode, side_effect)` — no args). `shell { cmd:
  "git status" }` and `shell { cmd: "rm -rf /" }` are indistinguishable to the broker.
- **No import.** The user's existing Claude Code allow/deny lists can't be carried over.

This also blocks the forthcoming **shell tool** spec (`docs/features/shell-tool.md`, sibling
spec — to be written) and the **Assay/bash safety** layer, both of which are specified to
delegate their allow/deny decision to *this* engine rather than re-implementing a parser.

---

## 2. Scope (MoSCoW)

### Must
- A **rule model**: ordered rules matching on `tool` (name/glob) + an arg matcher, each
  resolving to `allow`/`ask`/`deny`.
- **Two config layers** — user (`<config>/forge/config.toml`) and project
  (`./.forge/config.toml`) — merged with **project rules layered over user rules**.
- A **resolution algorithm** combining rules with the global mode, with a precisely defined
  precedence (deny wins; most-specific match wins; explicit beats mode default).
- **Built-in safety deny defaults** active in *every* mode including `bypass`: at minimum
  `rm -rf /`, `rm -rf ~`, and reads/writes of secret files (`.env`, `*.pem`, `id_rsa`,
  `**/.ssh/**`, `**/.aws/credentials`).
- **Argument context** threaded into the decision: the broker must see `tool.name` + the
  call's JSON `args` (`crates/forge-types/src/lib.rs:93`, `ToolCall.args`).
- TOML config schema with concrete examples; defaults-when-no-rules behaviour defined.
- TUI prompt shows **which rule matched / why it's asking** and offers
  *allow once / always allow / deny*.
- Glob matching for paths and shell binaries; safe handling of arg-hidden danger
  (`bash -c "..."`), path normalization + symlink-escape handling, ReDoS-safe regex.

### Should
- **Claude Code import**: map `settings.json` `permissions.allow` / `permissions.deny`
  arrays into Forge rules (a thin importer; references the planned migration layer,
  `docs/architecture/01-requirements.md:68`).
- "Always allow" written back to the **project** layer (session-scoped "allow for session"
  kept in memory only).
- Regex matching (opt-in `regex = "..."`) in addition to glob, behind a compiled,
  size/complexity-bounded matcher.

### Could
- Per-rule `reason` string surfaced verbatim in the prompt and the session log.
- `--explain-permissions <tool> <args>` CLI to dry-run the resolver and print the matched
  rule + decision trace.

### Won't (this iteration)
- Network/host-level firewalling, MCP-server permissions, or per-domain web allowlists
  (post-v0.1; the model is forward-compatible but unscoped here).
- A GUI rules editor. Rules are edited as TOML.
- Time-boxed / count-boxed grants ("allow 3 times"). Only once / session / always.
- Cryptographic signing of project rule files (trust-on-first-use is assumed; see Edge
  cases for the untrusted-project-rules risk).

### Non-goals
- Replacing the global modes. Modes stay; rules *refine* them.
- Sandboxing/`seccomp`/containerization of tool execution — that's the shell-tool spec's
  concern; this engine only decides allow/ask/deny.

---

## 3. Behaviour (Given / When / Then)

Notation: `mode` = global `PermissionMode`; a rule is `(tool, matcher) -> allow|ask|deny`.

### Positive / ceiling
1. **Allow rule auto-approves in `default` mode.**
   *Given* mode `default` and a user rule `shell allow "git *"`,
   *When* the agent calls `shell { cmd: "git status" }`,
   *Then* it runs with **no prompt** (rule `allow` beats the mode's `ask`), logged
   `permission=allowed rule=user:shell:"git *"`.

2. **Allow rule covers writes.**
   *Given* `default` and `write_file allow "src/**"`,
   *When* `write_file { path: "src/main.rs" }`,
   *Then* allowed without prompt.

3. **No matching rule falls back to the mode.**
   *Given* `default` and rules that match neither,
   *When* `shell { cmd: "make deploy" }`,
   *Then* the broker falls back to `decide(mode, side_effect)` → `Ask` (today's behaviour;
   fully backward-compatible).

### Negative / deny / floor
4. **Deny rule overrides `bypass`.**
   *Given* mode `bypass` and the built-in deny `shell deny "rm -rf /"`,
   *When* `shell { cmd: "rm -rf /" }`,
   *Then* **denied** regardless of mode; tool never runs; logged
   `permission=denied rule=builtin:shell:"rm -rf /"`. (Contrast: `permission.rs:22` allows
   it today.)

5. **Secret read denied in every mode.**
   *Given* any mode and built-in `read_file deny "**/.env"`,
   *When* `read_file { path: "./.env" }` (note `ReadOnly` — *currently never even checked*,
   `permission.rs:14`),
   *Then* **denied**. This requires routing read-only tools through the rule engine when
   built-in/secret denies exist (see §6 insertion point).

6. **Deny beats allow on conflict.**
   *Given* `shell allow "git *"` and `shell deny "git push *"`,
   *When* `shell { cmd: "git push origin main" }`,
   *Then* **denied** — deny wins over an equally/less specific allow.

7. **Most-specific allow beats broad ask.**
   *Given* `shell ask "*"` and `shell allow "cargo test*"`,
   *When* `shell { cmd: "cargo test" }`,
   *Then* allowed (longer/more-specific glob wins over `*`).

8. **Arg-hidden danger is unwrapped before matching.**
   *Given* built-in deny `rm -rf /`,
   *When* `shell { cmd: "bash -c 'rm -rf /'" }`,
   *Then* the matcher inspects the *effective* command (see §7 "command extraction") and
   **denies**. If the wrapper can't be safely parsed, the decision degrades to `ask`/`deny`
   per the conservative-fallback rule (§7), never silent allow.

### Layering / precedence
9. **Project overrides user.**
   *Given* user `shell allow "docker *"` and project `shell deny "docker *"`,
   *When* `shell { cmd: "docker run ..." }`,
   *Then* **denied** — project layer wins on the *same* tool+pattern; and even if it
   didn't, deny would win.

10. **Empty config = pure mode behaviour.**
    *Given* no user and no project rules (and built-ins),
    *When* any tool call,
    *Then* identical to today except the built-in safety denies still apply; all current
    `permission.rs` tests must still pass.

### Import
11. **Claude Code lists import to rules.**
    *Given* a `settings.json` with `permissions.allow: ["Bash(cargo:*)", "Bash(git:*)"]`
    and `permissions.deny: ["Read(.env)", "Bash(rm -rf /)"]`,
    *When* the importer runs,
    *Then* it emits Forge rules `shell allow "cargo *"`, `shell allow "git *"`,
    `read_file deny "**/.env"`, `shell deny "rm -rf /"` into the chosen layer.

### TUI
12. **Prompt explains itself and offers persistence.**
    *Given* a call resolving to `ask` via rule `shell ask "make *"`,
    *When* the prompt shows,
    *Then* it states the tool, the args, **the matched rule and why**, and offers
    `[y] allow once  [a] always allow  [n] deny`. Choosing `a` writes an `allow` rule to the
    project layer; `y` allows only this call; `n`/Esc denies.

---

## 4. Impact analysis & insertion points (file:line)

| Where | File:line | Change |
|---|---|---|
| Decision signature | `crates/forge-core/src/permission.rs:10` | `decide` gains the rule set + tool name + args. New signature e.g. `decide(mode, side_effect, tool_name, args, &RuleSet) -> (PermissionDecision, MatchInfo)`. Keep the old `decide(mode, side_effect)` as an internal `decide_mode` fallback so the existing mode tests at `permission.rs:39-75` survive unchanged. |
| ReadOnly short-circuit | `crates/forge-core/src/permission.rs:14` | Today `ReadOnly` returns `Allow` before any rule check. Must now consult **deny** rules first (so secret *reads* can be denied) and **allow**, then keep the `ReadOnly→Allow` default if nothing matched. |
| Broker call site | `crates/forge-core/src/lib.rs:317` | `permission::decide(self.mode, side_effect)` becomes `permission::decide(self.mode, side_effect, &call.name, &call.args, &self.rules)`. The returned `MatchInfo` flows into both the `confirm` call (line 320) and the `record_tool_call` log (line 338). |
| Session state | `crates/forge-core/src/lib.rs:33-45` | Add `rules: RuleSet` field to `Session`; populate it in `build` (`lib.rs:111-123`) from `config` (next row). Also add session-scoped in-memory "allow for session" overlay. |
| Config schema | `crates/forge-config/src/lib.rs:30-36` | Add `permissions: PermissionsConfig` to `Config`. New struct holds ordered `rules: Vec<RuleConfig>` (+ optional per-tool sub-tables). Defaults merged via the existing figment stack (`lib.rs:98-107`) so project `./.forge/config.toml` overlays user config automatically — layering comes for free. |
| Built-in defaults | `crates/forge-config/src/lib.rs:57-78` (`Config::default`) | Seed the **built-in safety deny rules** here (or a dedicated `builtin_deny_rules()` fn) so they exist even with zero user/project config and *before* any user merge — they must not be overridable by lowering precedence (see §7 builtin precedence). |
| Args context | `crates/forge-types/src/lib.rs:93-100` | `ToolCall.args` already carries the JSON args — no type change strictly required; the broker just needs to pass `&call.args`. `SideEffect` (`lib.rs:150`) stays as-is (rules key on tool name, not on a richer side-effect). Add a small `Rule`/`RuleDecision`/`MatchInfo` type set — place in `forge-types` (leaf crate, `lib.rs:1-4`) so config, core and tui can all name them without a cycle. |
| Presenter signature | `crates/forge-tui/src/lib.rs:54-60` | `confirm(&mut self, tool: &str, side_effect: SideEffect)` gains `MatchInfo` (matched rule + reason + offered choices) and returns a richer `ConfirmOutcome { AllowOnce, AllowAlways, Deny }` instead of `bool`. Update both impls: `HeadlessPresenter::confirm` (`lib.rs:124`) and `TuiPresenter::confirm` (`crates/forge-tui/src/tui.rs:97`), plus the test stub at `crates/forge-core/src/lib.rs:378` and the driver path `crates/forge-tui/src/driver.rs:26,47`. |
| TUI prompt render | `crates/forge-tui/src/tui.rs:97-116` & `App.prompt` (`crates/forge-tui/src/app.rs`, the `prompt: Option<String>` field) | Render the matched-rule/why line + the three-way key hints; handle `a` (always) in the key loop (`tui.rs:102-107`) in addition to `y`/`n`. |
| "Always allow" writeback | `crates/forge-config/src/lib.rs` (new fn) | On `AllowAlways`, append an `allow` rule to `./.forge/config.toml`. Core invokes it from `invoke_tool` after a positive `AllowAlways` outcome. |
| Import layer | new `crates/forge-config` module (or the planned migration crate, ref `docs/architecture/01-requirements.md:68`) | `import_claude_code(settings_json) -> Vec<RuleConfig>`. Pure mapping; no core change beyond exposing the importer to the CLI. |
| Shell-tool / Assay | `docs/features/shell-tool.md` (sibling spec) | That spec MUST call this engine for its allow/deny gate rather than parse commands itself; this spec owns the command-extraction logic (§7) it relies on. |

No DB schema change: `record_tool_call` (`crates/forge-core/src/lib.rs:338`) already stores a
`permission_label`; widen the value space to include the matched rule id (e.g.
`"allowed:user:shell:git *"`).

---

## 5. Technical design — rule model

### 5.1 Types (proposed, in `forge-types`)

```
RuleDecision = Allow | Ask | Deny           // distinct from PermissionDecision? — reuse it.

Rule {
    tool:    ToolMatch,        // exact name or glob, e.g. "shell", "write_file", "*"
    matcher: ArgMatcher,       // how to match the call's args
    decision: PermissionDecision,
    source:  RuleSource,       // Builtin | User | Project | Session
    reason:  Option<String>,   // shown in the prompt + logged
}

ArgMatcher =
  | Any                                   // matches any args for this tool
  | ShellCommand { glob | regex }         // matched against the *effective* command(s)
  | Path        { glob }                  // matched against the normalized path arg

MatchInfo { rule: Option<Rule>, decision: PermissionDecision, via_mode: bool }
```

`RuleSet` is the resolved, ordered list: `builtin_denies ++ user_rules ++ project_rules ++
session_rules`, each tagged with its `source`.

### 5.2 Matching a single rule against a call

1. `tool` must match `call.name` (exact or glob).
2. The `ArgMatcher` runs against the relevant arg:
   - **Path tools** (`read_file`/`write_file`/`edit_file`/`list_dir`): take the `path` arg,
     **normalize** it (resolve `.`/`..`, expand `~`, canonicalize against `cwd`, resolve
     symlinks to their real target), then glob-match. Matching happens on the *canonical*
     path so `./foo/../.env`, `~/.env`, and a symlink to `.env` all match `**/.env`.
   - **Shell tool**: extract the **effective command list** (see §7) and match the rule's
     glob/regex against *each*; a deny matches if **any** extracted command matches; an
     allow requires the matcher to cover the command(s) it's evaluated against.
3. A non-matching tool or matcher → rule does not apply.

### 5.3 Resolution algorithm (the precedence contract)

Given a call `(tool_name, args, side_effect)`, mode `M`, and `RuleSet R`:

1. **Collect matches.** Find every rule in `R` that matches the call. Record each with its
   `source` and a **specificity score** (see §5.4).
2. **Built-in deny floor.** If any matching rule has `source = Builtin` and
   `decision = Deny` → **Deny**. (Cannot be overridden by anything, including `bypass` and
   project `allow`.) Stop.
3. **Deny precedence.** Else if any matching rule (User/Project/Session) is `Deny` → **Deny**.
   Stop. *(Deny always beats allow/ask among non-builtin rules.)*
4. **Most-specific wins among allow/ask.** Among remaining matches (all `Allow`/`Ask`), pick
   the single most-specific (§5.4). Tie-break by layer: **Session > Project > User**. The
   winner's decision is the result — `Allow` runs without prompt; `Ask` prompts. Stop.
5. **No rule matched → fall back to the mode.** Return `decide_mode(M, side_effect)` (the
   current `permission.rs` logic, unchanged), then apply the existing `ReadOnly→Allow`
   default. This guarantees Given/When/Then #3 and #10.

Plain-language invariants:
- **deny > allow > ask > mode-default**, with **builtin-deny** above all.
- An explicit `allow` rule auto-approves even in `default` mode (#1).
- An explicit `deny` rule blocks even in `bypass` (#4).
- `plan` mode still denies all side effects *unless* a rule explicitly... — **No.** `plan` is
  a hard read-only contract: in `plan`, step 5's `decide_mode` returns `Deny` for any
  side effect, and an `allow` rule does **not** resurrect it. Resolution order:
  built-in-deny → explicit-deny → (if mode is `plan` and side_effect ≠ ReadOnly) **Deny** →
  most-specific allow/ask → mode default. I.e. `plan` sits *between* explicit-deny and
  explicit-allow so `plan` cannot be escaped by an allow rule. This is called out because it
  is the one place "allow auto-approves" does **not** hold; document it loudly.

Revised canonical order (single source of truth):

```
1. builtin Deny match      -> Deny
2. any Deny match          -> Deny
3. mode == Plan && side-effecting -> Deny
4. most-specific Allow/Ask match -> that decision
5. fallback: decide_mode(mode, side_effect)
```

### 5.4 Specificity (most-specific match wins)

Score a matcher so ambiguity is resolved deterministically:
- Exact tool name (`"shell"`) > tool glob (`"*"`): +1000.
- Arg matcher specificity = literal-character count of the glob/regex with wildcards
  removed (e.g. `"git push *"` (8) > `"git *"` (4) > `"*"` (0)).
- `Any` matcher scores 0 on args.
- Final tie → layer order Session > Project > User > Builtin (builtin only appears as deny,
  already resolved). If *still* tied (same pattern, same layer) it's a config error: keep
  the **last** occurrence and emit a load-time warning (see Edge cases).

### 5.5 Config schema (TOML)

User `<config>/forge/config.toml` or project `./.forge/config.toml`:

```toml
permission_mode = "accept-edits"          # existing global mode (FR-10)

# Ordered, but resolution is by specificity/precedence — not file order — except for
# exact-duplicate tie-break (last wins). Keep readable; don't rely on order for safety.
[[permissions.rules]]
tool     = "shell"
allow    = "git *"                        # glob over the effective command
reason   = "version control is safe"

[[permissions.rules]]
tool     = "shell"
allow    = ["cargo *", "ls *", "find *", "grep *", "rg *", "cat *", "docker *"]

[[permissions.rules]]
tool     = "shell"
deny     = ["rm -rf /", "rm -rf ~", "rm -rf /*"]

[[permissions.rules]]
tool     = "shell"
ask      = "*"                            # anything not matched above -> prompt

[[permissions.rules]]
tool     = "write_file"
allow    = "src/**"

[[permissions.rules]]
tool     = "read_file"
deny     = ["**/.env", "**/*.pem", "**/id_rsa", "**/.ssh/**", "**/.aws/credentials"]

# Opt-in regex form (compiled with a bounded engine; see ReDoS handling)
[[permissions.rules]]
tool     = "shell"
deny_regex = '^sudo\b'
```

Each `[[permissions.rules]]` block has exactly one of `allow`/`ask`/`deny`
(or the `*_regex` variant); a string or array of strings. Mixing `allow` and `deny` in one
block is a load error.

**Built-in defaults** (shipped, present even with empty config, source = `Builtin`, deny,
non-overridable):

```toml
# conceptually injected before any user/project rule
shell      deny "rm -rf /", "rm -rf ~", "rm -rf /*", ":(){ :|:& };:"   # fork bomb
read_file  deny "**/.env", "**/*.pem", "**/id_rsa", "**/id_ed25519", "**/.ssh/**", "**/.aws/credentials", "**/.git-credentials"
write_file deny "**/.ssh/**", "/etc/**"
edit_file  deny (same as write_file)
```

### 5.6 Claude Code import mapping

`settings.json` shape: `{ "permissions": { "allow": [...], "deny": [...] } }`. Entries look
like `Bash(cargo:*)`, `Read(./.env)`, `Edit(src/**)`.

| Claude Code entry | Forge rule |
|---|---|
| `Bash(<cmd>:*)` / `Bash(<cmd> ...)` | `shell` + glob `"<cmd> *"` |
| `Bash(rm -rf /)` | `shell` deny/allow `"rm -rf /"` (verbatim) |
| `Read(<glob>)` | `read_file` + `Path` glob (also `list_dir`) |
| `Edit(<glob>)` / `Write(<glob>)` | `edit_file` + `write_file` + `Path` glob |
| `WebFetch(...)`, MCP tools | **skipped** with a warning (out of scope this iteration) |

The `allow`/`deny` array decides the rule decision. Unparseable entries are skipped with a
warning, never silently dropped. Importer lives in the config/migration layer
(`docs/architecture/01-requirements.md:68`); output is appended to the chosen layer's TOML.

### 5.7 TUI prompt mockup

Current prompt (`tui.rs:98`): `allow {tool} ({side_effect:?})?`. New:

```
┌ permission ─────────────────────────────────────────────────┐
│ shell  cargo run --release                                   │
│ matched: ask  rule "shell: *"  (no specific rule)            │
│                                                              │
│ [y] allow once   [a] always allow   [n] deny                 │
└──────────────────────────────────────────────────────────────┘
```

For a deny that *blocked* (no prompt, shown as a notice in the stream, not a question):

```
  ✗ shell  rm -rf /   denied by builtin safety rule "rm -rf /"
```

Headless (`lib.rs:129`) line form:
`⚠ allow shell "cargo run"? matched ask rule "*" [y]es/[a]lways/[N]o`. Non-interactive
(`lib.rs:125`) keeps denying, but a `deny`-by-rule prints the matched rule for debuggability.

---

## 6. Read-only routing note

Because secret *reads* must be deniable (#5), the `ReadOnly` early-return at
`permission.rs:14` can no longer unconditionally `Allow`. Design: when the `RuleSet`
contains **any** rule (builtin or user) that could match a read-only tool, route read-only
calls through resolution; otherwise keep the fast `Allow` path. Since built-in secret denies
always exist, in practice read-only calls always run resolution — but resolution for a
read with no matching deny still returns `Allow` at step 4/5, so the hot path stays cheap
(glob checks only against the handful of secret patterns).

---

## 7. Edge cases

| # | Edge case | Handling |
|---|---|---|
| E1 | **Conflicting rules** (allow + deny both match) | Deny wins (resolution step 2/3). Documented, tested (#6). |
| E2 | **Exact-duplicate rules** (same tool+pattern+layer) | Last occurrence wins; emit a load-time warning naming the file + pattern. Never panic. |
| E3 | **Overly broad glob** (`shell allow "*"` in project) | Allowed but flagged: load-time warning when an `allow` rule's effective specificity is 0 (pure wildcard) on a `Shell`/`Write` tool. Built-in denies still override it, so `*` allow can never resurrect `rm -rf /`. |
| E4 | **Regex injection / ReDoS** | Regex is opt-in (`*_regex`). Compile with a linear-time engine (Rust `regex` crate is already linear/no-backtracking) and a `size_limit`/`dfa_size_limit`. Reject patterns that fail to compile or exceed the limit at **load time** with a warning; that rule is dropped, not applied as allow. No user-call ever compiles a regex (only config does). |
| E5 | **Arg-hidden danger** (`bash -c "rm -rf /"`, `sh -lc`, `xargs rm`, `env X=1 rm`, `eval`) | **Command extraction**: for shell calls, parse the command line (shell-words), strip leading `env`/`nice`/`nohup`/`time` wrappers, and for `bash -c`/`sh -c`/`-lc` recursively extract the quoted script and match against *its* commands too. Split on `;`, `&&`, `||`, `|`. Match denies against **every** extracted segment. If parsing fails or nesting exceeds a small depth cap → **do not allow**: fall back to `ask` (or `deny` if a builtin-deny token like `rm -rf` appears anywhere by substring). Never silent-allow an unparseable command. |
| E6 | **Path normalization / symlink escape** | Normalize before matching (§5.2): expand `~`, resolve `.`/`..`, canonicalize against `cwd`, resolve symlinks to the real target. Match the secret-deny globs against the canonical path so a symlink `link -> ~/.aws/credentials` or `../../etc/shadow` cannot evade a deny. A path that escapes `cwd` upward is additionally flagged for `Write`/`Edit` tools. |
| E7 | **Precedence ambiguity** (two equally-specific allow/ask) | Deterministic tie-break: layer (Session>Project>User), then last-wins + warning. Resolution is total — never undefined. |
| E8 | **Empty / no rules** | Built-in denies still apply; everything else falls to mode behaviour. Identical to today otherwise (#10); existing `permission.rs` tests pass. |
| E9 | **Untrusted project rules** (a cloned repo ships `./.forge/config.toml` with `shell allow "*"`) | Project rules can only *widen to ask/allow within mode limits and never below the builtin-deny floor*; they cannot weaken built-in denies. Additionally: on first load of a project config that contains `allow` rules, warn the user (TUI notice) that the project supplies permission rules, with a one-time confirmation before they take effect (TOFU). Project rules can never escalate the **mode** itself. |
| E10 | **Glob vs regex confusion** | Distinct keys (`allow` = glob, `allow_regex` = regex). A `*` in a glob is "any chars"; documented. No silent reinterpretation. |
| E11 | **Tool not in registry** | Handled upstream (`lib.rs:299` unknown-tool path) before resolution; rules referencing unknown tools warn at load but are harmless. |
| E12 | **`AllowAlways` writeback races / read-only FS** | Writeback to `./.forge/config.toml` is best-effort; on failure, downgrade to session-scoped allow and warn. The grant still applies for the session. |

---

## 8. Definition of done

- `decide` consumes mode + side_effect + tool name + args + `RuleSet` and returns
  decision + `MatchInfo`; old mode-only logic preserved as `decide_mode` and its tests at
  `permission.rs:39-75` still pass unchanged.
- Resolution implements the canonical 5-step order (§5.3) including the `Plan`-between-deny-
  and-allow rule; unit tests cover every Given/When/Then in §3 (esp. #4 deny-beats-bypass,
  #5 secret-read-deny, #6 deny-beats-allow, #7 specificity, #8 `bash -c` unwrap, #9 layering,
  #10 empty-config-parity).
- Built-in safety denies are present with zero config and **cannot** be overridden by any
  user/project rule or by `bypass`; a test asserts `bypass` + `rm -rf /` → Deny.
- Config schema parses the §5.5 TOML through the existing figment stack with project
  overlaying user (`forge-config/src/lib.rs:98-107`); a malformed rule block is a load error
  with a clear message; duplicate/over-broad rules warn but don't fail.
- Command-extraction (§7 E5) unwraps `bash -c`, wrapper binaries, and `;`/`&&`/`|` chains;
  path normalization (E6) defeats `..`, `~`, and symlink escapes for secret denies — both
  tested with adversarial inputs.
- Regex matchers are bounded (E4); a pathological pattern is rejected at load, not at call.
- TUI prompt shows tool + args + matched rule + why and offers once/always/deny;
  `AllowAlways` writes to the project layer (or session overlay on FS failure); headless and
  non-interactive paths updated; the test stub at `lib.rs:378` and `driver.rs:47` updated.
- Claude Code importer maps the §5.6 table; unparseable entries warn and are skipped; a
  fixture `settings.json` round-trips to the expected rules.
- `docs/features/shell-tool.md` (when written) and the Assay/bash-safety layer reference this
  engine as the single decision point; this spec owns command-extraction they depend on.
- `permission.rs:3` doc comment updated from "planned enhancement" to describe the shipped
  engine; FR-10 (`docs/architecture/01-requirements.md:56-60`) marked fully satisfied.
