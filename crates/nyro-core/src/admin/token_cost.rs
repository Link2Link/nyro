//! Current API-equivalent value of retained token usage, not historical billing.
//! Only local catalogue data is used; never fetch capabilities per model/day.

use std::collections::HashMap;
use std::path::Path;

use crate::db::models::{ModelCapabilities, Provider, TokenUsageCost, TokenUsageDay};
use crate::provider::metadata::CapabilitiesSource;

use super::model_catalog::{
    PricingCatalog, bigmodel_cn_pricing, is_bigmodel_cn_url, is_subscription_pricing_catalog,
    preset_capabilities_source,
};

use super::token_price::{TokenPriceRates, TokenPriceReference, resolve_manual_price};

#[derive(Clone)]
pub(super) struct Pricing {
    is_manual: bool,
    currency: String,
    provider: String,
    model: String,
    input: f64,
    output: f64,
    cache_read: Option<f64>,
}

impl Pricing {
    pub(super) fn reference(&self) -> TokenPriceReference {
        TokenPriceReference {
            rates: TokenPriceRates {
                currency: self.currency.clone(),
                input_rate: self.input,
                output_rate: self.output,
                cache_read_rate: self.cache_read,
            },
            pricing_provider: self.provider.clone(),
            pricing_model: self.model.clone(),
        }
    }

    fn from_manual(rates: &TokenPriceRates, model: &str) -> Self {
        // Only validated stored overrides reach this constructor.
        Self {
            is_manual: true,
            currency: rates.currency.clone(),
            provider: "manual".into(),
            model: model.into(),
            input: rates.input_rate,
            output: rates.output_rate,
            cache_read: rates.cache_read_rate,
        }
    }

    fn from_capability(cap: &ModelCapabilities) -> Option<Self> {
        let pricing = Self {
            is_manual: false,
            currency: cap.currency.clone().unwrap_or_else(|| "USD".into()),
            provider: cap.provider.clone(),
            model: cap.model_id.clone(),
            input: cap.input_cost?,
            output: cap.output_cost?,
            cache_read: cap.cache_read_cost,
        };
        let valid = |rate: f64| rate.is_finite() && rate >= 0.0;
        (valid(pricing.input) && valid(pricing.output) && pricing.cache_read.is_none_or(valid))
            .then_some(pricing)
    }

    fn cost(&self, row: &TokenUsageDay) -> Option<TokenUsageCost> {
        let input = row.input_tokens.max(0);
        let cached = row.cache_read_tokens.clamp(0, input);
        let cache_rate = self.cache_read.unwrap_or(self.input);
        // Gross input already includes cached tokens. Cache-write premiums are
        // unmodeled because the retained usage has no separate write count.
        let input_cost = (input - cached) as f64 / 1_000_000.0 * self.input;
        let cache_read_cost = cached as f64 / 1_000_000.0 * cache_rate;
        let output_cost = row.output_tokens.max(0) as f64 / 1_000_000.0 * self.output;
        let total_cost = input_cost + cache_read_cost + output_cost;
        // Finite rates can still overflow when multiplied by large token totals.
        if !total_cost.is_finite() {
            return None;
        }
        Some(TokenUsageCost {
            is_manual: self.is_manual,
            currency: self.currency.clone(),
            input_cost,
            cache_read_cost,
            output_cost,
            total_cost,
            cache_rate_fallback: cached > 0 && self.cache_read.is_none(),
            pricing_provider: self.provider.clone(),
            pricing_model: self.model.clone(),
            input_rate: self.input,
            output_rate: self.output,
            cache_read_rate: cache_rate,
        })
    }
}

/// Strong family/namespace hints select a vendor, never a price. That vendor's
/// catalogue must contain the exact model or a bounded base prefix (no substring
/// search, fuzzy aliases, or selection of a random reseller's rate).
fn official_model(model: &str) -> Option<(&'static str, &str)> {
    let lower = model.to_ascii_lowercase();
    for (namespace, vendor) in [
        ("openai/", "openai"),
        ("anthropic/", "anthropic"),
        ("google/", "google"),
        ("xai/", "xai"),
        ("deepseek/", "deepseek"),
        ("moonshotai/", "moonshotai"),
        ("minimax/", "minimax"),
    ] {
        if lower.starts_with(namespace) {
            return Some((vendor, &model[namespace.len()..]));
        }
    }
    if lower.starts_with("gpt-")
        || lower.starts_with("chatgpt-")
        || ["o1", "o3", "o4"].iter().any(|family| {
            lower == *family
                || lower
                    .strip_prefix(family)
                    .is_some_and(|s| s.starts_with('-'))
        })
    {
        Some(("openai", model))
    } else if lower.starts_with("claude-") {
        Some(("anthropic", model))
    } else if lower.starts_with("gemini-") {
        Some(("google", model))
    } else if lower.starts_with("grok-") {
        Some(("xai", model))
    } else if lower.starts_with("deepseek-") {
        Some(("deepseek", model))
    } else if lower.starts_with("kimi-") {
        Some(("moonshotai", model))
    } else if lower.starts_with("minimax-") {
        Some(("minimax", model))
    } else if lower.starts_with("glm-") {
        Some(("zai", model))
    } else if lower.starts_with("qwen") {
        Some(("alibaba", model))
    } else {
        None
    }
}

pub(super) fn resolve_pricing(
    catalog: Option<&PricingCatalog>,
    provider: &Provider,
    model: &str,
) -> Option<Pricing> {
    let model = model.trim();
    if model.is_empty() {
        return None;
    }
    // The two registered native CNY prices remain available even if the
    // models.dev snapshot predates them. Spoof URLs cannot activate this table.
    if let Some((id, input, cache_read, output)) = bigmodel_cn_pricing(&provider.base_url, model) {
        return Some(Pricing {
            is_manual: false,
            currency: "CNY".into(),
            provider: "zhipuai".into(),
            model: id.into(),
            input,
            output,
            cache_read: Some(cache_read),
        });
    }
    // models.dev's bigmodel entries are USD conversions, not native CNY
    // prices. Without a registered native base, leave official bigmodel
    // unpriced rather than labeling a converted reference amount as native.
    if is_bigmodel_cn_url(&provider.base_url) {
        return None;
    }
    let catalog = catalog?;
    let preset_vendor = match preset_capabilities_source(provider) {
        CapabilitiesSource::ModelsDev(vendor) if !vendor.is_empty() => Some(vendor),
        _ => None,
    };
    for vendor in [
        preset_vendor,
        provider.vendor.as_deref(),
        provider.preset_key.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        if is_subscription_pricing_catalog(vendor) {
            continue;
        }
        if let Some(cap) = catalog.resolve(model, Some(vendor)).ok()? {
            // A matching but invalid price must not fall through to a cheaper
            // or otherwise unrelated provider's entry.
            return Pricing::from_capability(cap);
        }
    }
    if let Some((vendor, official_id)) = official_model(model) {
        return catalog
            .resolve(official_id, Some(vendor))
            .ok()?
            .and_then(Pricing::from_capability);
    }
    catalog
        .resolve(model, None)
        .ok()?
        .and_then(Pricing::from_capability)
}

/// Batch enrichment; a missing/bad catalogue cannot fail token statistics.
/// Automatic pricing requires current provider metadata. An explicit manual
/// rule can price retained usage even after the provider has been deleted.
pub(super) fn enrich_token_costs(
    rows: &mut [TokenUsageDay],
    providers: &HashMap<String, Provider>,
    data_dir: &Path,
    overrides: &HashMap<(String, String), TokenPriceRates>,
) {
    if rows.is_empty() {
        return;
    }
    let catalog = PricingCatalog::load(data_dir).ok();
    let mut memo: HashMap<(String, String), Option<Pricing>> = HashMap::new();
    for row in rows {
        let pricing = memo
            .entry((row.provider_id.clone(), row.model.clone()))
            .or_insert_with(|| {
                if let Some((base, rates)) =
                    resolve_manual_price(overrides, &row.provider_id, &row.model, false)
                {
                    return Some(Pricing::from_manual(rates, base));
                }
                let provider = providers.get(&row.provider_id)?;
                resolve_pricing(catalog.as_ref(), provider, &row.model)
            });
        // Missing usage remains visible via missing_usage_count. Like token
        // totals, the estimate includes only known partial values in that row.
        row.cost = pricing.as_ref().and_then(|pricing| pricing.cost(row));
    }
}

#[cfg(test)]
mod tests;
