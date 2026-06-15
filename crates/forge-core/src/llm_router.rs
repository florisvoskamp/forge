//! Optional cheap-LLM task classifier (ADR-0006 option 2, opt-in). Asks a small model to
//! label the tier before routing, then reuses the heuristic router's pin/budget/cost-aware
//! selection. Any failure — error, timeout, or an unparseable reply — silently falls back to
//! the deterministic heuristic, so enabling it can never break a turn. Off by default (A-2:
//! no per-task model call unless the user opts in via `mesh.classifier = "llm"`).

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use forge_mesh::{BudgetState, HeuristicRouter, Router, RoutingDecision};
use forge_provider::Provider;
use forge_types::{Message, ModelHealth, TaskTier};

/// Hard ceiling on the classification call so a slow/hung model degrades to the heuristic.
const CLASSIFY_TIMEOUT: Duration = Duration::from_secs(15);

const CLASSIFY_SYSTEM: &str = "You classify a software-engineering task by difficulty. Reply \
with EXACTLY ONE lowercase word, no punctuation: trivial, standard, or complex. \
trivial = a one-line or mechanical edit (typo, rename, format). \
standard = an ordinary coding task (a function, an endpoint, a small feature). \
complex = needs deep reasoning, architecture, algorithms, concurrency, or debugging.";

/// A [`Router`] that labels the tier with a cheap model, falling back to `fallback`.
pub struct LlmRouter {
    provider: Arc<dyn Provider>,
    model: String,
    fallback: HeuristicRouter,
}

impl LlmRouter {
    pub fn new(provider: Arc<dyn Provider>, model: String, fallback: HeuristicRouter) -> Self {
        Self {
            provider,
            model,
            fallback,
        }
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
    ) -> RoutingDecision {
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
            ),
            None => {
                // Couldn't classify → deterministic heuristic, noted in the rationale.
                let mut d = self.fallback.route(prompt, budget, health).await;
                d.rationale
                    .push_str(" (llm classify unavailable → heuristic)");
                d
            }
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
            .route("tweak it", BudgetState::default(), &ModelHealth::default())
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
            )
            .await;
        // heuristic catches the hard prompt
        assert_eq!(d.tier, TaskTier::Complex);
        assert!(d.rationale.contains("heuristic"), "{}", d.rationale); // AC-B2
    }

    #[tokio::test]
    async fn falls_back_on_provider_error() {
        let d = llm_router(Err(()))
            .route("fix typo", BudgetState::default(), &ModelHealth::default())
            .await;
        assert_eq!(d.tier, TaskTier::Trivial);
        assert!(d.rationale.contains("heuristic"), "{}", d.rationale); // AC-B2
    }
}
