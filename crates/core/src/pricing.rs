//! Per-token pricing for known models. Users can override in config.toml.
//! Rates are USD per 1M tokens unless noted. Multipliers expressed as fractions
//! of the input rate.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ModelRate {
    /// USD per 1M input tokens (regular, no caching).
    pub input_per_mtok: f64,
    /// USD per 1M output tokens.
    pub output_per_mtok: f64,
    /// Multiplier applied to input rate for 5-minute cache writes.
    pub cache_write_5m_mult: f64,
    /// Multiplier applied to input rate for 1-hour cache writes.
    pub cache_write_1h_mult: f64,
    /// Multiplier applied to input rate for cache reads.
    pub cache_read_mult: f64,
}

impl ModelRate {
    pub const fn anthropic_default() -> Self {
        // Conservative defaults; user should override per-model in config.
        Self {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_5m_mult: 1.25,
            cache_write_1h_mult: 2.0,
            cache_read_mult: 0.10,
        }
    }
}

/// Look up default Anthropic pricing by model id (substring match).
pub fn anthropic_default(model: &str) -> ModelRate {
    let m = model.to_ascii_lowercase();
    // Order matters — most specific first.
    if m.contains("opus") {
        ModelRate {
            input_per_mtok: 15.0,
            output_per_mtok: 75.0,
            cache_write_5m_mult: 1.25,
            cache_write_1h_mult: 2.0,
            cache_read_mult: 0.10,
        }
    } else if m.contains("haiku") {
        ModelRate {
            input_per_mtok: 1.0,
            output_per_mtok: 5.0,
            cache_write_5m_mult: 1.25,
            cache_write_1h_mult: 2.0,
            cache_read_mult: 0.10,
        }
    } else if m.contains("sonnet") {
        ModelRate {
            input_per_mtok: 3.0,
            output_per_mtok: 15.0,
            cache_write_5m_mult: 1.25,
            cache_write_1h_mult: 2.0,
            cache_read_mult: 0.10,
        }
    } else {
        ModelRate::anthropic_default()
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AnthropicTokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_5m_input_tokens: u64,
    pub cache_creation_1h_input_tokens: u64,
}

impl AnthropicTokenUsage {
    pub fn cost_usd(&self, rate: ModelRate) -> f64 {
        let per_tok = |usd_per_m: f64| usd_per_m / 1_000_000.0;
        let input_rate = per_tok(rate.input_per_mtok);
        let output_rate = per_tok(rate.output_per_mtok);
        let mut cost = 0.0;
        cost += self.input_tokens as f64 * input_rate;
        cost += self.output_tokens as f64 * output_rate;
        cost += self.cache_read_input_tokens as f64 * input_rate * rate.cache_read_mult;
        cost += self.cache_creation_5m_input_tokens as f64 * input_rate * rate.cache_write_5m_mult;
        cost += self.cache_creation_1h_input_tokens as f64 * input_rate * rate.cache_write_1h_mult;
        cost
    }

    pub fn total_billed_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.output_tokens)
            .saturating_add(self.cache_read_input_tokens)
            .saturating_add(self.cache_creation_5m_input_tokens)
            .saturating_add(self.cache_creation_1h_input_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opus_cost_smoke() {
        let r = anthropic_default("claude-opus-4-7");
        let u = AnthropicTokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 0,
            ..Default::default()
        };
        // 1M input @ $15/Mtok = $15.
        assert!((u.cost_usd(r) - 15.0).abs() < 1e-6);
    }

    #[test]
    fn cache_read_cheap() {
        let r = anthropic_default("claude-sonnet-4-6");
        let u = AnthropicTokenUsage {
            cache_read_input_tokens: 1_000_000,
            ..Default::default()
        };
        // 1M cache read @ $3/Mtok * 0.1 = $0.30.
        assert!((u.cost_usd(r) - 0.30).abs() < 1e-6);
    }
}
