//! Per-model pricing and cost computation (FR-5, A-7). Rates are bundled defaults and
//! user-overridable via config, so a provider price change needs no release.

use std::collections::HashMap;

/// USD price per 1,000 tokens for a model's input and output.
#[derive(Debug, Clone, Copy)]
pub struct ModelRate {
    pub input_per_1k: f64,
    pub output_per_1k: f64,
}

impl From<forge_config::PriceOverride> for ModelRate {
    fn from(o: forge_config::PriceOverride) -> Self {
        ModelRate {
            input_per_1k: o.input_per_1k,
            output_per_1k: o.output_per_1k,
        }
    }
}

/// A table of model id -> rate. Unknown models cost nothing (e.g. local Ollama).
#[derive(Debug, Clone)]
pub struct Pricing {
    rates: HashMap<String, ModelRate>,
}

/// Bundled default rates (USD per 1k tokens) for the models Forge ships in its defaults,
/// approximating mid-2026 list prices. Overridable via config (A-7).
const DEFAULT_RATES: &[(&str, f64, f64)] = &[
    ("openai::gpt-4o-mini", 0.00015, 0.0006),
    ("anthropic::claude-opus-4-8", 0.015, 0.075),
    // Local models (e.g. ollama::*) are intentionally absent -> free.
];

impl Default for Pricing {
    fn default() -> Self {
        let rates = DEFAULT_RATES
            .iter()
            .map(|&(id, input_per_1k, output_per_1k)| {
                (
                    id.to_string(),
                    ModelRate {
                        input_per_1k,
                        output_per_1k,
                    },
                )
            })
            .collect();
        Self { rates }
    }
}

impl Pricing {
    /// Build from explicit rates (used by config overrides and tests).
    pub fn from_rates(rates: HashMap<String, ModelRate>) -> Self {
        Self { rates }
    }

    /// Apply user overrides on top of the defaults (overrides win per model id).
    pub fn with_overrides(mut self, overrides: HashMap<String, ModelRate>) -> Self {
        self.rates.extend(overrides);
        self
    }

    /// Bundled defaults with the config's per-model overrides applied (A-7).
    pub fn from_config(config: &forge_config::Config) -> Self {
        let overrides = config
            .mesh
            .pricing
            .iter()
            .map(|(id, &o)| (id.clone(), o.into()))
            .collect();
        Pricing::default().with_overrides(overrides)
    }

    /// Compute the USD cost of a call given token counts. Unknown models cost nothing.
    pub fn cost_for(&self, model: &str, input_tokens: u64, output_tokens: u64) -> f64 {
        match self.rates.get(model) {
            Some(rate) => {
                (input_tokens as f64 / 1000.0) * rate.input_per_1k
                    + (output_tokens as f64 / 1000.0) * rate.output_per_1k
            }
            None => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_is_tokens_times_rate_per_1k() {
        let mut rates = HashMap::new();
        rates.insert(
            "openai::gpt-4o-mini".to_string(),
            ModelRate {
                input_per_1k: 0.00015,
                output_per_1k: 0.0006,
            },
        );
        let pricing = Pricing { rates };

        // 1000 input @ 0.00015 + 2000 output @ 0.0006 = 0.00015 + 0.0012 = 0.00135
        let cost = pricing.cost_for("openai::gpt-4o-mini", 1000, 2000);
        assert!((cost - 0.00135).abs() < 1e-9, "got {cost}");
    }

    #[test]
    fn unknown_model_is_free() {
        let pricing = Pricing::default();
        assert_eq!(pricing.cost_for("ollama::llama3.2", 5000, 5000), 0.0);
    }

    #[test]
    fn defaults_price_the_paid_models() {
        let pricing = Pricing::default();
        assert!(pricing.cost_for("openai::gpt-4o-mini", 1000, 1000) > 0.0);
        assert!(pricing.cost_for("anthropic::claude-opus-4-8", 1000, 1000) > 0.0);
    }

    #[test]
    fn config_overrides_win_over_defaults() {
        let mut config = forge_config::Config::default();
        config.mesh.pricing.insert(
            "openai::gpt-4o-mini".to_string(),
            forge_config::PriceOverride {
                input_per_1k: 1.0,
                output_per_1k: 2.0,
            },
        );
        let pricing = Pricing::from_config(&config);
        // 1000 in * 1.0/1k + 1000 out * 2.0/1k = 1.0 + 2.0 = 3.0
        assert!((pricing.cost_for("openai::gpt-4o-mini", 1000, 1000) - 3.0).abs() < 1e-9);
    }
}
