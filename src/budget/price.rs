//! Prices come from the selected `[[model]]` entry, never from a constant.

use serde::{Deserialize, Serialize};

use crate::config::Model;

/// Tokens one call consumed. `input_tokens` includes the cached part, the way
/// the vendors report it. `output_tokens` already includes reasoning / thinking
/// tokens; `output_tokens_details.reasoning_tokens` is a breakdown, not extra.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub cached_input_tokens: u32,
    pub output_tokens: u32,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.output_tokens += other.output_tokens;
    }
}

/// Per million token prices, in the currency of the model's provider.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Serialize)]
pub struct Price {
    pub input_per_1m: f64,
    pub cached_input_per_1m: Option<f64>,
    pub output_per_1m: f64,
}

impl Price {
    pub fn from_model(model: &Model) -> Self {
        Self {
            input_per_1m: model.input_per_1m,
            cached_input_per_1m: model.cached_input_per_1m,
            output_per_1m: model.output_per_1m,
        }
    }

    /// Cached input is billed at `cached_input_per_1m` when the model entry
    /// gives one; without one it costs the same as fresh input.
    pub fn cost(&self, usage: &TokenUsage) -> f64 {
        const PER_MILLION: f64 = 1_000_000.0;
        let cached = usage.cached_input_tokens.min(usage.input_tokens);
        let fresh = usage.input_tokens - cached;
        let cached_rate = self.cached_input_per_1m.unwrap_or(self.input_per_1m);
        (fresh as f64 * self.input_per_1m
            + cached as f64 * cached_rate
            + usage.output_tokens as f64 * self.output_per_1m)
            / PER_MILLION
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn price() -> Price {
        Price {
            input_per_1m: 2.0,
            cached_input_per_1m: Some(0.2),
            output_per_1m: 3.0,
        }
    }

    #[test]
    fn cached_input_is_billed_at_the_cached_rate() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            output_tokens: 0,
        };
        assert!((price().cost(&usage) - 0.2).abs() < 1e-9);
    }

    #[test]
    fn fresh_and_cached_input_are_billed_separately() {
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 500_000,
            output_tokens: 1_000_000,
        };
        // 0.5M fresh at 2.0, 0.5M cached at 0.2, 1M output at 3.0.
        assert!((price().cost(&usage) - 4.1).abs() < 1e-9);
    }

    #[test]
    fn a_model_without_a_cached_price_pays_full_rate() {
        let price = Price {
            input_per_1m: 8.0,
            cached_input_per_1m: None,
            output_per_1m: 24.0,
        };
        let usage = TokenUsage {
            input_tokens: 1_000_000,
            cached_input_tokens: 1_000_000,
            output_tokens: 0,
        };
        assert!((price.cost(&usage) - 8.0).abs() < 1e-9);
    }
}
