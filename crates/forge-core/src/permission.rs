//! The permission broker (ADR-0008): the single chokepoint that decides whether a tool may
//! run. Two layers compose here:
//!
//! 1. **Global modes** ([`decide_mode`]) — the coarse `default`/`accept-edits`/`bypass`/`plan`
//!    posture over a tool's [`SideEffect`] class.
//! 2. **Fine-grained rules** ([`decide`], FR-10) — ordered allow/ask/deny rules matching a
//!    tool by name + argument pattern, layered *on top of* the modes.
//!
//! Precedence (single source of truth):
//! 1. a `Builtin` deny match → Deny (safety floor — beats every mode incl. `bypass`)
//! 2. any deny match → Deny
//! 3. `plan` mode + a side effect → Deny (read-only contract; an allow rule cannot escape it)
//! 4. the most-specific allow/ask match → that decision
//! 5. otherwise fall back to the mode ([`decide_mode`]).

use forge_types::{PermissionDecision, PermissionMode, PermissionRule, RuleSource, SideEffect};
use serde_json::Value;

/// Decide the outcome from the global mode alone (the pre-FR-10 behaviour). Retained as the
/// fallback when no rule matches, so the mode contract is unchanged.
pub fn decide_mode(mode: PermissionMode, side_effect: SideEffect) -> PermissionDecision {
    use PermissionDecision::*;

    // Read-only tools never prompt, regardless of mode.
    if side_effect == SideEffect::ReadOnly {
        return Allow;
    }

    match mode {
        // Read-only session: no side effects at all.
        PermissionMode::Plan => Deny,
        // Explicit, deliberate "do anything".
        PermissionMode::Bypass => Allow,
        // Auto-allow edits, still gate shell. A network read is low-risk → allow. An external
        // (untrusted MCP) call is at least as risky as shell → still ask, even in accept-edits.
        PermissionMode::AcceptEdits => match side_effect {
            SideEffect::Write => Allow,
            SideEffect::Shell => Ask,
            SideEffect::ReadOnly => Allow,
            SideEffect::Network => Allow,
            SideEffect::External => Ask,
        },
        // Safe default: confirm any side effect.
        PermissionMode::Default => Ask,
    }
}

/// Decide the outcome for a tool call, composing fine-grained `rules` with the global `mode`.
pub fn decide(
    mode: PermissionMode,
    side_effect: SideEffect,
    tool_name: &str,
    args: &Value,
    rules: &[PermissionRule],
) -> PermissionDecision {
    use PermissionDecision::*;

    let matched: Vec<&PermissionRule> = rules
        .iter()
        .filter(|r| rule_matches(r, tool_name, args))
        .collect();

    // 1. Built-in safety floor — unoverridable, even by `bypass` or a project `allow`.
    if matched
        .iter()
        .any(|r| r.source == RuleSource::Builtin && r.decision == Deny)
    {
        return Deny;
    }
    // 2. Any explicit deny beats any allow/ask.
    if matched.iter().any(|r| r.decision == Deny) {
        return Deny;
    }
    // 3. `plan` is a hard read-only contract: a side effect is denied and no allow escapes it.
    if mode == PermissionMode::Plan && side_effect != SideEffect::ReadOnly {
        return Deny;
    }
    // 4. Most-specific allow/ask wins.
    if let Some(rule) = matched
        .iter()
        .filter(|r| r.decision != Deny)
        .max_by_key(|r| specificity(r))
    {
        return rule.decision;
    }
    // 5. No rule applies: fall back to the global mode.
    decide_mode(mode, side_effect)
}

/// Does this rule apply to the call? Tool name must match (exact or `*`) and, if the rule
/// carries patterns, at least one must match the relevant argument.
fn rule_matches(rule: &PermissionRule, tool_name: &str, args: &Value) -> bool {
    if rule.tool != "*" && rule.tool != tool_name {
        return false;
    }
    if rule.patterns.is_empty() {
        return true; // matches any args for this tool
    }
    // Shell tool: match against every effective command extracted from the command line.
    if is_shell_tool(tool_name) {
        let cmd = args.get("command").and_then(Value::as_str).unwrap_or("");
        let (segments, parsed_ok) = effective_commands(cmd);
        let any_glob = segments
            .iter()
            .any(|seg| rule.patterns.iter().any(|p| shell_match(p, seg)));
        if any_glob {
            return true;
        }
        if rule.source == RuleSource::Builtin {
            // Built-in denies also match against the *raw* command line. This catches
            // pipe-to-shell (`curl … | sh`) that segment-splitting would separate, and
            // other obfuscation the per-segment match misses.
            if rule.patterns.iter().any(|p| shell_match(p, cmd)) {
                return true;
            }
            // Conservative floor: if extraction was imperfect, still catch a literal deny
            // token hidden anywhere in the raw command (e.g. `bash -c 'rm -rf /'`).
            if !parsed_ok || segments.len() > 1 {
                return rule
                    .patterns
                    .iter()
                    .any(|p| !p.contains('*') && cmd.contains(p.as_str()));
            }
        }
        return false;
    }
    // Path tools: match against the path arg and its normalized variants.
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let candidates = path_candidates(path);
        return rule
            .patterns
            .iter()
            .any(|p| candidates.iter().any(|c| path_match(p, c)));
    }
    // Generic tools (MCP server tools, etc.): a bare "*" means "match any args". More specific
    // patterns are intentionally not supported for non-shell/non-path tools — the only meaningful
    // distinction users make is "deny any call to this tool" (deny = "*") vs "deny for specific
    // args" (which requires tool-specific pattern support we don't have yet).
    if rule.patterns.iter().any(|p| p == "*") {
        return true;
    }
    false
}

fn is_shell_tool(tool_name: &str) -> bool {
    matches!(tool_name, "shell" | "bash" | "run")
}

/// Specificity score so the most-specific match wins deterministically (spec §5.4).
/// Exact tool name beats a `*` tool glob; among args, more literal characters wins.
fn specificity(rule: &PermissionRule) -> usize {
    let tool_score = if rule.tool == "*" { 0 } else { 1000 };
    let arg_score = rule
        .patterns
        .iter()
        .map(|p| p.chars().filter(|c| !matches!(c, '*' | '?')).count())
        .max()
        .unwrap_or(0);
    tool_score + arg_score
}

/// Extract the effective command(s) from a shell command line so that arg-hidden danger
/// (`bash -c '...'`, wrapper binaries, `;`/`&&`/`|` chains) is unwrapped before matching.
/// Returns the segments and whether parsing fully succeeded.
fn effective_commands(cmd: &str) -> (Vec<String>, bool) {
    let mut out = Vec::new();
    let mut ok = true;
    collect_commands(cmd, 0, &mut out, &mut ok);
    if out.is_empty() {
        out.push(cmd.trim().to_string());
    }
    (out, ok)
}

fn collect_commands(cmd: &str, depth: usize, out: &mut Vec<String>, ok: &mut bool) {
    if depth > 4 {
        *ok = false;
        out.push(cmd.trim().to_string());
        return;
    }
    // Split on shell operators into segments, then normalize each.
    for raw in split_operators(cmd) {
        let seg = raw.trim();
        if seg.is_empty() {
            continue;
        }
        let Some(tokens) = shell_words::split(seg).ok().filter(|t| !t.is_empty()) else {
            *ok = false;
            out.push(seg.to_string());
            continue;
        };
        let stripped = strip_wrappers(&tokens);
        // `bash -c "<script>"` / `sh -lc "<script>"`: recurse into the inner script.
        if let Some(inner) = inner_script(&stripped) {
            collect_commands(&inner, depth + 1, out, ok);
            continue;
        }
        out.push(stripped.join(" "));
    }
}

/// Split a command line on `;`, `&&`, `||`, `|` (outside of quotes).
fn split_operators(cmd: &str) -> Vec<String> {
    let mut segs = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    let bytes: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' => {
                    segs.push(std::mem::take(&mut cur));
                }
                '&' | '|' => {
                    // consume a possible doubled operator (&& / ||) and a single | .
                    segs.push(std::mem::take(&mut cur));
                    if i + 1 < bytes.len() && bytes[i + 1] == c {
                        i += 1;
                    }
                }
                _ => cur.push(c),
            },
        }
        i += 1;
    }
    if !cur.trim().is_empty() {
        segs.push(cur);
    }
    segs
}

/// Drop leading no-op wrapper binaries so `env X=1 nice rm ...` matches `rm ...`.
fn strip_wrappers(tokens: &[String]) -> Vec<String> {
    let mut i = 0;
    while i < tokens.len() {
        match tokens[i].as_str() {
            "nohup" | "nice" | "time" | "command" | "builtin" | "exec" => i += 1,
            // `env` followed by VAR=VAL assignments
            "env" => {
                i += 1;
                while i < tokens.len() && tokens[i].contains('=') && !tokens[i].starts_with('-') {
                    i += 1;
                }
            }
            _ => break,
        }
    }
    tokens[i..].to_vec()
}

/// If the command is `bash -c "<script>"` / `sh -lc "<script>"` / `cmd /C "<command>"` etc.,
/// return the inner script so catastrophic-deny patterns can be checked recursively.
fn inner_script(tokens: &[String]) -> Option<String> {
    if tokens.len() < 3 {
        return None;
    }
    let bin = std::path::Path::new(&tokens[0])
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&tokens[0]);
    if matches!(bin, "bash" | "sh" | "zsh" | "dash") {
        // find a `-c` (possibly combined like `-lc`) and take the following token as the script.
        for (i, t) in tokens.iter().enumerate().skip(1) {
            if t.starts_with('-') && t.contains('c') {
                return tokens.get(i + 1).cloned();
            }
        }
        return None;
    }
    // Windows: `cmd /C <command>` — everything after /C is the inner command.
    if bin.eq_ignore_ascii_case("cmd") {
        for (i, t) in tokens.iter().enumerate().skip(1) {
            if t.eq_ignore_ascii_case("/C") && i + 1 < tokens.len() {
                return Some(tokens[i + 1..].join(" "));
            }
        }
    }
    None
}

/// Lexically normalized candidate forms of a path for matching secret-deny globs against:
/// the raw path, a `~`-expanded form, and a `.`/`..`-collapsed form. (Symlink resolution is
/// intentionally out of scope for this iteration; see the spec edge-case table.)
fn path_candidates(path: &str) -> Vec<String> {
    let mut out = vec![path.to_string()];
    let expanded = if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
            format!("{home}/{rest}")
        } else {
            path.to_string()
        }
    } else {
        path.to_string()
    };
    if expanded != path {
        out.push(expanded.clone());
    }
    let cleaned = lexical_clean(&expanded);
    if !out.contains(&cleaned) {
        out.push(cleaned);
    }
    out
}

/// Collapse `.` and `..` segments lexically (no filesystem access).
fn lexical_clean(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut stack: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if matches!(stack.last(), Some(&s) if s != "..") {
                    stack.pop();
                } else if !absolute {
                    stack.push("..");
                }
            }
            p => stack.push(p),
        }
    }
    let joined = stack.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// Glob match for file paths: `*` does not cross `/`, `**` does (delegated to `globset`).
fn path_match(pattern: &str, path: &str) -> bool {
    match globset::Glob::new(pattern) {
        Ok(g) => g.compile_matcher().is_match(path),
        Err(_) => pattern == path,
    }
}

/// Wildcard match for shell commands: `*` matches any run of characters (including `/`),
/// `?` matches one. Linear time (no backtracking blowup).
fn shell_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_types::PermissionDecision::*;
    use serde_json::json;

    // ---- mode-only behaviour (unchanged; was `decide`, now `decide_mode`) ----

    #[test]
    fn read_only_always_allowed() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Bypass,
            PermissionMode::Plan,
        ] {
            assert_eq!(decide_mode(mode, SideEffect::ReadOnly), Allow);
        }
    }

    #[test]
    fn plan_denies_all_side_effects() {
        assert_eq!(decide_mode(PermissionMode::Plan, SideEffect::Write), Deny);
        assert_eq!(decide_mode(PermissionMode::Plan, SideEffect::Shell), Deny);
    }

    #[test]
    fn accept_edits_allows_write_asks_shell() {
        assert_eq!(
            decide_mode(PermissionMode::AcceptEdits, SideEffect::Write),
            Allow
        );
        assert_eq!(
            decide_mode(PermissionMode::AcceptEdits, SideEffect::Shell),
            Ask
        );
    }

    #[test]
    fn bypass_allows_everything() {
        assert_eq!(
            decide_mode(PermissionMode::Bypass, SideEffect::Shell),
            Allow
        );
    }

    #[test]
    fn default_asks_for_side_effects() {
        assert_eq!(decide_mode(PermissionMode::Default, SideEffect::Write), Ask);
        assert_eq!(decide_mode(PermissionMode::Default, SideEffect::Shell), Ask);
    }

    #[test]
    fn network_is_gated_per_mode() {
        use PermissionMode::*;
        assert_eq!(decide_mode(Plan, SideEffect::Network), Deny);
        assert_eq!(decide_mode(Default, SideEffect::Network), Ask);
        assert_eq!(decide_mode(AcceptEdits, SideEffect::Network), Allow);
        assert_eq!(decide_mode(Bypass, SideEffect::Network), Allow);
    }

    #[test]
    fn external_mcp_is_gated_per_mode() {
        // An MCP call is untrusted: denied in plan, asked in default AND accept-edits (unlike a
        // benign network read), only auto-allowed by the explicit bypass temper (mcp-client.md §6).
        use PermissionMode::*;
        assert_eq!(decide_mode(Plan, SideEffect::External), Deny);
        assert_eq!(decide_mode(Default, SideEffect::External), Ask);
        assert_eq!(decide_mode(AcceptEdits, SideEffect::External), Ask);
        assert_eq!(decide_mode(Bypass, SideEffect::External), Allow);
    }

    // ---- helpers ----

    fn rule(
        tool: &str,
        decision: PermissionDecision,
        src: RuleSource,
        pats: &[&str],
    ) -> PermissionRule {
        PermissionRule {
            tool: tool.to_string(),
            patterns: pats.iter().map(|s| s.to_string()).collect(),
            decision,
            source: src,
            reason: None,
        }
    }
    fn builtin_deny(tool: &str, pats: &[&str]) -> PermissionRule {
        rule(tool, Deny, RuleSource::Builtin, pats)
    }
    fn cfg(tool: &str, d: PermissionDecision, pats: &[&str]) -> PermissionRule {
        rule(tool, d, RuleSource::Configured, pats)
    }
    fn shell(cmd: &str) -> Value {
        json!({ "command": cmd })
    }
    fn path(p: &str) -> Value {
        json!({ "path": p })
    }

    // ---- spec §3 Given/When/Then ----

    #[test]
    fn gwt1_allow_rule_auto_approves_in_default_mode() {
        let rules = [cfg("shell", Allow, &["git *"])];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("git status"),
                &rules
            ),
            Allow
        );
    }

    #[test]
    fn gwt2_allow_rule_covers_writes() {
        let rules = [cfg("write_file", Allow, &["src/**"])];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Write,
                "write_file",
                &path("src/main.rs"),
                &rules
            ),
            Allow
        );
    }

    #[test]
    fn gwt3_no_match_falls_back_to_mode() {
        let rules = [cfg("shell", Allow, &["git *"])];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("make deploy"),
                &rules
            ),
            Ask
        );
    }

    #[test]
    fn gwt4_builtin_deny_overrides_bypass() {
        let rules = [builtin_deny("shell", &["rm -rf /"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("rm -rf /"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn gwt5_secret_read_denied_in_every_mode() {
        let rules = [builtin_deny("read_file", &["**/.env"])];
        for mode in [PermissionMode::Default, PermissionMode::Bypass] {
            assert_eq!(
                decide(
                    mode,
                    SideEffect::ReadOnly,
                    "read_file",
                    &path("./.env"),
                    &rules
                ),
                Deny,
                "secret read must be denied in {mode:?}"
            );
        }
    }

    #[test]
    fn builtin_secret_denylist_covers_env_variants_and_keys() {
        // Exercises the REAL forge_config::builtin_deny_rules() end-to-end through decide(), so the
        // expanded secret list is actually enforced (not just present as strings).
        let rules = forge_config::builtin_deny_rules();
        for p in [
            "./.env.local", // dotenv variant — the gap `**/.env` alone missed
            "./.env.production",
            "config/.env.staging",
            "/home/u/.ssh/id_ecdsa",
            "secrets/server.key",
            "./.npmrc",
            "./.netrc",
            "/home/u/.kube/config",
            "/home/u/.config/gcloud/credentials.db",
        ] {
            assert_eq!(
                decide(
                    PermissionMode::Bypass,
                    SideEffect::ReadOnly,
                    "read_file",
                    &path(p),
                    &rules
                ),
                Deny,
                "secret read must be denied even in bypass: {p}"
            );
        }
        // A normal source file is not swept up by the broadened globs.
        assert_ne!(
            decide(
                PermissionMode::Default,
                SideEffect::ReadOnly,
                "read_file",
                &path("./src/main.rs"),
                &rules
            ),
            Deny,
            "ordinary source must remain readable"
        );
    }

    #[test]
    fn gwt6_deny_beats_allow_on_conflict() {
        let rules = [
            cfg("shell", Allow, &["git *"]),
            cfg("shell", Deny, &["git push *"]),
        ];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("git push origin main"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn gwt7_most_specific_allow_beats_broad_ask() {
        let rules = [
            cfg("shell", Ask, &["*"]),
            cfg("shell", Allow, &["cargo test*"]),
        ];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("cargo test"),
                &rules
            ),
            Allow
        );
    }

    #[test]
    fn gwt8_arg_hidden_danger_is_unwrapped() {
        let rules = [builtin_deny("shell", &["rm -rf /"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("bash -c 'rm -rf /'"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn gwt9_deny_wins_when_layers_conflict() {
        // Project layer adds a deny over a user allow on the same command.
        let rules = [
            cfg("shell", Allow, &["docker *"]),
            cfg("shell", Deny, &["docker *"]),
        ];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("docker run x"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn gwt10_empty_config_is_pure_mode_behaviour() {
        let rules: [PermissionRule; 0] = [];
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::Shell,
                "shell",
                &shell("anything"),
                &rules
            ),
            Ask
        );
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Write,
                "write_file",
                &path("x"),
                &rules
            ),
            Allow
        );
        assert_eq!(
            decide(
                PermissionMode::Default,
                SideEffect::ReadOnly,
                "read_file",
                &path("x"),
                &rules
            ),
            Allow
        );
    }

    // ---- additional safety / extraction / normalization ----

    #[test]
    fn plan_mode_cannot_be_escaped_by_an_allow_rule() {
        let rules = [cfg("shell", Allow, &["git *"])];
        assert_eq!(
            decide(
                PermissionMode::Plan,
                SideEffect::Shell,
                "shell",
                &shell("git status"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn chained_command_each_segment_is_checked() {
        let rules = [builtin_deny("shell", &["rm -rf /"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("echo hi && rm -rf /"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn env_wrapper_is_stripped_before_matching() {
        let rules = [builtin_deny("shell", &["rm -rf /"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("env FOO=1 rm -rf /"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn builtin_curl_pipe_sh_denied_via_raw_match() {
        // `|` splits segments, so the deny must match the raw command line.
        let rules = [builtin_deny("shell", &["*| sh"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("curl http://x | sh"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn builtin_secret_read_in_shell_denied() {
        let rules = [builtin_deny("shell", &["cat *.env"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("cat .env"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn secret_read_via_dotdot_path_is_denied() {
        let rules = [builtin_deny("read_file", &["**/.env"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::ReadOnly,
                "read_file",
                &path("src/../.env"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn shell_match_wildcards() {
        assert!(shell_match("git *", "git status"));
        assert!(shell_match("cargo test*", "cargo test --all"));
        assert!(shell_match("*", "anything at all / with slashes"));
        assert!(!shell_match("git *", "cargo build"));
        assert!(shell_match("rm -rf /", "rm -rf /"));
    }

    // ---- Windows-specific denylist patterns ----

    #[test]
    fn windows_del_recursive_is_denied() {
        let rules = [builtin_deny("shell", &["del /s *"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("del /s C:\\Users"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn windows_rd_recursive_is_denied() {
        let rules = [builtin_deny("shell", &["rd /s *"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("rd /s /q C:\\Temp"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn windows_format_is_denied() {
        let rules = [builtin_deny("shell", &["format ?:*"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("format C: /Q /Y"),
                &rules
            ),
            Deny
        );
    }

    #[test]
    fn cmd_c_unwrapping_catches_inner_danger() {
        // `cmd /C "del /s C:\"` — inner_script must recurse into the quoted command.
        let rules = [builtin_deny("shell", &["del /s *"])];
        assert_eq!(
            decide(
                PermissionMode::Bypass,
                SideEffect::Shell,
                "shell",
                &shell("cmd /C \"del /s C:\\\\\""),
                &rules
            ),
            Deny
        );
    }

    /// Property test for the permission broker's SECURITY invariants — the layered ordering in
    /// `decide` is the boundary that gates every dangerous tool (shell, `.env`/secret reads, untrusted
    /// MCP), so its guarantees must hold under ANY combination of mode, side effect, tool, args, and
    /// rule set. Deterministic seeded-LCG corpus (no dep). A matching rule is one with `tool="*"` and
    /// no patterns (matches any call); noise rules use a tool name that never matches. Asserts the
    /// three invariants a future refactor of the ordered logic must never break:
    ///   1. ANY matching Deny rule (builtin or configured) ⟹ Deny — a deny is never outvoted by an
    ///      allow, in any mode (steps 1–2 precede the most-specific-allow step).
    ///   2. A matching BUILTIN Deny ⟹ Deny even under `Bypass` — the unoverridable safety floor
    ///      (`.env`/secret denylist can't be bypassed or allow-ruled away).
    ///   3. No matching Deny + `Plan` mode + a non-ReadOnly side effect ⟹ Deny — plan is a hard
    ///      read-only contract no allow rule can escape (step 3).
    #[test]
    fn broker_security_invariants_hold_under_fuzz() {
        use PermissionMode::*;
        let modes = [Plan, Default, AcceptEdits, Bypass];
        let effects = [
            SideEffect::ReadOnly,
            SideEffect::Write,
            SideEffect::Shell,
            SideEffect::Network,
            SideEffect::External,
        ];
        let tools = ["shell", "edit", "read", "myserver__tool", "*"];
        let args = [
            shell("rm -rf /"),
            path("/etc/passwd"),
            json!({}),
            shell("git status"),
        ];

        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as usize
        };
        for _ in 0..5000 {
            let mode = modes[next() % modes.len()];
            let eff = effects[next() % effects.len()];
            let tool = tools[next() % tools.len()];
            let arg = &args[next() % args.len()];

            // Assemble a rule set: always some non-matching noise, plus each kind of MATCHING rule
            // (tool="*", empty patterns ⇒ matches any call) flipped on independently at random.
            let mut rules: Vec<PermissionRule> = vec![
                cfg("__never__", Allow, &[]),
                builtin_deny("__never__", &["x"]),
            ];
            let has_builtin_deny = next() % 3 == 0;
            let has_cfg_deny = next() % 3 == 0;
            let has_cfg_allow = next() % 2 == 0;
            let has_cfg_ask = next() % 2 == 0;
            if has_builtin_deny {
                rules.push(builtin_deny("*", &[]));
            }
            if has_cfg_deny {
                rules.push(cfg("*", Deny, &[]));
            }
            if has_cfg_allow {
                rules.push(cfg("*", Allow, &[]));
            }
            if has_cfg_ask {
                rules.push(cfg("*", Ask, &[]));
            }

            let got = decide(mode, eff, tool, arg, &rules);
            let any_matching_deny = has_builtin_deny || has_cfg_deny;

            // Invariant 1 & 2: any matching deny (incl. builtin under Bypass) ⟹ Deny.
            if any_matching_deny {
                assert_eq!(
                    got, Deny,
                    "deny must win: mode={mode:?} eff={eff:?} tool={tool} \
                     builtin_deny={has_builtin_deny} cfg_deny={has_cfg_deny} \
                     cfg_allow={has_cfg_allow}"
                );
            }
            // Invariant 3: no matching deny + Plan + side effect ⟹ Deny.
            if !any_matching_deny && mode == Plan && eff != SideEffect::ReadOnly {
                assert_eq!(
                    got, Deny,
                    "plan must deny side effects: eff={eff:?} tool={tool} \
                     cfg_allow={has_cfg_allow} cfg_ask={has_cfg_ask}"
                );
            }
            // Bonus: a builtin matching deny is NEVER Allow even with a matching configured allow.
            if has_builtin_deny {
                assert_ne!(got, Allow, "builtin deny floor was overridden to Allow");
            }
        }
    }
}
