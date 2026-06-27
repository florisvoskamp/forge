use anyhow::{Context, Result};
use std::path::Path;
use std::sync::Arc;

use crate::*;

/// `forge assay list` / `forge assay compare <a> <b>` / `forge assay run` — assay commands.
pub(crate) async fn assay_cmd(sub: AssayCmd) -> Result<()> {
    if let AssayCmd::Run {
        scope,
        branch,
        since,
        path,
        format,
        fail_on,
        lenses,
        model,
        max_cost,
        yes,
    } = sub
    {
        return assay_run_cmd(
            scope, branch, since, path, format, fail_on, lenses, model, max_cost, yes,
        )
        .await;
    }
    let store = open_store()?;
    match sub {
        AssayCmd::Run { .. } => return Ok(()), // already handled above
        AssayCmd::List => {
            let runs = store.list_assay_runs().context("loading assay runs")?;
            if runs.is_empty() {
                println!("no assay runs found — run `/assay` inside `forge chat`");
                return Ok(());
            }
            println!("{:<10}  {:<28}  {:>8}  scope", "id", "date", "cost");
            println!("{}", "─".repeat(64));
            for (id, scope, cost, ts) in &runs {
                use chrono::{Local, TimeZone};
                let date = Local
                    .timestamp_opt(*ts, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_else(|| ts.to_string());
                println!(
                    "{:<10}  {:<28}  ${:>7.4}  {}",
                    &id[..id.len().min(8)],
                    date,
                    cost,
                    scope
                );
            }
        }
        AssayCmd::Compare { a, b } => {
            let resolve = |prefix: &str| -> Result<String> {
                let runs = store.list_assay_runs().context("loading assay runs")?;
                let matches: Vec<_> = runs
                    .into_iter()
                    .filter(|(id, ..)| id.starts_with(prefix))
                    .collect();
                match matches.len() {
                    0 => anyhow::bail!("no assay run matches '{prefix}' — see `forge assay list`"),
                    1 => Ok(matches.into_iter().next().unwrap().0),
                    n => anyhow::bail!("'{prefix}' is ambiguous ({n} runs) — use more characters"),
                }
            };
            let id_a = resolve(&a)?;
            let id_b = resolve(&b)?;
            let fa = store.load_findings(&id_a).context("loading run a")?;
            let fb = store.load_findings(&id_b).context("loading run b")?;
            let key = |f: &forge_types::Finding| format!("{}|{}", f.file, f.title);
            let keys_a: std::collections::HashSet<String> = fa.iter().map(key).collect();
            let keys_b: std::collections::HashSet<String> = fb.iter().map(key).collect();
            let fixed: Vec<_> = keys_a.difference(&keys_b).collect();
            let new_: Vec<_> = keys_b.difference(&keys_a).collect();
            let open: usize = keys_a.intersection(&keys_b).count();
            println!(
                "assay compare  {}  →  {}\n",
                &id_a[..id_a.len().min(8)],
                &id_b[..id_b.len().min(8)]
            );
            println!("  fixed      {:>4}", fixed.len());
            println!("  new        {:>4}", new_.len());
            println!("  still-open {:>4}", open);
            if !fixed.is_empty() {
                println!("\nfixed:");
                for k in fixed {
                    println!("  ✓ {k}");
                }
            }
            if !new_.is_empty() {
                println!("\nnew:");
                for k in new_ {
                    println!("  + {k}");
                }
            }
        }
    }
    Ok(())
}

/// Headless `forge assay run` — CI path. Prepares inputs exactly like the TUI's `spawn_assay`
/// (same model-tier discovery, same `bundle_scoped_source`), calls `run_assay`, renders output,
/// and exits 2 when `--fail-on` is set and a finding meets the threshold. Exit 1 on hard error
/// (propagated as `anyhow::Error`); exit 0 otherwise. Exit 3 when `--max-cost` is set and the
/// pre-estimate exceeds the cap (unless `--yes` is also passed).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn assay_run_cmd(
    scope_str: String,
    branch: Option<String>,
    since: Option<String>,
    path_override: Option<String>,
    format: AssayFormat,
    fail_on: Option<FailOnSeverity>,
    lenses_str: Option<String>,
    model_override: Option<String>,
    max_cost: Option<f64>,
    yes: bool,
) -> Result<()> {
    // Inject provider keys from env (ANTHROPIC_API_KEY / OPENROUTER_API_KEY etc.) so CI works
    // without a keyring — same call as `forge models` and `forge mesh` make.
    forge_config::inject_provider_keys();
    forge_config::inject_search_keys();

    // --- Resolve AssayScope from CLI flags ---
    let scope = match scope_str.trim().to_lowercase().as_str() {
        "repo" => forge_types::AssayScope::Repo,
        "diff" => forge_types::AssayScope::Diff,
        "branch" => {
            let base =
                branch.ok_or_else(|| anyhow::anyhow!("--scope branch requires --branch <ref>"))?;
            forge_types::AssayScope::Branch(base)
        }
        "since" => {
            let r = since.ok_or_else(|| anyhow::anyhow!("--scope since requires --since <ref>"))?;
            forge_types::AssayScope::Since(r)
        }
        "path" => {
            let p = path_override
                .ok_or_else(|| anyhow::anyhow!("--scope path requires --path <path>"))?;
            forge_types::AssayScope::Path(p)
        }
        other => anyhow::bail!("unknown scope '{other}' — valid: diff, repo, branch, since, path"),
    };

    // --- Bundle source for the scope ---
    let source = match bundle_scoped_source(&scope, 200_000) {
        Ok(s) => s,
        Err(e) => anyhow::bail!("assay: {e}"),
    };
    if source.trim().is_empty() {
        anyhow::bail!("assay: no analysable source files for the requested scope");
    }

    // --- Parse lenses ---
    let lenses: Vec<forge_types::FindingCategory> = match lenses_str {
        None => forge_types::FindingCategory::crew().to_vec(),
        Some(s) => {
            let mut out = Vec::new();
            for part in s.split(',') {
                let name = part.trim();
                match forge_types::FindingCategory::parse(name) {
                    Some(cat) => out.push(cat),
                    None => anyhow::bail!(
                        "unknown lens '{name}' — valid: dead-weight, correctness, unsafe, \
                         test-coverage, design, architecture, documentation, over-engineering"
                    ),
                }
            }
            if out.is_empty() {
                anyhow::bail!("--lenses was empty; provide at least one lens name");
            }
            out
        }
    };

    // --- Discover models (same path as TUI's spawn_assay) ---
    let config = forge_config::load().unwrap_or_default();
    let pricing = std::sync::Arc::new(forge_mesh::pricing::Pricing::from_config(&config));
    let store = std::sync::Arc::new(open_store()?);
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        anyhow::bail!(
            "assay: no models available — set a provider key (`forge auth <provider>`) or run ollama"
        );
    }
    let benched = store.current_benched().unwrap_or_default();
    let chain = |tier| -> Vec<String> {
        if let Some(ref m) = model_override {
            return vec![m.clone()];
        }
        let mut models: Vec<String> = cat
            .ranked_for(tier, &pricing, 8)
            .into_iter()
            .filter(|m| !benched.is_benched(m))
            .collect();
        if models.is_empty() {
            if let Some(m) = config.model_for(tier) {
                models.push(m.to_string());
            }
        }
        models
    };
    let (trivial, complex) = (
        chain(forge_types::TaskTier::Trivial),
        chain(forge_types::TaskTier::Complex),
    );
    if trivial.is_empty() && complex.is_empty() {
        anyhow::bail!("assay: every model is rate-limited/benched — try `forge models --probe`");
    }
    let models = forge_core::assay::TierModels { trivial, complex };

    // --- Cost pre-estimate: always print; abort on --max-cost unless --yes ---
    let estimate = forge_core::assay::estimate_assay_cost(&source, &lenses, &models, &pricing);
    eprintln!(
        "assay: estimated ~{} input tokens, ~${:.4}",
        estimate.est_input_tokens, estimate.est_usd
    );
    if !yes {
        if let Some(cap) = max_cost {
            if estimate.est_usd > cap {
                eprintln!(
                    "assay: estimated cost ${:.4} exceeds --max-cost ${:.4} — aborting \
                     (pass --yes to run anyway)",
                    estimate.est_usd, cap
                );
                use std::io::Write;
                std::io::stderr().flush().ok();
                std::process::exit(3);
            }
        }
    }

    // --- Build provider (same as build_provider_and_router, no mock in CI) ---
    let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
    let provider: std::sync::Arc<dyn forge_provider::Provider> = std::sync::Arc::new(
        forge_provider::DispatchProvider::new(harness)
            .with_max_output_tokens(config.mesh.effective_max_output_tokens()),
    );

    let cooldown = std::time::Duration::from_secs(config.mesh.failover_cooldown_secs);

    // --- Run the crew ---
    let src: std::sync::Arc<str> = std::sync::Arc::from(source.as_str());
    let report = forge_core::assay::run_assay(
        scope,
        src,
        lenses,
        models,
        provider,
        pricing,
        store,
        cooldown,
        &mut |_| {}, // progress events suppressed in headless mode
    )
    .await;

    // --- Render output ---
    match format {
        AssayFormat::Human => assay_output::print_human(&report),
        AssayFormat::Markdown => print!("{}", assay_output::print_markdown(&report)),
        AssayFormat::Json => println!("{}", assay_output::print_json(&report)),
        AssayFormat::Sarif => println!("{}", assay_output::print_sarif(&report)),
    }

    // --- Exit-code gate ---
    if let Some(threshold) = fail_on {
        let triggered = report
            .findings
            .iter()
            .any(|f| threshold.matches(f.severity));
        if triggered {
            use std::io::Write;
            std::io::stdout().flush().ok();
            std::process::exit(2);
        }
    }

    Ok(())
}

/// Resolve health-aware tier models from the discovery catalog, then spawn the Assay task (like
/// `spawn_turn`): the crew runs in the background while the spinner ticks, emits its report to the
/// TUI, and — when `cleanup` — runs a permission-gated, undoable Refine fix turn. Returns the task
/// handle (so Esc can interrupt it), or `None` if it couldn't start (no source / no live models).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn spawn_assay(
    cleanup: bool,
    lenses: Vec<forge_types::FindingCategory>,
    scope: forge_types::AssayScope,
    session: &Arc<tokio::sync::Mutex<Session>>,
    done_tx: &std::sync::mpsc::Sender<u64>,
    gen: u64,
    app: &mut forge_tui::App,
    busy: &mut bool,
    busy_since: &mut std::time::Instant,
) -> Result<Option<tokio::task::JoinHandle<()>>> {
    let source = match bundle_scoped_source(&scope, 200_000) {
        Ok(s) => s,
        Err(e) => {
            app.note(&format!("assay: {e}"));
            return Ok(None);
        }
    };
    if source.trim().is_empty() {
        app.note("assay: no analyzable source files under the working directory");
        return Ok(None);
    }
    let config = forge_config::load().unwrap_or_default();
    let pricing = forge_mesh::pricing::Pricing::from_config(&config);
    let store = open_store()?;
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        app.note("assay: no models available — `forge auth <provider>` or run ollama");
        return Ok(None);
    }
    // Route critics around rate-limited / benched models, like the agent loop does.
    let benched = store.current_benched().unwrap_or_default();
    // Build a CHAIN per tier (ranked, health-filtered): the crew tries them in order and fails
    // over when one rate-limits, instead of giving up on a single dead model.
    let chain = |tier| {
        let mut models: Vec<String> = cat
            .ranked_for(tier, &pricing, 8)
            .into_iter()
            .filter(|m| !benched.is_benched(m))
            .collect();
        if models.is_empty() {
            if let Some(m) = config.model_for(tier) {
                models.push(m.to_string());
            }
        }
        models
    };
    let (trivial, complex) = (chain(TaskTier::Trivial), chain(TaskTier::Complex));
    if trivial.is_empty() && complex.is_empty() {
        app.note(
            "assay: every model is rate-limited/benched — try /mode or `forge models --probe`",
        );
        return Ok(None);
    }
    let models = forge_core::assay::TierModels { trivial, complex };

    app.submit_user(if cleanup {
        "/assay → full cleanup (Refine)"
    } else {
        "/assay → analysis"
    });
    app.done = false;
    app.tick = 0;
    *busy = true;
    *busy_since = std::time::Instant::now();
    let s = session.clone();
    let dt = done_tx.clone();
    let src: Arc<str> = Arc::from(source.as_str());
    Ok(Some(tokio::spawn(async move {
        let _done = DoneGuard(dt, gen);
        let mut sess = s.lock().await;
        if let Err(e) = sess.assay(src, models, lenses, scope, cleanup).await {
            sess.notify_error(&format!("assay failed: {e}"));
        }
    })))
}

/// Concatenate the analyzable source under `root` (capped) with `// FILE:` headers, for the crew
/// prompt. Skips VCS/build/vendor dirs; deterministic order. A single file is bundled directly.
pub(crate) fn bundle_source(root: &Path, max_bytes: usize) -> String {
    fn is_skip_dir(name: &str) -> bool {
        matches!(
            name,
            ".git" | "target" | ".forge" | "node_modules" | "graphify-out" | ".idea" | ".vscode"
        )
    }
    fn is_source(ext: &str) -> bool {
        matches!(
            ext,
            "rs" | "toml"
                | "md"
                | "py"
                | "js"
                | "ts"
                | "tsx"
                | "go"
                | "java"
                | "c"
                | "cpp"
                | "h"
                | "hpp"
                | "sh"
                | "yaml"
                | "yml"
                | "json"
                | "sql"
        )
    }
    fn append(out: &mut String, path: &Path) {
        if let Ok(content) = std::fs::read_to_string(path) {
            out.push_str(&format!("// FILE: {}\n{content}\n\n", path.display()));
        }
    }

    let mut out = String::new();
    if root.is_file() {
        append(&mut out, root);
        out.truncate(floor_char_boundary(&out, max_bytes));
        return out;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if out.len() >= max_bytes {
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = rd.filter_map(|e| e.ok().map(|e| e.path())).collect();
        paths.sort();
        for p in paths {
            if out.len() >= max_bytes {
                break;
            }
            if p.is_dir() {
                if !p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(is_skip_dir)
                    .unwrap_or(false)
                {
                    stack.push(p);
                }
            } else if p
                .extension()
                .and_then(|e| e.to_str())
                .map(is_source)
                .unwrap_or(false)
            {
                append(&mut out, &p);
            }
        }
    }
    out.truncate(floor_char_boundary(&out, max_bytes));
    out
}

/// Bundle source for the given scope. For git-backed scopes (Diff/Branch/Since) the changed-file
/// list is resolved via `git diff --name-only`; only those files are bundled. Returns an error
/// string when a git scope is requested outside a repo or the git command fails.
pub(crate) fn bundle_scoped_source(
    scope: &forge_types::AssayScope,
    max_bytes: usize,
) -> Result<String, String> {
    use forge_types::AssayScope::*;
    let git_files = |args: &[&str]| -> Result<Vec<std::path::PathBuf>, String> {
        let out = std::process::Command::new("git")
            .args(args)
            .output()
            .map_err(|e| format!("git: {e}"))?;
        if !out.status.success() {
            let msg = String::from_utf8_lossy(&out.stderr).trim().to_string();
            return Err(format!("git {}: {msg}", args.join(" ")));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| !l.is_empty())
            .map(std::path::PathBuf::from)
            .collect())
    };
    match scope {
        Repo => Ok(bundle_source(std::path::Path::new("."), max_bytes)),
        Path(p) => Ok(bundle_source(std::path::Path::new(p), max_bytes)),
        Diff => {
            // `diff HEAD` (not bare `diff`) so STAGED changes are included — a plain `git diff`
            // compares the working tree to the index and silently drops anything already `git add`ed,
            // so a fully-staged change looked like "no uncommitted changes".
            let files = git_files(&["diff", "HEAD", "--name-only"])?;
            if files.is_empty() {
                return Err(
                    "no uncommitted changes (git diff HEAD --name-only returned nothing)".into(),
                );
            }
            Ok(bundle_file_list(&files, max_bytes))
        }
        Branch(base) => {
            let files = git_files(&["diff", "--name-only", &format!("{base}...HEAD")])?;
            if files.is_empty() {
                return Err(format!(
                    "no changes vs {base} (git diff --name-only {base}...HEAD returned nothing)"
                ));
            }
            Ok(bundle_file_list(&files, max_bytes))
        }
        Since(ref_) => {
            let files = git_files(&["diff", "--name-only", ref_])?;
            if files.is_empty() {
                return Err(format!(
                    "no changes since {ref_} (git diff --name-only {ref_} returned nothing)"
                ));
            }
            Ok(bundle_file_list(&files, max_bytes))
        }
    }
}

/// Bundle a specific list of file paths (e.g. from a git diff) with `// FILE:` headers.
pub(crate) fn bundle_file_list(files: &[std::path::PathBuf], max_bytes: usize) -> String {
    let mut out = String::new();
    for p in files {
        if out.len() >= max_bytes {
            break;
        }
        if let Ok(content) = std::fs::read_to_string(p) {
            out.push_str(&format!("// FILE: {}\n{content}\n\n", p.display()));
            if out.len() > max_bytes {
                out.truncate(floor_char_boundary(&out, max_bytes));
                break;
            }
        }
    }
    out
}

/// Largest index ≤ `max` that is a char boundary (so truncation never splits a UTF-8 char).
pub(crate) fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        return s.len();
    }
    let mut i = max;
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
