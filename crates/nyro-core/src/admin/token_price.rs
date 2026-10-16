//! Provider-scoped manual prices with case-sensitive bounded model inheritance,
//! stored as independent exact-identity settings.
//! No catalogue refresh, routing mutation, or historical billing is involved.

use std::collections::HashMap;

use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};

use super::{
    AdminService,
    model_catalog::{PricingCatalog, pricing_model_prefixes},
    token_cost,
};

const SETTING_PREFIX: &str = "token_price.v1.";
const MAX_RATE: f64 = 1_000_000_000.0;

/// Currency units per one million tokens. Null cache rate uses the input rate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPriceRates {
    pub currency: String,
    pub input_rate: f64,
    pub output_rate: f64,
    pub cache_read_rate: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPriceReference {
    #[serde(flatten)]
    pub rates: TokenPriceRates,
    pub pricing_provider: String,
    pub pricing_model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenPriceConfig {
    pub provider_id: String,
    pub model: String,
    /// Automatic reference only, regardless of any manual rules.
    pub default_price: Option<TokenPriceReference>,
    /// Longest same-provider parent rule, even when an own override is active.
    /// Resetting this exact identity reveals this price, not necessarily default.
    pub inherited_price: Option<TokenPriceReference>,
    /// Only the manual rule stored for this exact provider/model identity.
    pub override_price: Option<TokenPriceRates>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenPriceInput {
    pub provider_id: String,
    pub model: String,
    // Require an explicit value (including null); missing price must not reset.
    #[serde(deserialize_with = "deserialize_price")]
    pub price: Option<TokenPriceRates>,
}

fn deserialize_price<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<TokenPriceRates>, D::Error> {
    Option::<TokenPriceRates>::deserialize(deserializer)
}

#[derive(Debug, thiserror::Error)]
pub enum TokenPriceError {
    #[error("{0}")]
    InvalidInput(String),
    #[error("Invalid stored token price: {0}")]
    InvalidStored(String),
}

fn validate_identity(provider_id: &str, model: &str) -> Result<(), TokenPriceError> {
    for (field, value) in [("provider_id", provider_id), ("model", model)] {
        if value.trim().is_empty() {
            return Err(TokenPriceError::InvalidInput(format!(
                "{field} cannot be blank"
            )));
        }
    }
    // Deliberately do not trim or normalize: whitespace, case, Unicode, and
    // delimiters are part of the exact logged identity.
    Ok(())
}

fn validate_rates(price: &TokenPriceRates) -> Result<(), TokenPriceError> {
    if !matches!(price.currency.as_str(), "USD" | "CNY") {
        return Err(TokenPriceError::InvalidInput(
            "currency must be USD or CNY".into(),
        ));
    }
    for (field, rate) in [
        ("input_rate", Some(price.input_rate)),
        ("output_rate", Some(price.output_rate)),
        ("cache_read_rate", price.cache_read_rate),
    ] {
        if rate.is_some_and(|rate| !rate.is_finite() || !(0.0..=MAX_RATE).contains(&rate)) {
            return Err(TokenPriceError::InvalidInput(format!(
                "{field} must be finite and between 0 and 1000000000"
            )));
        }
    }
    Ok(())
}

fn validate_input(input: &TokenPriceInput) -> Result<(), TokenPriceError> {
    validate_identity(&input.provider_id, &input.model)?;
    if let Some(price) = &input.price {
        validate_rates(price)?;
    }
    Ok(())
}

fn setting_key(provider_id: &str, model: &str) -> String {
    // Length-framed UTF-8 prevents ambiguous delimiter concatenation. SHA-256
    // makes the key fixed-length and safe even for case-insensitive DB keys.
    let mut hash = Sha256::new();
    for part in [provider_id, model] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    format!("{SETTING_PREFIX}{}", hex::encode(hash.finalize()))
}

fn is_price_setting(key: &str) -> bool {
    // MySQL settings keys may use a case-insensitive collation. Reserve every
    // ASCII case variant, but accept only the canonical lower-case key.
    key.get(..SETTING_PREFIX.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(SETTING_PREFIX))
}

fn parse_setting(key: &str, value: &str) -> Result<TokenPriceInput, TokenPriceError> {
    let invalid = |reason: String| TokenPriceError::InvalidStored(format!("{key}: {reason}"));
    let input: TokenPriceInput = serde_json::from_str(value).map_err(|e| invalid(e.to_string()))?;
    validate_input(&input).map_err(|e| invalid(e.to_string()))?;
    if key != setting_key(&input.provider_id, &input.model) {
        return Err(invalid(
            "setting key does not match the exact identity".into(),
        ));
    }
    Ok(input)
}

/// Generic settings writes and imports must not bypass price validation.
/// Unknown unrelated settings are intentionally left alone.
pub(super) fn validate_setting(key: &str, value: &str) -> anyhow::Result<()> {
    if is_price_setting(key) {
        parse_setting(key, value).map_err(|e| TokenPriceError::InvalidInput(e.to_string()))?;
    }
    Ok(())
}

/// Remap only rules whose source provider was successfully recreated. Skip
/// conflicted/failed providers entirely. Preserve deleted/unknown identities,
/// except never let an unmapped legacy rule overwrite a live provider's price.
pub(super) fn remap_import_setting(
    key: &str,
    value: &str,
    provider_ids: &HashMap<String, Option<String>>,
    current_provider_ids: &std::collections::HashSet<String>,
) -> anyhow::Result<Option<(String, String)>> {
    if !is_price_setting(key) {
        return Ok(Some((key.into(), value.into())));
    }
    let mut input =
        parse_setting(key, value).map_err(|e| TokenPriceError::InvalidInput(e.to_string()))?;
    match provider_ids.get(&input.provider_id) {
        Some(Some(destination)) => input.provider_id = destination.clone(),
        Some(None) => return Ok(None),
        None if current_provider_ids.contains(&input.provider_id) => return Ok(None),
        None => {}
    }
    Ok(Some((
        setting_key(&input.provider_id, &input.model),
        serde_json::to_string(&input)?,
    )))
}

/// One settings snapshot per statistics request, never a lookup per day/model.
/// Fail closed on corrupt reserved data; never turn it into zero or automatic.
pub(super) fn parse_overrides(
    settings: Vec<(String, String)>,
) -> anyhow::Result<HashMap<(String, String), TokenPriceRates>> {
    let mut overrides = HashMap::new();
    for (key, value) in settings {
        if is_price_setting(&key) {
            let input = parse_setting(&key, &value)?;
            if let Some(price) = input.price {
                overrides.insert((input.provider_id, input.model), price);
            }
        }
    }
    Ok(overrides)
}

/// Shared by cost enrichment and the editor. Stored keys remain exact and
/// independent; only reads inherit, never normalize case or cross providers.
/// `parent_only` skips the exact identity even when it has its own override.
pub(super) fn resolve_manual_price<'a>(
    overrides: &'a HashMap<(String, String), TokenPriceRates>,
    provider_id: &str,
    model: &str,
    parent_only: bool,
) -> Option<(&'a str, &'a TokenPriceRates)> {
    pricing_model_prefixes(model)
        .skip(usize::from(parent_only))
        .find_map(|base| {
            overrides
                .get_key_value(&(provider_id.into(), base.into()))
                .map(|((_, matched_model), rates)| (matched_model.as_str(), rates))
        })
}

impl AdminService {
    async fn default_token_price(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<Option<TokenPriceReference>> {
        let Some(provider) = self.gw.storage.providers().get(provider_id).await? else {
            return Ok(None);
        };
        let catalog = PricingCatalog::load(&self.gw.config.data_dir).ok();
        Ok(
            token_cost::resolve_pricing(catalog.as_ref(), &provider, model)
                .map(|pricing| pricing.reference()),
        )
    }

    pub async fn get_token_price(
        &self,
        provider_id: &str,
        model: &str,
    ) -> anyhow::Result<TokenPriceConfig> {
        validate_identity(provider_id, model)?;
        // One validated snapshot, just like statistics, including historical
        // provider IDs. The automatic reference never incorporates manual rules.
        let overrides = parse_overrides(self.gw.storage.settings().list_all().await?)?;
        let override_price = overrides.get(&(provider_id.into(), model.into())).cloned();
        let inherited_price =
            resolve_manual_price(&overrides, provider_id, model, true).map(|(base, rates)| {
                TokenPriceReference {
                    rates: rates.clone(),
                    pricing_provider: "manual".into(),
                    pricing_model: base.into(),
                }
            });
        Ok(TokenPriceConfig {
            provider_id: provider_id.into(),
            model: model.into(),
            default_price: self.default_token_price(provider_id, model).await?,
            inherited_price,
            override_price,
        })
    }

    pub async fn set_token_price(
        &self,
        input: TokenPriceInput,
    ) -> anyhow::Result<TokenPriceConfig> {
        validate_input(&input)?;
        // SettingsStore has no delete operation. An explicit null tombstone is
        // an idempotent reset, and allows repair even if the previous value was
        // corrupt. Each pair is a single atomic upsert, not a shared-list RMW.
        let key = setting_key(&input.provider_id, &input.model);
        self.gw
            .storage
            .settings()
            .set(&key, &serde_json::to_string(&input)?)
            .await?;
        // Reuse GET after the atomic write so reset truthfully exposes any
        // inherited rule. No settings-list read/modify/write or lost updates.
        self.get_token_price(&input.provider_id, &input.model).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_price_keys_are_bounded_framed_and_case_sensitive() {
        let pairs = [
            ("a/b", "c"),
            ("a", "b/c"),
            ("P", "M"),
            ("p", "M"),
            ("P", "m"),
            ("p:+# /模型\0", "m:+# /模型\0"),
        ];
        let keys: std::collections::HashSet<_> =
            pairs.into_iter().map(|(p, m)| setting_key(p, m)).collect();
        assert_eq!(keys.len(), pairs.len());
        assert!(
            keys.iter()
                .all(|key| key.len() == SETTING_PREFIX.len() + 64)
        );
        assert_eq!(
            setting_key(&"p".repeat(10_000), "m").len(),
            SETTING_PREFIX.len() + 64
        );
        assert_eq!(setting_key("p", "m"), setting_key("p", "m"));
    }

    #[test]
    fn token_price_rejects_nonfinite_and_out_of_bounds_rust_values() {
        for invalid in [
            f64::NAN,
            f64::INFINITY,
            f64::NEG_INFINITY,
            -1.0,
            MAX_RATE + 1.0,
        ] {
            for field in 0..3 {
                let mut rates = TokenPriceRates {
                    currency: "USD".into(),
                    input_rate: 0.0,
                    output_rate: MAX_RATE,
                    cache_read_rate: None,
                };
                match field {
                    0 => rates.input_rate = invalid,
                    1 => rates.output_rate = invalid,
                    _ => rates.cache_read_rate = Some(invalid),
                }
                assert!(validate_rates(&rates).is_err());
            }
        }
    }
}
