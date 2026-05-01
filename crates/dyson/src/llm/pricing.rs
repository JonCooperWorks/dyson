// ===========================================================================
// Pricing — per-model USD rate cards for tracking agent cost.
//
// Pricing is a first-class concept in Dyson: every agent run knows, in real
// time, how much it has billed.  That number flows into artefact metadata
// so a security-review report carries its own cost tag, and into the
// /api/conversations/:id/events stream so UIs can surface a live running
// cost chip.
//
// The registry here is a small hand-curated table of published public rate
// cards for the models Dyson is commonly pointed at.  It is not exhaustive
// — unknown models simply don't get a cost computed (the token counts are
// still recorded).  Operators can override or extend via dyson.json (see
// `ProviderConfig.pricing_overrides`) without rebuilding.
//
// Unit convention throughout this module: dollars per **million** tokens
// (USD/MTok), matching the headline numbers on provider price pages.  We
// convert once to USD when computing `cost_usd` so arithmetic stays
// floating-point clean.
//
// Source (as of 2026-04): Anthropic, OpenAI, and OpenRouter public pricing
// pages.  Prompt-caching and batch discounts are NOT modeled — they'd
// require per-request cache-hit counts from the provider and a richer
// accounting model than we need for "approximate cost of this run."
// ===========================================================================

use crate::config::LlmProvider;

/// Per-model pricing in USD per million tokens.  Extra fields (cache-read,
/// cache-write) can be added later without breaking existing consumers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelPricing {
    /// Input (prompt) cost, USD per million tokens.
    pub input_per_mtok: f64,
    /// Output (completion) cost, USD per million tokens.
    pub output_per_mtok: f64,
}

impl ModelPricing {
    /// Construct a pricing record from rate-card values.
    pub const fn new(input_per_mtok: f64, output_per_mtok: f64) -> Self {
        Self {
            input_per_mtok,
            output_per_mtok,
        }
    }

    /// Compute the total USD cost for a given (input, output) token pair.
    pub fn cost_usd(&self, input_tokens: usize, output_tokens: usize) -> f64 {
        let input = (input_tokens as f64) * self.input_per_mtok / 1_000_000.0;
        let output = (output_tokens as f64) * self.output_per_mtok / 1_000_000.0;
        input + output
    }
}

/// Look up pricing for a (provider, model) pair.  Returns `None` for
/// models the built-in table doesn't know — caller decides whether to
/// omit the cost line, fall back to a configured override, or estimate
/// from a provider-family default.
///
/// Model matching is case-insensitive and ignores common version
/// suffixes (`-20250514`, `-preview`, ...) so `claude-opus-4-7` and
/// `claude-opus-4-7-20250514` both hit the same entry.  Keeps the table
/// small and makes pricing resilient to provider renames.
pub fn lookup(provider: &LlmProvider, model: &str) -> Option<ModelPricing> {
    let key = normalize(model);
    let table: &[(&str, ModelPricing)] = match provider {
        LlmProvider::Anthropic => ANTHROPIC,
        LlmProvider::OpenAi => OPENAI,
        LlmProvider::OpenRouter => OPENROUTER,
        // CLI/subprocess providers bill out-of-band (the CLI itself reports
        // cost in its JSON output stream — see claude_code.rs).  Don't
        // double-count: return None and let the out-of-band number be the
        // source of truth when one is available.
        _ => return None,
    };

    // Prefer exact normalised match.  Fall back to prefix match so
    // `claude-opus-4` catches `claude-opus-4-7` (useful when a new
    // dashed suffix ships and this table hasn't been updated yet).
    if let Some((_, p)) = table.iter().find(|(name, _)| *name == key) {
        return Some(*p);
    }
    let mut best: Option<(&str, ModelPricing)> = None;
    for (name, pricing) in table {
        if key.starts_with(name) && best.is_none_or(|(b, _)| name.len() > b.len()) {
            best = Some((name, *pricing));
        }
    }
    best.map(|(_, p)| p)
}

/// Lower-case the model name and strip trailing date/version suffixes so
/// the table stays small.  `claude-opus-4-7-20250514` → `claude-opus-4-7`.
fn normalize(model: &str) -> String {
    let lower = model.to_ascii_lowercase();
    // Strip a trailing -YYYYMMDD date stamp (Anthropic convention).
    if let Some(pos) = lower.rfind('-') {
        let tail = &lower[pos + 1..];
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            return lower[..pos].to_string();
        }
    }
    lower
}

// ---------------------------------------------------------------------------
// Built-in rate cards.  USD per million tokens, sourced from provider public
// pricing pages as of 2026-04.  Keep entries sorted by family then tier for
// ease of audit.
// ---------------------------------------------------------------------------

const ANTHROPIC: &[(&str, ModelPricing)] = &[
    // Claude 4.x family — current flagship line.
    ("claude-opus-4-7", ModelPricing::new(15.00, 75.00)),
    ("claude-opus-4-6", ModelPricing::new(15.00, 75.00)),
    ("claude-opus-4", ModelPricing::new(15.00, 75.00)),
    ("claude-sonnet-4-6", ModelPricing::new(3.00, 15.00)),
    ("claude-sonnet-4", ModelPricing::new(3.00, 15.00)),
    ("claude-haiku-4-5", ModelPricing::new(1.00, 5.00)),
    ("claude-haiku-4", ModelPricing::new(1.00, 5.00)),
    // Claude 3.x fall-through for older configs.
    ("claude-3-5-sonnet", ModelPricing::new(3.00, 15.00)),
    ("claude-3-5-haiku", ModelPricing::new(0.80, 4.00)),
    ("claude-3-opus", ModelPricing::new(15.00, 75.00)),
    ("claude-3-haiku", ModelPricing::new(0.25, 1.25)),
];

const OPENAI: &[(&str, ModelPricing)] = &[
    ("gpt-4o", ModelPricing::new(2.50, 10.00)),
    ("gpt-4o-mini", ModelPricing::new(0.15, 0.60)),
    ("gpt-4.1", ModelPricing::new(2.00, 8.00)),
    ("gpt-4.1-mini", ModelPricing::new(0.40, 1.60)),
    ("gpt-4.1-nano", ModelPricing::new(0.10, 0.40)),
    ("o1", ModelPricing::new(15.00, 60.00)),
    ("o1-mini", ModelPricing::new(3.00, 12.00)),
    ("o3", ModelPricing::new(2.00, 8.00)),
    ("o3-mini", ModelPricing::new(1.10, 4.40)),
    ("o4-mini", ModelPricing::new(1.10, 4.40)),
];

// OpenRouter proxies many underlying providers; model identifiers include
// the publisher prefix (e.g. `anthropic/claude-opus-4-7`).  List the ones
// we actually use against OpenRouter in the smoke examples.  Unknown IDs
// fall through to None.
const OPENROUTER: &[(&str, ModelPricing)] = &[
    ("anthropic/claude-opus-4-7", ModelPricing::new(15.00, 75.00)),
    (
        "anthropic/claude-sonnet-4-6",
        ModelPricing::new(3.00, 15.00),
    ),
    ("anthropic/claude-haiku-4-5", ModelPricing::new(1.00, 5.00)),
    ("openai/gpt-4o", ModelPricing::new(2.50, 10.00)),
    ("openai/gpt-4o-mini", ModelPricing::new(0.15, 0.60)),
    ("google/gemini-2.5-pro", ModelPricing::new(1.25, 10.00)),
    ("qwen/qwen-2.5-72b-instruct", ModelPricing::new(0.35, 0.40)),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_computation_scales_linearly() {
        let p = ModelPricing::new(10.0, 30.0);
        // 1M input * $10 + 1M output * $30 = $40.
        assert!((p.cost_usd(1_000_000, 1_000_000) - 40.0).abs() < 1e-9);
        // Half = half.
        assert!((p.cost_usd(500_000, 500_000) - 20.0).abs() < 1e-9);
        // Zero = zero.
        assert_eq!(p.cost_usd(0, 0), 0.0);
    }

    #[test]
    fn lookup_hits_exact_normalised_name() {
        let p = lookup(&LlmProvider::Anthropic, "claude-opus-4-7").unwrap();
        assert_eq!(p.input_per_mtok, 15.00);
        assert_eq!(p.output_per_mtok, 75.00);
    }

    #[test]
    fn lookup_strips_date_suffix() {
        let p = lookup(&LlmProvider::Anthropic, "claude-opus-4-7-20250514").unwrap();
        assert_eq!(p.input_per_mtok, 15.00);
    }

    #[test]
    fn lookup_uppercase_matches() {
        let p = lookup(&LlmProvider::Anthropic, "Claude-Opus-4-7").unwrap();
        assert_eq!(p.input_per_mtok, 15.00);
    }

    #[test]
    fn lookup_prefix_fallback_takes_longest_match() {
        // `claude-opus-4-9` (unreleased) shouldn't hit `claude-opus-4-7`
        // — fall through to the `claude-opus-4` prefix (same tier).
        let p = lookup(&LlmProvider::Anthropic, "claude-opus-4-9").unwrap();
        assert_eq!(p.input_per_mtok, 15.00);
    }

    #[test]
    fn lookup_unknown_model_returns_none() {
        assert!(lookup(&LlmProvider::Anthropic, "mystery-model-9000").is_none());
    }

    #[test]
    fn cli_providers_return_none() {
        // claude-code and codex bill via their own CLI output stream —
        // Dyson must not double-count those as USD cost here.
        assert!(lookup(&LlmProvider::ClaudeCode, "claude-opus-4-7").is_none());
    }
}
