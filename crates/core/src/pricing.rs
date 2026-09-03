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

/// Default Muse Spark pricing (Meta Model API standard tier, 2026):
/// $1.25 / 1M input, $4.25 / 1M output, $0.15 / 1M cached input.
/// Expressed through the shared [`ModelRate`] shape so the standard
/// cost math applies: cached input is 0.12× the input rate
/// (1.25 × 0.12 = 0.15). Callers must pass cached tokens as the
/// read bucket — session logs report them as a subset of input
/// (OpenAI-style), matching `cost_usd`'s additive buckets only when
/// the caller moves them out of `input_tokens` first, as
/// [`muse_spark_cost_usd`] does.
pub fn muse_spark_default() -> ModelRate {
    ModelRate {
        input_per_mtok: 1.25,
        output_per_mtok: 4.25,
        cache_write_5m_mult: 1.0,
        cache_write_1h_mult: 1.0,
        cache_read_mult: 0.12,
    }
}

/// Cost of one Muse Spark step. `cached` is a subset of `input`
/// (verified against session logs: each step's cached count ≈ the
/// previous step's input), so only the uncached remainder bills at
/// the full input rate. Reasoning tokens bill at the output rate —
/// Meta publishes no separate reasoning price, and every sibling
/// provider bills thinking at output.
pub fn muse_spark_cost_usd(input: u64, output: u64, cached: u64, reasoning: u64) -> f64 {
    let usage = AnthropicTokenUsage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: output.saturating_add(reasoning),
        cache_read_input_tokens: cached.min(input),
        ..Default::default()
    };
    usage.cost_usd(muse_spark_default())
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

    #[test]
    fn rate_lookup_matches_substring_case_insensitive() {
        // "Opus" anywhere — match wins over base default.
        let r = anthropic_default("Claude-OPUS-something");
        assert_eq!(r.input_per_mtok, 15.0);
        let r = anthropic_default("claude-haiku-4");
        assert_eq!(r.input_per_mtok, 1.0);
        let r = anthropic_default("claude-sonnet-4-6");
        assert_eq!(r.input_per_mtok, 3.0);
    }

    #[test]
    fn unknown_model_falls_back_to_anthropic_default() {
        let r = anthropic_default("unknown-model");
        let baseline = ModelRate::anthropic_default();
        assert_eq!(r.input_per_mtok, baseline.input_per_mtok);
        assert_eq!(r.output_per_mtok, baseline.output_per_mtok);
    }

    #[test]
    fn output_dominates_at_naive_input_output_mix() {
        // Output is 5x input on opus — confirm the math.
        let r = anthropic_default("claude-opus");
        let u = AnthropicTokenUsage {
            input_tokens: 1_000_000,
            output_tokens: 1_000_000,
            ..Default::default()
        };
        // 1M * $15 + 1M * $75 = $90.
        assert!((u.cost_usd(r) - 90.0).abs() < 1e-6);
    }

    #[test]
    fn cache_writes_apply_correct_multipliers() {
        let r = anthropic_default("claude-sonnet-4-6"); // input $3
        let u = AnthropicTokenUsage {
            cache_creation_5m_input_tokens: 1_000_000, // * 1.25 → $3.75
            cache_creation_1h_input_tokens: 1_000_000, // * 2.0  → $6.00
            ..Default::default()
        };
        // 3.75 + 6.00 = 9.75
        assert!((u.cost_usd(r) - 9.75).abs() < 1e-6);
    }

    #[test]
    fn total_billed_tokens_sums_every_bucket() {
        let u = AnthropicTokenUsage {
            input_tokens: 100,
            output_tokens: 200,
            cache_read_input_tokens: 300,
            cache_creation_5m_input_tokens: 400,
            cache_creation_1h_input_tokens: 500,
        };
        assert_eq!(u.total_billed_tokens(), 1500);
    }

    #[test]
    fn empty_usage_costs_zero() {
        let r = anthropic_default("anything");
        let u = AnthropicTokenUsage::default();
        assert_eq!(u.cost_usd(r), 0.0);
    }

    #[test]
    fn muse_spark_standard_tier_rates() {
        // 1M input + 1M output, nothing cached:
        // 1.25 + 4.25 = $5.50.
        assert!((muse_spark_cost_usd(1_000_000, 1_000_000, 0, 0) - 5.50).abs() < 1e-9);
    }

    #[test]
    fn muse_spark_cached_subset_bills_at_cached_rate() {
        // 1M input fully cached: 1M × $0.15, not 1M × $1.25.
        assert!((muse_spark_cost_usd(1_000_000, 0, 1_000_000, 0) - 0.15).abs() < 1e-9);
    }

    #[test]
    fn muse_spark_reasoning_bills_as_output() {
        // 1M reasoning tokens at the $4.25 output rate.
        assert!((muse_spark_cost_usd(0, 0, 0, 1_000_000) - 4.25).abs() < 1e-9);
    }
}
