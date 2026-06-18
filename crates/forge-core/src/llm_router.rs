//! Optional cheap-LLM task classifier (ADR-0006 option 2, opt-in). Asks a small model to
//! label the tier before routing, then reuses the heuristic router's pin/budget/cost-aware
//! selection. Any failure — error, timeout, or an unparseable reply — silently falls back to
//! the deterministic heuristic, so enabling it can never break a turn. Off by default (A-2:
//! no per-task model call unless the user opts in via `mesh.classifier = "llm"`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_mesh::{BudgetState, HeuristicRouter, RouteHints, Router, RoutingDecision};
use forge_provider::Provider;
use forge_types::{Message, ModelHealth, SubscriptionQuota, TaskTier};

/// Hard ceiling on the classification call so a slow/hung model degrades to the heuristic.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(15);

/// Prompt that drives the LLM classification call.
///
/// Key design choices:
/// - Three tiers with concrete examples, not vague descriptions.
/// - Explicit "LENGTH IS NOT THE SIGNAL" rule — the single most important insight: a 6-word
///   prompt can be deeply complex; a 200-word prompt can be mechanical.
/// - Phrased as "what does this REQUIRE" not "how does it read".
/// - One word reply format, tolerant parser handles any stray prose.
const CLASSIFY_SYSTEM: &str = "You classify a software-engineering task by what it REQUIRES, \
not how many words describe it. Reply with EXACTLY ONE lowercase word: trivial, standard, \
or complex. No explanation, no punctuation, just the word.

trivial — mechanical edit, zero reasoning needed: fix a typo, rename a symbol, reformat \
or reorder code, bump a version number, delete or add a single line or comment, change a \
string literal, add whitespace.

standard — routine engineering with a clear scope: implement a self-contained function or \
endpoint, write or update tests, fix a clearly-described bug, add a small feature, \
convert/port code between similar languages, straightforward refactoring of one module.

complex — requires deep analysis, broad context, or subtle reasoning: architecture or \
system design decisions, debugging an intermittent or non-obvious bug, security audits, \
performance profiling and optimisation, algorithm design or correctness proofs, \
understanding how a non-trivial system works, reviewing an entire module or codebase area, \
evaluating trade-offs between approaches, multi-module refactoring.

CRITICAL: prompt length is irrelevant. Examples — \
'Fix the race condition in the scheduler' is COMPLEX (subtle concurrency, needs deep analysis). \
'Investigate why the cache warms slowly' is COMPLEX (open-ended investigation). \
'Audit the permission checks' is COMPLEX (security analysis). \
'Add a newline to the README' is TRIVIAL despite being in a long message. \
'Rename foo to bar in utils.rs' is TRIVIAL. \
'Implement a rate-limiter with token-bucket' is STANDARD (clear, self-contained). \
Classify by what thinking the task demands, not its surface length.";

/// A [`Router`] that labels the tier with a cheap model call, falling back to `fallback`.
///
/// Two modes:
/// - `hybrid = false` (Llm): always calls the LLM, every turn.
/// - `hybrid = true` (Hybrid): checks heuristic confidence first; only calls the LLM when
///   the heuristic score is near a tier boundary (the uncertain middle zone). Clear Trivial
///   or strongly-signalled Complex tasks skip the LLM entirely — zero added latency for them.
pub struct LlmRouter {
    provider: Arc<dyn Provider>,
    model: String,
    fallback: HeuristicRouter,
    hybrid: bool,
}

impl LlmRouter {
    pub fn new(provider: Arc<dyn Provider>, model: String, fallback: HeuristicRouter) -> Self {
        Self {
            provider,
            model,
            fallback,
            hybrid: false,
        }
    }

    /// Enable hybrid mode: skip the LLM when the heuristic is already confident.
    pub fn with_hybrid(mut self, hybrid: bool) -> Self {
        self.hybrid = hybrid;
        self
    }
}

/// Find the first tier word anywhere in the reply (tolerant of "Standard.", "I think complex",
/// leading whitespace, etc.). `None` if no tier word appears.
fn parse_tier(text: &str) -> Option<TaskTier> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphabetic())
        .find_map(|w| match w {
            "trivial" => Some(TaskTier::Trivial),
            "standard" => Some(TaskTier::Standard),
            "complex" => Some(TaskTier::Complex),
            _ => None,
        })
}

#[async_trait]
impl Router for LlmRouter {
    async fn route(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
    ) -> RoutingDecision {
        let hints = RouteHints::from_prompt(prompt);

        // Hybrid fast-path: if the heuristic is already confident, skip the LLM call. This
        // keeps zero added latency for obvious Trivial tasks (typo, rename) and strongly-
        // signalled Complex ones (multiple reasoning terms). Only the uncertain middle — score
        // −3…7 — triggers the extra round-trip.
        if self.hybrid {
            let (tier, confident, reason) = HeuristicRouter::classify_confident(prompt);
            if confident {
                return self.fallback.decide(
                    tier,
                    format!("{reason} (hybrid: heuristic confident)"),
                    budget,
                    health,
                    hints,
                    quota,
                );
            }
        }

        let messages = [Message::system(CLASSIFY_SYSTEM), Message::user(prompt)];
        let mut sink = |_: forge_provider::StreamEvent| {}; // classifier output isn't shown

        let tier = match tokio::time::timeout(
            CLASSIFY_TIMEOUT,
            self.provider
                .complete(&self.model, &messages, &[], &mut sink),
        )
        .await
        {
            Ok(Ok(resp)) => parse_tier(&resp.content),
            _ => None, // request error, or timed out
        };

        match tier {
            Some(t) => self.fallback.decide(
                t,
                format!("classified by {} as {}", self.model, t.as_str()),
                budget,
                health,
                hints,
                quota,
            ),
            None => {
                // Couldn't classify → deterministic heuristic, noted in the rationale.
                let mut d = self.fallback.route(prompt, budget, health, quota).await;
                d.rationale
                    .push_str(" (llm classify unavailable → heuristic)");
                d
            }
        }
    }

    async fn route_hinted(
        &self,
        prompt: &str,
        budget: BudgetState,
        health: &ModelHealth,
        quota: &SubscriptionQuota,
        tier_override: Option<TaskTier>,
    ) -> RoutingDecision {
        match tier_override {
            // An explicit command/skill tier hint skips the classifier model call entirely.
            Some(tier) => self.fallback.decide(
                tier,
                format!("tier hint: {}", tier.as_str()),
                budget,
                health,
                RouteHints::from_prompt(prompt),
                quota,
            ),
            None => self.route(prompt, budget, health, quota).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use forge_provider::{EventSink, ModelResponse, ProviderError, ToolSpec};

    /// A provider that returns a fixed classification reply, or an error.
    struct FakeProvider(Result<String, ()>);

    #[async_trait]
    impl Provider for FakeProvider {
        async fn complete(
            &self,
            _model: &str,
            _messages: &[Message],
            _tools: &[ToolSpec],
            _on_event: &mut EventSink<'_>,
        ) -> Result<ModelResponse, ProviderError> {
            match &self.0 {
                Ok(text) => Ok(ModelResponse {
                    content: text.clone(),
                    tool_calls: Vec::new(),
                    usage: Default::default(),
                    quotas: Vec::new(),
                }),
                Err(()) => Err(ProviderError::Request("boom".into())),
            }
        }
    }

    fn llm_router(reply: Result<&str, ()>) -> LlmRouter {
        let provider = Arc::new(FakeProvider(reply.map(String::from)));
        let fallback = HeuristicRouter::new(forge_config::Config::default());
        LlmRouter::new(provider, "ollama::tiny".into(), fallback)
    }

    #[test]
    fn parses_tier_words_tolerantly() {
        assert_eq!(parse_tier("complex"), Some(TaskTier::Complex));
        assert_eq!(parse_tier("Standard."), Some(TaskTier::Standard));
        assert_eq!(parse_tier("  trivial\n"), Some(TaskTier::Trivial));
        assert_eq!(
            parse_tier("I think this is standard"),
            Some(TaskTier::Standard)
        );
        assert_eq!(parse_tier("banana"), None);
    }

    #[tokio::test]
    async fn uses_the_llm_label() {
        // A short prompt the heuristic would call Trivial, but the LLM says complex.
        let d = llm_router(Ok("complex"))
            .route(
                "tweak it",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Complex); // AC-B1
        assert!(d.rationale.contains("classified by"), "{}", d.rationale);
    }

    #[tokio::test]
    async fn falls_back_on_gibberish() {
        let d = llm_router(Ok("banana"))
            .route(
                "design a lock-free queue",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        // heuristic catches the hard prompt
        assert_eq!(d.tier, TaskTier::Complex);
        assert!(d.rationale.contains("heuristic"), "{}", d.rationale); // AC-B2
    }

    #[tokio::test]
    async fn falls_back_on_provider_error() {
        let d = llm_router(Err(()))
            .route(
                "fix typo",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(d.tier, TaskTier::Trivial);
        assert!(d.rationale.contains("heuristic"), "{}", d.rationale); // AC-B2
    }

    fn hybrid_router(reply: Result<&str, ()>) -> LlmRouter {
        let provider = Arc::new(FakeProvider(reply.map(String::from)));
        let fallback = HeuristicRouter::new(forge_config::Config::default());
        LlmRouter::new(provider, "ollama::tiny".into(), fallback).with_hybrid(true)
    }

    #[tokio::test]
    async fn hybrid_skips_llm_for_confident_trivial() {
        // "typo" hits TRIVIAL_PATTERNS → score −4 → confident → LLM must NOT be called.
        // The FakeProvider would return "complex" if called, revealing whether the skip worked.
        let d = hybrid_router(Ok("complex"))
            .route(
                "fix the typo in the readme",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(
            d.tier,
            TaskTier::Trivial,
            "hybrid must not call LLM for confident Trivial: {}",
            d.rationale
        );
        assert!(
            d.rationale.contains("confident"),
            "rationale should mention confident fast-path: {}",
            d.rationale
        );
    }

    #[tokio::test]
    async fn hybrid_skips_llm_for_confident_complex() {
        // REASONING_TERM (+5) + two ANALYSIS_TERMS (+3 each) → score 11 ≥ 8 → confident.
        // FakeProvider returns "trivial" — if called, tier would flip; it must be skipped.
        let d = hybrid_router(Ok("trivial"))
            .route(
                "analyze the performance bottleneck in the authentication service",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(
            d.tier,
            TaskTier::Complex,
            "hybrid must not call LLM for confident Complex: {}",
            d.rationale
        );
        assert!(
            d.rationale.contains("confident"),
            "rationale should mention confident fast-path: {}",
            d.rationale
        );
    }

    #[tokio::test]
    async fn hybrid_calls_llm_for_uncertain_standard() {
        // "add a function" → score ~2 (Standard, uncertain) → LLM IS called and overrides.
        // FakeProvider returns "complex" → tier should become Complex.
        let d = hybrid_router(Ok("complex"))
            .route(
                "add a function that validates emails",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(
            d.tier,
            TaskTier::Complex,
            "hybrid must use LLM for uncertain Standard: {}",
            d.rationale
        );
        assert!(
            d.rationale.contains("classified by"),
            "rationale should show llm result: {}",
            d.rationale
        );
    }

    #[tokio::test]
    async fn hybrid_calls_llm_for_barely_complex_prompt() {
        // Single REASONING_TERM → score 5 (barely Complex, uncertain) → LLM IS called.
        // FakeProvider returns "standard" → tier becomes Standard.
        let d = hybrid_router(Ok("standard"))
            .route(
                "refactor this helper",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        assert_eq!(
            d.tier,
            TaskTier::Standard,
            "hybrid must use LLM for barely-Complex uncertain prompt: {}",
            d.rationale
        );
    }

    #[tokio::test]
    async fn hybrid_falls_back_gracefully_when_llm_fails() {
        // Uncertain prompt + provider error → heuristic tier used.
        let d = hybrid_router(Err(()))
            .route(
                "implement a small utility function",
                BudgetState::default(),
                &ModelHealth::default(),
                &SubscriptionQuota::default(),
            )
            .await;
        // Heuristic gives Standard for this prompt.
        assert_eq!(d.tier, TaskTier::Standard);
        assert!(d.rationale.contains("heuristic"), "{}", d.rationale);
    }
}
