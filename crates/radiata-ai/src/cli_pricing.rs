//! List-price cost estimates for subscription CLI providers.
//!
//! Antigravity, Claude Code, and Cursor bill against a plan, so their streams report token counts
//! but rarely a per-request price. These helpers price those tokens at public API list rates
//! ($/million tokens) so session cost and context rollups stay comparable with metered providers.
//! A provider-reported USD total always wins over the token estimate.

use std::collections::HashMap;
use std::sync::LazyLock;

use serde_json::Value;

use crate::models::usage_cost_at_rates;
use crate::models_generated::{get_models, get_providers};
use crate::types::{JsNumber, Model, ModelCost, Usage, UsageCost};

/// Catalog providers whose prices are first-party list prices; they win over resellers when the
/// same normalized model id appears more than once.
const PREFERRED_PROVIDERS: &[&str] = &["anthropic", "openai", "openai-codex", "google"];

/// Effort and mode suffixes the CLIs append to wire model ids. `-extra-high` sits before
/// `-high` so the longer suffix is removed whole.
const MODE_SUFFIXES: &[&str] = &[
    "-extra-high",
    "-xhigh",
    "-medium",
    "-high",
    "-low",
    "-max",
    "-fast",
    "-thinking",
];

/// Opus 4.0/4.1 kept the original $15/$75 list price; later Opus releases are cheaper.
const LEGACY_OPUS_KEYS: &[&str] = &[
    "claude-opus-4",
    "claude-opus-4-0",
    "claude-opus-4-1",
    "claude-4-opus",
    "claude-4-0-opus",
    "claude-4-1-opus",
];

/// Catalog entries used for current-generation Opus ids that the catalog does not list yet,
/// newest first.
const CURRENT_OPUS_KEYS: &[&str] = &["claude-opus-5", "claude-opus-4-6", "claude-opus-4-5"];

const LEGACY_OPUS_RATES: (f64, f64, f64, f64) = (15.0, 75.0, 1.5, 18.75);

/// Keys in the payloads of the CLIs that carry a provider-computed USD total.
const REPORTED_COST_KEYS: &[&str] = &["total_cost_usd", "cost_usd", "costUsd"];

/// The list price for `model_id`, from the compiled catalog first, then from family rates.
#[must_use]
pub fn list_price_for_model(model_id: &str) -> Option<ModelCost> {
    let key = normalize(model_id);
    if key.is_empty() {
        return None;
    }
    let stripped = strip_mode_suffixes(&key);
    catalog_price(&key)
        .or_else(|| catalog_price(stripped))
        .or_else(|| family_price(stripped))
}

/// Price `usage` and write the breakdown to `usage.cost`.
///
/// Rates come from `model.cost` when it is non-zero, else from the list price of
/// `response_model`, else from the list price of `model.id`. A finite, positive
/// `reported_total_usd` replaces the computed total and keeps the component breakdown.
pub fn apply_estimated_cost(
    model: &Model,
    response_model: Option<&str>,
    usage: &mut Usage,
    reported_total_usd: Option<f64>,
) {
    let rates = if has_rates(&model.cost) {
        Some(model.cost)
    } else {
        response_model
            .and_then(list_price_for_model)
            .or_else(|| list_price_for_model(&model.id))
    };
    let mut cost = rates.map_or_else(UsageCost::default, |rates| {
        usage_cost_at_rates(&rates, usage)
    });
    if let Some(total) = reported_total_usd.filter(|v| v.is_finite() && *v > 0.0) {
        cost.total = JsNumber::from(total);
    }
    usage.cost = cost;
}

/// The provider-reported USD total in `value`, if any key carries a finite, positive number.
#[must_use]
pub fn reported_cost_usd(value: &Value) -> Option<f64> {
    REPORTED_COST_KEYS
        .iter()
        .filter_map(|key| value[*key].as_f64())
        .find(|v| v.is_finite() && *v > 0.0)
}

fn has_rates(cost: &ModelCost) -> bool {
    cost.input.as_f64() > 0.0 || cost.output.as_f64() > 0.0
}

/// Canonical lookup key: lowercase, no provider or Bedrock region prefix, no Bedrock version,
/// date, or `[1m]` context suffix, and `.` written as `-` so `claude-opus-4.6` and
/// `claude-opus-4-6` meet.
fn normalize(model_id: &str) -> String {
    let lower = model_id.trim().to_ascii_lowercase();
    let mut key = lower.rsplit('/').next().unwrap_or_default();
    key = key.trim_start_matches('~');
    if let Some(index) = key.find("anthropic.") {
        key = &key[index + "anthropic.".len()..];
    }
    if let Some(index) = key.find('[') {
        key = &key[..index];
    }
    key = strip_trailing_segment(key, |segment| {
        segment.strip_prefix('v').is_some_and(|version| {
            !version.is_empty()
                && version
                    .split(':')
                    .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
        })
    });
    key = strip_trailing_segment(key, |segment| {
        segment.len() == 8 && segment.bytes().all(|b| b.is_ascii_digit())
    });
    key.replace('.', "-")
}

fn strip_trailing_segment(key: &str, matches: impl Fn(&str) -> bool) -> &str {
    match key.rsplit_once('-') {
        Some((stem, segment)) if !stem.is_empty() && matches(segment) => stem,
        _ => key,
    }
}

fn strip_mode_suffixes(key: &str) -> &str {
    let mut key = key;
    while let Some(stem) = MODE_SUFFIXES
        .iter()
        .find_map(|suffix| key.strip_suffix(suffix).filter(|stem| !stem.is_empty()))
    {
        key = stem;
    }
    key
}

fn catalog_price(key: &str) -> Option<ModelCost> {
    static INDEX: LazyLock<HashMap<String, ModelCost>> = LazyLock::new(|| {
        let mut providers = get_providers();
        providers.sort_by_key(|provider| {
            PREFERRED_PROVIDERS
                .iter()
                .position(|preferred| preferred == provider)
                .unwrap_or(PREFERRED_PROVIDERS.len())
        });
        let mut index = HashMap::new();
        for provider in providers {
            for model in get_models(provider) {
                if has_rates(&model.cost) {
                    index.entry(normalize(&model.id)).or_insert(model.cost);
                }
            }
        }
        index
    });
    INDEX.get(key).copied()
}

fn rates((input, output, cache_read, cache_write): (f64, f64, f64, f64)) -> ModelCost {
    ModelCost {
        input: JsNumber::from(input),
        output: JsNumber::from(output),
        cache_read: JsNumber::from(cache_read),
        cache_write: JsNumber::from(cache_write),
    }
}

/// List-price estimates for model families the compiled catalog does not carry.
fn family_price(key: &str) -> Option<ModelCost> {
    let starts = |prefixes: &[&str]| prefixes.iter().any(|prefix| key.starts_with(prefix));
    let claude = key.starts_with("claude");
    let table = if starts(&["gemini-4-neon", "gemini-pro-orchestrator-neon"]) {
        (2.5, 15.0, 0.25, 0.0)
    } else if starts(&["gemini-4-argon", "gemini-pro-orchestrator-g1-pro"]) {
        (1.25, 10.0, 0.125, 0.0)
    } else if starts(&["gemini-4-hydrogen"]) {
        (0.5, 3.0, 0.05, 0.0)
    } else if starts(&["gemini-3-8-flash", "gemini-2-5-flash", "gemini-3-flash"]) {
        (0.3, 2.5, 0.075, 0.0)
    } else if starts(&["gemini-2-5-pro", "gemini-3-pro"]) {
        (1.25, 10.0, 0.31, 0.0)
    } else if claude && key.contains("opus") {
        if LEGACY_OPUS_KEYS.contains(&key) {
            LEGACY_OPUS_RATES
        } else {
            return CURRENT_OPUS_KEYS
                .iter()
                .find_map(|opus| catalog_price(opus))
                .or_else(|| Some(rates(LEGACY_OPUS_RATES)));
        }
    } else if claude && key.contains("sonnet") {
        (3.0, 15.0, 0.3, 3.75)
    } else if claude && key.contains("haiku") {
        (1.0, 5.0, 0.1, 1.25)
    } else if starts(&["gpt-5", "codex"]) {
        (2.5, 15.0, 0.25, 0.0)
    } else if starts(&["composer-"]) {
        (1.25, 6.0, 0.125, 0.0)
    } else if starts(&["gpt-oss-120b"]) {
        (0.15, 0.6, 0.0, 0.0)
    } else {
        return None;
    };
    Some(rates(table))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{zero_model_cost, ModelInput};

    fn cli_model(id: &str, cost: ModelCost) -> Model {
        Model {
            id: id.into(),
            name: id.into(),
            api: "test-cli".into(),
            provider: "test-cli".into(),
            base_url: "test://local".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![ModelInput::Text],
            cost,
            context_window: 200_000,
            max_tokens: 32_000,
            max_tokens_explicit: false,
            featured: None,
            headers: None,
            compat: None,
            serving: None,
        }
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Usage {
        Usage {
            input,
            output,
            cache_read,
            cache_write,
            total_tokens: input + output + cache_read + cache_write,
            cost: UsageCost::default(),
        }
    }

    fn assert_close(actual: JsNumber, expected: f64) {
        assert!(
            (actual.as_f64() - expected).abs() < 1e-12,
            "expected {expected}, got {}",
            actual.as_f64()
        );
    }

    fn assert_rates(cost: Option<ModelCost>, expected: (f64, f64, f64, f64)) {
        let cost = cost.expect("list price");
        assert_close(cost.input, expected.0);
        assert_close(cost.output, expected.1);
        assert_close(cost.cache_read, expected.2);
        assert_close(cost.cache_write, expected.3);
    }

    #[test]
    fn normalizes_prefixes_versions_and_dates() {
        assert_eq!(normalize("anthropic/claude-opus-4.6"), "claude-opus-4-6");
        assert_eq!(
            normalize("~anthropic/claude-fable-latest"),
            "claude-fable-latest"
        );
        assert_eq!(
            normalize("jp.anthropic.claude-haiku-4-5-20251001-v1:0"),
            "claude-haiku-4-5"
        );
        assert_eq!(normalize("Claude-Opus-4-6[1m]"), "claude-opus-4-6");
        assert_eq!(normalize("gemini-3.8-flash"), "gemini-3-8-flash");
        assert_eq!(
            strip_mode_suffixes("claude-opus-5-5-extra-high-fast"),
            "claude-opus-5-5"
        );
        assert_eq!(strip_mode_suffixes("gemini-4-neon-xhigh"), "gemini-4-neon");
    }

    #[test]
    fn catalog_prices_win_and_match_dash_versions() {
        assert_rates(
            list_price_for_model("claude-opus-4-6"),
            (5.0, 25.0, 0.5, 6.25),
        );
        assert_rates(
            list_price_for_model("claude-fable-5-high"),
            (10.0, 50.0, 1.0, 12.5),
        );
        assert_rates(list_price_for_model("gpt-5.1"), (1.25, 10.0, 0.125, 0.0));
    }

    #[test]
    fn antigravity_family_rates() {
        assert_rates(
            list_price_for_model("gemini-4-neon"),
            (2.5, 15.0, 0.25, 0.0),
        );
        assert_rates(
            list_price_for_model("gemini-4-neon-xhigh"),
            (2.5, 15.0, 0.25, 0.0),
        );
        assert_rates(
            list_price_for_model("gemini-pro-orchestrator-g1-pro"),
            (1.25, 10.0, 0.125, 0.0),
        );
        assert_rates(
            list_price_for_model("gemini-4-hydrogen-low"),
            (0.5, 3.0, 0.05, 0.0),
        );
    }

    #[test]
    fn claude_family_rates() {
        // Opus 5.5 is not in the compiled catalog; it takes the newest catalog Opus price.
        assert_rates(
            list_price_for_model("claude-opus-5-5"),
            (5.0, 25.0, 0.5, 6.25),
        );
        assert_rates(
            list_price_for_model("claude-opus-5-5-thinking-max"),
            (5.0, 25.0, 0.5, 6.25),
        );
        assert_rates(
            list_price_for_model("claude-opus-4"),
            (15.0, 75.0, 1.5, 18.75),
        );
        assert_rates(
            list_price_for_model("claude-sonnet-5-5"),
            (3.0, 15.0, 0.3, 3.75),
        );
        assert_rates(
            list_price_for_model("claude-haiku-6"),
            (1.0, 5.0, 0.1, 1.25),
        );
    }

    #[test]
    fn cursor_family_rates() {
        assert_rates(
            list_price_for_model("composer-2.5-fast"),
            (1.25, 6.0, 0.125, 0.0),
        );
        assert_rates(
            list_price_for_model("gpt-5-codex-high"),
            (2.5, 15.0, 0.25, 0.0),
        );
        assert!(list_price_for_model("auto").is_none());
        assert!(list_price_for_model("").is_none());
    }

    #[test]
    fn estimates_from_list_price_when_model_cost_is_zero() {
        let model = cli_model("gemini-4-neon", zero_model_cost());
        let mut usage = usage(1_000_000, 100_000, 200_000, 0);
        apply_estimated_cost(&model, None, &mut usage, None);
        assert_close(usage.cost.input, 2.5);
        assert_close(usage.cost.output, 1.5);
        assert_close(usage.cost.cache_read, 0.05);
        assert_close(usage.cost.total, 4.05);
    }

    #[test]
    fn response_model_price_wins_over_model_id_price() {
        let model = cli_model("auto", zero_model_cost());
        let mut usage = usage(1_000_000, 0, 0, 0);
        apply_estimated_cost(&model, Some("composer-2.5"), &mut usage, None);
        assert_close(usage.cost.total, 1.25);
    }

    #[test]
    fn model_cost_wins_when_non_zero() {
        let model = cli_model("gemini-4-neon", rates((1.0, 2.0, 0.0, 0.0)));
        let mut usage = usage(1_000_000, 1_000_000, 0, 0);
        apply_estimated_cost(&model, Some("claude-opus-5-5"), &mut usage, None);
        assert_close(usage.cost.total, 3.0);
    }

    #[test]
    fn reported_total_replaces_estimate_and_keeps_breakdown() {
        let model = cli_model("claude-opus-5-5", zero_model_cost());
        let mut usage = usage(1_000_000, 0, 0, 0);
        apply_estimated_cost(&model, None, &mut usage, Some(0.42));
        assert_close(usage.cost.input, 5.0);
        assert_close(usage.cost.total, 0.42);

        for ignored in [Some(0.0), Some(-1.0), Some(f64::NAN), None] {
            apply_estimated_cost(&model, None, &mut usage, ignored);
            assert_close(usage.cost.total, 5.0);
        }
    }

    #[test]
    fn zero_tokens_cost_nothing() {
        let model = cli_model("claude-opus-5-5", zero_model_cost());
        let mut usage = usage(0, 0, 0, 0);
        apply_estimated_cost(&model, None, &mut usage, None);
        assert_eq!(usage.cost, UsageCost::default());

        let unknown = cli_model("auto", zero_model_cost());
        let mut usage = self::usage(10, 10, 0, 0);
        apply_estimated_cost(&unknown, None, &mut usage, None);
        assert_eq!(usage.cost, UsageCost::default());
    }

    #[test]
    fn reads_reported_cost_keys() {
        let value = serde_json::json!({"total_cost_usd": 0.0, "cost_usd": 0.12});
        assert_eq!(reported_cost_usd(&value), Some(0.12));
        assert_eq!(
            reported_cost_usd(&serde_json::json!({"costUsd": 1.5})),
            Some(1.5)
        );
        assert_eq!(reported_cost_usd(&serde_json::json!({})), None);
    }
}
