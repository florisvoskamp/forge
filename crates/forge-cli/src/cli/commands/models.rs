use anyhow::{Context, Result};
use std::sync::Arc;

use forge_config::ClassifierKind;
use forge_core::LlmRouter;
use forge_mesh::{HeuristicRouter, Router};
use forge_provider::{DispatchProvider, MockProvider, Provider};
use forge_store::Store;
use forge_types::TaskTier;

use crate::*;

/// Construct the model backend + router from config. Shared by interactive sessions and the
/// `mcp-serve` subagent path (RFC subagent-orchestration Phase 3), so both route identically.
pub(crate) fn build_provider_and_router(
    config: &forge_config::Config,
    mock: bool,
    pin: Option<String>,
    catalog: Option<forge_mesh::ModelCatalog>,
) -> (Arc<dyn Provider>, Arc<dyn Router>) {
    let provider: Arc<dyn Provider> = if mock {
        Arc::new(MockProvider)
    } else {
        // Routes API models to genai and `claude-cli::`/`codex-cli::` to the subscription CLI
        // bridge. `harness` mode runs the bridge's tools through Forge's MCP server (RFC Phase 2).
        let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
        Arc::new(
            DispatchProvider::new(harness)
                .with_max_output_tokens(config.mesh.effective_max_output_tokens()),
        )
    };
    let mut heuristic = HeuristicRouter::new(config.clone()).with_pin(pin);
    if let Some(cat) = catalog {
        heuristic = heuristic.with_catalog(cat);
    }
    let router: Arc<dyn Router> = if matches!(
        config.mesh.classifier,
        ClassifierKind::Llm | ClassifierKind::Hybrid
    ) {
        // LLM / Hybrid classifier: a cheap model labels the tier; the heuristic router
        // does cost-aware selection; any failure falls back to the heuristic.
        // Hybrid additionally skips the LLM call when the heuristic is already confident
        // (score ≤−4 or ≥8), keeping zero added latency for obvious cases.
        let classifier_model = config
            .mesh
            .classifier_model
            .clone()
            .or_else(|| config.model_for(TaskTier::Trivial).map(String::from))
            .unwrap_or_default();
        let classify_provider: Arc<dyn Provider> = if mock {
            Arc::new(MockProvider)
        } else {
            // classification needs no tools/harness; cap output (one tier word) so a free
            // classifier model isn't 402'd on a huge default max-token request.
            Arc::new(
                DispatchProvider::new(false)
                    .with_max_output_tokens(config.mesh.effective_max_output_tokens()),
            )
        };
        let hybrid = config.mesh.classifier == ClassifierKind::Hybrid;
        Arc::new(LlmRouter::new(classify_provider, classifier_model, heuristic).with_hybrid(hybrid))
    } else {
        Arc::new(heuristic)
    };
    (provider, router)
}

/// Build a session around a caller-provided presenter, wiring all subsystems.
/// Discover the models the user can actually use, as a [`forge_mesh::ModelCatalog`] for
/// auto-discovery routing: query each provider that has a key (plus keyless local `ollama`) for
/// its model list, with a short per-provider timeout, and skip any that error. Providers are
/// probed concurrently so startup pays the slowest single provider's budget, not their sum.
/// Discover one provider's listable models, honoring its timeout `budget` and logging failures with
/// the right severity. Returns an empty Vec on any skip/failure/timeout so the caller can flatten
/// concurrently. A KEYED provider failing/timing out means the user configured a key but its models
/// silently vanish from routing (the mesh falls back to built-in defaults) — make that LOUD. Keyless
/// `ollama` failing just means it isn't running: debug.
async fn discover_provider_models(p: &str, budget: std::time::Duration) -> Vec<String> {
    let keyed = p != "ollama";
    // Some keyed providers are completion-only — they answer turns fine (via the custom
    // service-target resolver) but have no model-LISTING API, so auto-discovery can't enumerate
    // them. That's expected, not a key/network failure: skip them quietly with accurate guidance
    // (configure their models explicitly) instead of a scary "discovery failed — check your key".
    if keyed && !forge_provider::is_discoverable(p) {
        tracing::debug!(
            "'{p}' has no model-listing API — it's completion-only; pin a `{p}::<model>` id \
             (or add it under [mesh.models]) to route it. (Not a key/network problem.)"
        );
        return Vec::new();
    }
    match tokio::time::timeout(budget, forge_provider::list_models(p)).await {
        Ok(Ok(list)) => list,
        Ok(Err(e)) if keyed => {
            tracing::warn!(
                "model discovery FAILED for keyed provider '{p}': {e} — its models won't be routable this session (check the key / network)"
            );
            Vec::new()
        }
        Ok(Err(e)) => {
            tracing::debug!("model discovery skipped {p}: {e}");
            Vec::new()
        }
        Err(_) if keyed => {
            tracing::warn!(
                "model discovery TIMED OUT for keyed provider '{p}' after {}s — its models won't be routable this session",
                budget.as_secs()
            );
            Vec::new()
        }
        Err(_) => {
            tracing::debug!("model discovery timed out for {p}");
            Vec::new()
        }
    }
}

pub(crate) async fn discover_catalog(config: &forge_config::Config) -> forge_mesh::ModelCatalog {
    use std::time::Duration;
    let mut models = Vec::new();
    // Keyless local first, then every key-holding provider.
    let mut providers = vec!["ollama".to_string()];
    providers.extend(
        forge_config::known_key_providers()
            .filter(|p| forge_config::has_api_key(p))
            .map(str::to_string),
    );
    // Probe every provider CONCURRENTLY: each `list_models` is an independent network call to a
    // different endpoint, so a sequential loop made startup pay the SUM of every provider's budget
    // (3 keyed providers × 8s ≈ 24s worst case). `join_all` makes it the MAX instead (~8s), the same
    // pattern `drop_unaffordable_models` already uses. Results are flattened in provider order so the
    // catalog stays deterministic (dedup below relies on a stable first-seen order).
    let probes = providers.iter().map(|p| {
        discover_provider_models(p, Duration::from_secs(if p != "ollama" { 8 } else { 4 }))
    });
    for list in futures::future::join_all(probes).await {
        models.extend(list);
    }
    // Custom OpenAI-compatible providers (NVIDIA NIM, SambaNova, Mistral, Cerebras, …) have no genai
    // SDK adapter, so the genai probe above skips them — but they DO expose an OpenAI `/v1/models`
    // endpoint. List them LIVE (the full catalog the key can reach) so EVERY model is visible, not a
    // hand-seeded few; fall back to the curated seed ids only if the live call fails (offline /
    // endpoint down). Generic over the registry — future providers need no code here. Probed
    // concurrently with an 8s budget each, like the genai providers above.
    let custom: Vec<_> = forge_config::custom_providers()
        .filter(|cp| forge_config::has_api_key(cp.namespace))
        .collect();
    let custom_lists = futures::future::join_all(custom.iter().map(|cp| async move {
        let seeds = || {
            cp.seed_models
                .iter()
                .map(|m| format!("{}::{}", cp.namespace, m))
                .collect::<Vec<_>>()
        };
        match tokio::time::timeout(
            Duration::from_secs(8),
            forge_provider::list_custom_models(cp.namespace),
        )
        .await
        {
            Ok(Ok(list)) if !list.is_empty() => list,
            Ok(Err(e)) => {
                tracing::debug!(
                    "{} live model list failed: {e} — using seed ids",
                    cp.namespace
                );
                seeds()
            }
            _ => seeds(),
        }
    }))
    .await;
    for list in custom_lists {
        models.extend(list);
    }
    // Always-available subscription bridges (claude-cli/codex-cli) if their CLI is installed.
    // They don't rate-limit like the free API tiers, so the mesh can rely on them — and being
    // $0 subscriptions they rank first (prefer_subscription), so routing reaches a working model
    // instead of erroring out when metered providers are throttled. Each installed bridge
    // contributes its bare default id PLUS one id per model alias (config override, else the
    // bridge's built-in defaults) so the mesh can size each turn (haiku/mini ↔ opus) instead of
    // seeing a single model. A stale alias just benches itself via failover — never a hard error.
    for k in forge_provider::CliKind::all()
        .into_iter()
        .filter(|k| k.available())
    {
        let prefix = k.prefix();
        models.push(k.default_model_id());
        match config.mesh.bridge_models.get(prefix) {
            Some(custom) if !custom.is_empty() => {
                models.extend(custom.iter().map(|m| format!("{prefix}::{m}")));
            }
            _ => models.extend(k.default_models().iter().map(|m| format!("{prefix}::{m}"))),
        }
    }
    // Dedup while preserving discovery order (a provider could list the same id twice).
    let mut seen = std::collections::HashSet::new();
    models.retain(|m| seen.insert(m.clone()));
    // Drop any model/provider the user disabled (`[mesh] disabled`), so the mesh never routes to
    // or fails over onto it (known-issues.md: disable a flaky model without deleting its key).
    models.retain(|m| !forge_config::is_model_disabled(m, &config.mesh.disabled));
    // Pre-flight balance: for each provider that exposes a key-authenticated balance API, drop its
    // PAID models when the account is out of credit — so the mesh never tries (and 402s on) a model
    // it can't pay for (e.g. OpenRouter at $0 balance). Free variants + providers without a balance
    // API are untouched (fail open). Probes run concurrently across providers; each is short-timed.
    drop_unaffordable_models(&mut models).await;
    // Fetch + persist real per-model context windows (OpenRouter exposes `context_length`) so the
    // core can trim each turn to the routed model's window instead of overflowing it. Best-effort;
    // the family heuristic covers everything else.
    context_windows::fetch_and_persist(&models).await;
    // Attach measured benchmark scores (ADR-0011) so the mesh ranks on real performance. Cache-
    // first + incremental: only hits the API when a newly-discovered model has no rating yet.
    let bench = benchmarks::ensure(config, &models, false).await;
    forge_mesh::ModelCatalog::new(models).with_benchmarks(bench)
}

/// Remove a provider's metered models from `models` when its account balance is confirmed below
/// [`balance::MIN_CREDIT_USD`]. Only providers exposing a key-authenticated balance API are probed
/// (others return `None` → kept); genuinely-free variants (e.g. OpenRouter `:free`) are kept too.
pub(crate) async fn drop_unaffordable_models(models: &mut Vec<String>) {
    let mut providers: Vec<String> = models
        .iter()
        .map(|m| forge_config::provider_of(m).to_string())
        .filter(|p| !p.is_empty())
        .collect();
    providers.sort();
    providers.dedup();

    // Probe every provider concurrently; collect the ones confirmed broke.
    let checks = providers.into_iter().map(|p| async move {
        match balance::remaining_credit(&p).await {
            Some(bal) if bal < balance::MIN_CREDIT_USD => Some((p, bal)),
            _ => None,
        }
    });
    let broke: Vec<(String, f64)> = futures::future::join_all(checks)
        .await
        .into_iter()
        .flatten()
        .collect();

    for (p, bal) in broke {
        let before = models.len();
        models.retain(|m| forge_config::provider_of(m) != p || balance::is_free_model_id(m));
        let dropped = before - models.len();
        if dropped > 0 {
            tracing::info!(
                "{p} balance {bal:.2} < {:.2} — dropped {dropped} paid model(s) from discovery (free variants kept)",
                balance::MIN_CREDIT_USD
            );
        }
    }
}

/// `forge models [--probe]`: discover the usable models + show the mesh's capability-ranked pick
/// per tier. With `--probe`, also ping each model and persist health (the user-driven rescan).
pub(crate) async fn models(probe: bool, probe_all: bool, clear: bool) -> Result<()> {
    if clear {
        let store = open_store()?;
        let n = store
            .clear_all_model_health()
            .context("clearing model benches")?;
        println!("cleared {n} model bench(es) — the mesh will reconsider every model");
        return Ok(());
    }
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        println!(
            "no models discovered — set a provider key (`forge auth <provider>`) or run ollama"
        );
        return Ok(());
    }
    let store = open_store()?;

    if probe {
        // Default: only re-probe the benched/excluded models (cheap — that's the whole point of a
        // recheck). `--all` pings every discovered model (costs real money on paid providers).
        let targets: Vec<String> = if probe_all {
            cat.models().to_vec()
        } else {
            let benched = store.current_benched().unwrap_or_default();
            cat.models()
                .iter()
                .filter(|m| benched.is_benched(m))
                .cloned()
                .collect()
        };
        if targets.is_empty() {
            println!(
                "no benched models to recheck — all {} discovered models are healthy. \
                 Use `--probe --all` to force a full re-ping.",
                cat.models().len()
            );
        } else {
            if !probe_all {
                println!("rechecking {} benched model(s)…", targets.len());
            }
            probe_models(&targets, &config, &store).await?;
        }
        println!();
    }

    let pricing = forge_mesh::pricing::Pricing::from_config(&config);
    let benched = store.current_benched().unwrap_or_default();
    let s = cat.stats(&pricing);
    println!(
        "{} models · {} frontier · {} free · {} subscription · {} paid · {} providers\n",
        s.total, s.frontier, s.free, s.subscription, s.paid, s.providers
    );
    for g in cat.by_provider(&pricing) {
        println!("{} ({} models)", g.provider, g.total());
        for m in &g.models {
            let name = if m.name.is_empty() {
                "(default)"
            } else {
                m.name.as_str()
            };
            let mut tags: Vec<String> = Vec::new();
            if m.subscription {
                tags.push("subscription".into());
            }
            if m.frontier {
                tags.push("frontier".into());
            }
            if m.free {
                tags.push("free".into());
            }
            if m.cost > f64::EPSILON {
                tags.push(format!("paid ~${:.4}/turn", m.cost));
            } else if m.paid {
                tags.push("paid".into());
            }
            if benched.is_benched(&m.id) {
                tags.push("benched".into());
            }
            println!("  {name:<30} {}", tags.join(" · "));
        }
    }
    println!("\nmesh auto-pick per tier:");
    for tier in [TaskTier::Trivial, TaskTier::Standard, TaskTier::Complex] {
        // Mirror routing: skip benched models so the shown pick is the one the mesh would
        // actually use right now (model-health-failover).
        let pick = cat
            .ranked_for(tier, &pricing, 5)
            .into_iter()
            .find(|m| !benched.is_benched(m))
            .unwrap_or_else(|| "—".into());
        println!("  {:<9} {pick}", tier.as_str());
    }
    if !probe {
        println!(
            "\ntip: `forge models --probe` rechecks only the benched models (cheap); \
             add `--all` to re-ping every model (costs money on paid providers)."
        );
    }
    Ok(())
}

/// `forge benchmarks [--refresh]` — show measured model scores + catalog coverage (ADR-0011).
pub(crate) async fn benchmarks_cmd(refresh: bool) -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    if !config.mesh.benchmark_ranking {
        println!("benchmark ranking is disabled (`mesh.benchmark_ranking = false`).");
        return Ok(());
    }
    let cat = discover_catalog(&config).await;
    let models = cat.models().to_vec();
    let scores = benchmarks::ensure(&config, &models, refresh).await;
    let Some(scores) = scores.filter(|s| !s.is_empty()) else {
        println!(
            "no benchmark data yet. Set a free Artificial Analysis key to enable real-performance \
             ranking:\n  export ARTIFICIALANALYSIS_API_KEY=…   (or `forge auth artificialanalysis`)\n\
             then `forge benchmarks --refresh`. Until then the mesh ranks on the family heuristic."
        );
        return Ok(());
    };
    let (covered, total) = cat.benchmark_coverage();
    println!(
        "{} models scored · {covered}/{total} catalog models matched\n",
        scores.len()
    );
    let mut rows: Vec<(String, Option<forge_mesh::BenchScore>)> = cat
        .models()
        .iter()
        .filter(|m| forge_mesh::catalog::is_routable(m))
        .map(|m| (m.clone(), scores.score_for(m)))
        .collect();
    // Scored first (by intelligence desc), then the unmatched (heuristic fallback).
    rows.sort_by(|a, b| match (a.1, b.1) {
        (Some(x), Some(y)) => y.intelligence.total_cmp(&x.intelligence),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.0.cmp(&b.0),
    });
    for (id, score) in rows {
        match score {
            Some(s) => println!(
                "  {:<40} intelligence {:>5.1}  coding {:>5.1}",
                id, s.intelligence, s.coding
            ),
            None => println!("  {:<40} —  (heuristic fallback)", id),
        }
    }
    Ok(())
}

/// `forge mesh [PROMPT]` — explain how the mesh routes. With a prompt: the full decision trace.
/// Without one: the per-tier picks + subscription-quota overview. The non-interactive sibling of
/// the `/mesh` TUI inspector; both read the same [`forge_mesh::RoutingExplanation`] engine.
pub(crate) async fn mesh_explain(prompt: String, json: bool) -> Result<()> {
    forge_config::inject_provider_keys();
    let config = forge_config::load().unwrap_or_default();
    let cat = discover_catalog(&config).await;
    if cat.is_empty() {
        println!(
            "no models discovered — set a provider key (`forge auth <provider>`) or run ollama"
        );
        return Ok(());
    }
    let store = open_store()?;
    // Codex from its rollout files; claude's CURRENT 5h+weekly utilisation from a one-shot
    // `claude --debug` probe (gated: skip if the store was updated < 5 min ago).
    let bstats = tokio::task::spawn_blocking(bridge_stats::fetch)
        .await
        .unwrap_or_default();
    seed_store_quota(&store, "codex-cli", "five_hour", bstats.codex_5h_pct);
    seed_store_quota(&store, "codex-cli", "weekly", bstats.codex_weekly_pct);
    if store
        .subscription_age_secs("claude-cli")
        .is_none_or(|a| a > 300)
    {
        let limits = tokio::task::spawn_blocking(bridge_stats::probe_claude_limits)
            .await
            .unwrap_or_default();
        for (window, frac) in limits {
            seed_store_quota(&store, "claude-cli", &window, Some(frac * 100.0));
        }
    }
    let quota = store
        .current_quota()
        .unwrap_or_default()
        .with_plans(config.mesh.subscriptions.clone())
        .with_conserve(config.mesh.subscription_conserve);
    let health = store.current_benched().unwrap_or_default();
    let budget = forge_mesh::BudgetState {
        spent_today_usd: store.spend_today_usd().unwrap_or(0.0),
        daily_cap_usd: config.mesh.daily_budget_usd,
        spent_week_usd: store.spend_this_week_usd().unwrap_or(0.0),
        weekly_cap_usd: config.mesh.weekly_budget_usd,
        spent_month_usd: store.spend_this_month_usd().unwrap_or(0.0),
        monthly_cap_usd: config.mesh.monthly_cap_usd,
        warn_fraction: config.mesh.warn_threshold,
    };
    let router = HeuristicRouter::new(config.clone()).with_catalog(cat.clone());

    if prompt.trim().is_empty() {
        mesh_overview(&cat, &config, &quota);
        return Ok(());
    }
    let e = router.explain(&prompt, budget, &health, &quota);
    if json {
        println!("{}", mesh_explanation_json(&e));
    } else {
        print_mesh_explanation(&e);
    }
    Ok(())
}

/// Record a subscription window fraction (0–100 pct) into the store, mapping it to a status. Used
/// to seed the mesh quota from the Claude/Codex rate-limit caches in the `forge mesh` CLI path.
pub(crate) fn seed_store_quota(store: &Store, provider: &str, window: &str, pct: Option<f64>) {
    let Some(pct) = pct else { return };
    let frac = (pct / 100.0).clamp(0.0, 1.0);
    let status = if frac >= 0.98 {
        forge_types::QuotaStatus::Exhausted
    } else if frac >= 0.80 {
        forge_types::QuotaStatus::Warning
    } else {
        forge_types::QuotaStatus::Ok
    };
    let _ = store.record_quota(&forge_types::QuotaHint {
        provider: provider.to_string(),
        window: window.to_string(),
        status,
        resets_at: None,
        fraction_used: Some(frac),
    });
}

/// A 10-cell ASCII meter for a 0.0–1.0 fraction.
pub(crate) fn meter(frac: f64) -> String {
    let filled = (frac.clamp(0.0, 1.0) * 10.0).round() as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(10 - filled))
}

/// The no-prompt overview: subscription quota gauges + per-tier ranked picks.
pub(crate) fn mesh_overview(
    cat: &forge_mesh::ModelCatalog,
    config: &forge_config::Config,
    quota: &forge_types::SubscriptionQuota,
) {
    let pricing = forge_mesh::pricing::Pricing::from_config(config);
    println!(
        "subscription quota (conservation {}):",
        if config.mesh.subscription_conserve {
            "on"
        } else {
            "off"
        }
    );
    let mut subs: Vec<&str> = cat
        .models()
        .iter()
        .filter(|m| forge_mesh::catalog::is_subscription(m))
        .map(|m| forge_mesh::catalog::provider_of(m))
        .collect();
    subs.sort_unstable();
    subs.dedup();
    if subs.is_empty() {
        println!("  (no subscription bridges installed)");
    }
    for p in &subs {
        let frac = quota.fraction_for(p);
        let plan = quota.plan_for(p);
        let plan = if plan.is_empty() { "?" } else { plan };
        let pc = forge_mesh::ModelCatalog::spread_probability(TaskTier::Complex, frac, plan, false);
        let ps =
            forge_mesh::ModelCatalog::spread_probability(TaskTier::Standard, frac, plan, false);
        println!(
            "  {:<11} {} {:>3.0}% · plan {plan} · {:?} · spread P(complex)={:.0}% P(standard)={:.0}%",
            p,
            meter(frac),
            frac * 100.0,
            quota.status_for(p),
            pc * 100.0,
            ps * 100.0,
        );
    }
    println!("\nper-tier ranking (top 5):");
    for tier in [TaskTier::Trivial, TaskTier::Standard, TaskTier::Complex] {
        let (_, rows) = cat.ranked_rows(tier, &pricing, false, 0, quota);
        println!("  {}:", tier.as_str());
        for r in rows.iter().take(5) {
            println!(
                "    {:<34} score {:>6.2}  {}",
                r.model,
                r.final_score,
                cost_tag(r.cost_class)
            );
        }
    }
    println!("\ntip: `forge mesh \"<your task>\"` explains exactly how one prompt routes.");
}

pub(crate) fn cost_tag(class: u8) -> &'static str {
    match class {
        0 => "free",
        1 => "subscription",
        _ => "paid",
    }
}

/// The formatted single-prompt explanation.
pub(crate) fn print_mesh_explanation(e: &forge_mesh::RoutingExplanation) {
    println!("prompt: {:?}", e.prompt);
    print!("classified: {}", e.classified_tier.as_str());
    if e.routed_tier != e.classified_tier {
        print!(" → routed {}", e.routed_tier.as_str());
    }
    println!(
        "  ·  code-heavy: {}  ·  reasons: {}",
        if e.code_heavy { "yes" } else { "no" },
        e.classify_reasons.join(", ")
    );

    if !e.quota.is_empty() {
        println!("\nquota:");
        for q in &e.quota {
            let plan = if q.plan.is_empty() { "?" } else { &q.plan };
            println!(
                "  {:<11} {} {:>3.0}% · plan {plan} · {:?} · spread P={:.0}%",
                q.provider,
                meter(q.fraction),
                q.fraction * 100.0,
                q.status,
                q.spread_probability * 100.0,
            );
        }
    }

    let c = &e.conserve;
    if c.enabled {
        let verdict = if !c.eligible {
            "no frontier alternative → not applied".to_string()
        } else if c.fired {
            format!(
                "FIRED (roll {:.2} < P {:.2}) → spread off subscriptions",
                c.roll, c.probability
            )
        } else {
            format!(
                "not fired (roll {:.2} ≥ P {:.2}) → subscription kept",
                c.roll, c.probability
            )
        };
        println!("\nconservation: {verdict}");
    } else {
        println!("\nconservation: off");
    }

    if !e.candidates.is_empty() {
        println!("\ncandidates (top {}):", e.candidates.len().min(8));
        for c in e.candidates.iter().take(8) {
            let marker = if c.selected { "*" } else { " " };
            let pen = if c.row.conserve_penalty > 0.0 {
                format!(" −{:.0}", c.row.conserve_penalty)
            } else {
                String::new()
            };
            println!(
                "  {marker} #{:<2} {:<34} score {:>6.2}  cap {:>5.2}  {}{}{}{}",
                c.rank,
                c.row.model,
                c.row.final_score,
                c.row.capability,
                cost_tag(c.row.cost_class),
                pen,
                if c.row.frontier { " · frontier" } else { "" },
                if c.usable { "" } else { " · UNUSABLE" },
            );
        }
    }

    println!("\npick: {}", e.pick);
    if !e.fallbacks.is_empty() {
        println!("fallbacks: {}", e.fallbacks.join(", "));
    }
    println!("why: {}", e.rationale);
}

/// JSON form of the explanation (stable shape for scripting / tests).
pub(crate) fn mesh_explanation_json(e: &forge_mesh::RoutingExplanation) -> String {
    let candidates: Vec<_> = e
        .candidates
        .iter()
        .map(|c| {
            serde_json::json!({
                "rank": c.rank,
                "model": c.row.model,
                "provider": c.row.provider,
                "final_score": c.row.final_score,
                "capability": c.row.capability,
                "cost_class": c.row.cost_class,
                "conserve_penalty": c.row.conserve_penalty,
                "subscription": c.row.subscription,
                "frontier": c.row.frontier,
                "usable": c.usable,
                "selected": c.selected,
            })
        })
        .collect();
    let quota: Vec<_> = e
        .quota
        .iter()
        .map(|q| {
            serde_json::json!({
                "provider": q.provider,
                "status": format!("{:?}", q.status),
                "fraction": q.fraction,
                "plan": q.plan,
                "spread_probability": q.spread_probability,
            })
        })
        .collect();
    serde_json::to_string_pretty(&serde_json::json!({
        "prompt": e.prompt,
        "classified_tier": e.classified_tier.as_str(),
        "routed_tier": e.routed_tier.as_str(),
        "classify_reasons": e.classify_reasons,
        "code_heavy": e.code_heavy,
        "seed": e.seed,
        "conserve": {
            "enabled": e.conserve.enabled,
            "eligible": e.conserve.eligible,
            "probability": e.conserve.probability,
            "roll": e.conserve.roll,
            "fired": e.conserve.fired,
        },
        "quota": quota,
        "candidates": candidates,
        "pick": e.pick,
        "fallbacks": e.fallbacks,
        "rationale": e.rationale,
    }))
    .unwrap_or_else(|_| "{}".into())
}

/// Ping every discovered model with a 1-token request; clear the healthy ones and bench the
/// ones that rate-limit / fail auth / are down, so the mesh routes around them.
pub(crate) async fn probe_models(
    targets: &[String],
    config: &forge_config::Config,
    store: &Store,
) -> Result<()> {
    use std::time::Duration;
    let harness = config.mesh.bridge_mode == forge_config::BridgeMode::Harness;
    let provider = DispatchProvider::new(harness)
        .with_max_output_tokens(config.mesh.effective_max_output_tokens());
    let default_cooldown = Duration::from_secs(config.mesh.failover_cooldown_secs);
    let ping = [forge_types::Message::user("ping")];
    // Probe WITH a representative tool: the real agent loop always advertises tools, so a model
    // that can't do function calling (groq compound-mini, many OpenRouter models) must fail the
    // probe too — a no-tool ping would falsely pass it. This is what *confirms* a model (incl. any
    // marked "free") can actually serve a turn, not just answer a bare prompt.
    let probe_tool = [forge_provider::ToolSpec {
        name: "noop".to_string(),
        description: "A no-op used to verify the model accepts tool calls.".to_string(),
        schema: serde_json::json!({"type": "object", "properties": {}}),
    }];
    let mut sink = |_: forge_provider::StreamEvent| {};

    println!("probing {} model(s)…", targets.len());
    for m in targets {
        let res = tokio::time::timeout(
            Duration::from_secs(20),
            provider.complete(m, &ping, &probe_tool, &mut sink),
        )
        .await;
        match res {
            Ok(Ok(_)) => {
                store.clear_model_health(m).ok();
                println!("  ✓ {m}");
            }
            // A PERMANENT incapability (no tool support / unaffordable) → exclude for a long window
            // so discovery stops resurrecting it every run.
            Ok(Err(e)) if e.is_permanent() => {
                store.exclude_model(m, e.reason()).ok();
                println!("  ⊘ {m} — {} (excluded)", e.reason());
            }
            Ok(Err(e)) if e.is_retryable() => {
                let cooldown = e.cooldown(default_cooldown);
                store.bench_for(m, cooldown, e.reason()).ok();
                println!("  ✗ {m} — {} (benched {}s)", e.reason(), cooldown.as_secs());
            }
            Ok(Err(e)) => {
                // Non-retryable (e.g. the ping payload upset the model) → don't bench it.
                println!("  ? {m} — {} (not benched)", e.reason());
            }
            Err(_) => {
                store.bench_for(m, default_cooldown, "probe timeout").ok();
                println!(
                    "  ✗ {m} — timeout (benched {}s)",
                    default_cooldown.as_secs()
                );
            }
        }
    }
    Ok(())
}
