use super::*;
use serde_json::json;

fn row(input: i64, cached: i64, output: i64) -> TokenUsageDay {
    TokenUsageDay {
        date: "2024-02-29".into(),
        provider_id: "provider".into(),
        provider_name: "Provider".into(),
        model: "test-model".into(),
        request_count: 1,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cached,
        missing_usage_count: 0,
        cache_read_reported_count: 1,
        cost: None,
    }
}

fn capability() -> ModelCapabilities {
    ModelCapabilities {
        provider: "vendor".into(),
        model_id: "test-model".into(),
        context_window: 128_000,
        embedding_length: None,
        output_max_tokens: None,
        tool_call: false,
        reasoning: false,
        input_modalities: vec![],
        output_modalities: vec![],
        input_cost: Some(2.0),
        output_cost: Some(8.0),
        cache_read_cost: Some(0.5),
        currency: None,
    }
}

fn provider(vendor: Option<&str>) -> Provider {
    serde_json::from_value(json!({
        "id": "provider", "name": "Provider", "vendor": vendor,
        "protocol": "openai-compatible", "base_url": "https://proxy.example/v1",
        "api_key": "", "is_enabled": true, "created_at": "", "updated_at": ""
    }))
    .unwrap()
}

fn catalog(data: serde_json::Value) -> (tempfile::TempDir, PricingCatalog) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("models.dev.json"), data.to_string()).unwrap();
    let catalog = PricingCatalog::load(dir.path()).unwrap();
    (dir, catalog)
}

fn assert_close(actual: f64, expected: f64) {
    assert!((actual - expected).abs() < 1e-12, "{actual} != {expected}");
}

#[test]
fn gross_input_excludes_cache_reads_once_and_exposes_rates() {
    let pricing = Pricing::from_capability(&capability()).unwrap();
    let cost = pricing.cost(&row(1_000_000, 250_000, 500_000)).unwrap();
    assert_eq!(cost.currency, "USD");
    assert_eq!(cost.input_cost, 1.5);
    assert_eq!(cost.cache_read_cost, 0.125);
    assert_eq!(cost.output_cost, 4.0);
    assert_eq!(cost.total_cost, 5.625);
    assert_eq!(cost.input_rate, 2.0);
    assert_eq!(cost.output_rate, 8.0);
    assert_eq!(cost.cache_read_rate, 0.5);
    assert_eq!(cost.pricing_provider, "vendor");
    assert_eq!(cost.pricing_model, "test-model");
    assert!(!cost.cache_rate_fallback);
}

#[test]
fn cache_reads_are_clamped_to_gross_input_and_nonnegative() {
    let pricing = Pricing::from_capability(&capability()).unwrap();
    let exceeded = pricing.cost(&row(100, 200, 0)).unwrap();
    assert_eq!(exceeded.input_cost, 0.0);
    assert_close(exceeded.total_cost, 0.00005);
    let negative = pricing.cost(&row(100, -200, 0)).unwrap();
    assert_eq!(negative.cache_read_cost, 0.0);
    assert_close(negative.total_cost, 0.0002);
    assert_eq!(pricing.cost(&row(-10, 100, -10)).unwrap().total_cost, 0.0);
}

#[test]
fn missing_cache_rate_uses_input_rate_only_for_effective_positive_cache() {
    let mut cap = capability();
    cap.cache_read_cost = None;
    let pricing = Pricing::from_capability(&cap).unwrap();
    let cost = pricing.cost(&row(1_000_000, 250_000, 0)).unwrap();
    assert_eq!(cost.input_cost, 1.5);
    assert_eq!(cost.cache_read_cost, 0.5);
    assert_eq!(cost.total_cost, 2.0);
    assert_eq!(cost.cache_read_rate, 2.0);
    assert!(cost.cache_rate_fallback);
    for usage in [row(100, 0, 0), row(100, -10, 0), row(0, 10, 0)] {
        assert!(!pricing.cost(&usage).unwrap().cache_rate_fallback);
    }
    cap.cache_read_cost = Some(0.0);
    let cost = Pricing::from_capability(&cap)
        .unwrap()
        .cost(&row(1_000_000, 1_000_000, 0))
        .unwrap();
    assert_eq!(cost.total_cost, 0.0);
    assert_eq!(cost.cache_read_rate, 0.0);
    assert!(!cost.cache_rate_fallback);
}

#[test]
fn zero_rates_are_real_prices_but_missing_or_invalid_rates_are_not() {
    let mut cap = capability();
    cap.input_cost = Some(0.0);
    cap.output_cost = Some(0.0);
    cap.cache_read_cost = Some(0.0);
    assert_eq!(
        Pricing::from_capability(&cap)
            .unwrap()
            .cost(&row(100, 20, 40))
            .unwrap()
            .total_cost,
        0.0
    );
    for invalid in [
        None,
        Some(-1.0),
        Some(f64::NAN),
        Some(f64::INFINITY),
        Some(f64::NEG_INFINITY),
    ] {
        for output in [false, true] {
            let mut cap = capability();
            if output {
                cap.output_cost = invalid;
            } else {
                cap.input_cost = invalid;
            }
            assert!(Pricing::from_capability(&cap).is_none());
        }
    }
    for invalid in [-1.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut cap = capability();
        cap.cache_read_cost = Some(invalid);
        assert!(Pricing::from_capability(&cap).is_none());
    }
    let mut cap = capability();
    cap.input_cost = Some(f64::MAX);
    assert!(
        Pricing::from_capability(&cap)
            .unwrap()
            .cost(&row(i64::MAX, 0, 0))
            .is_none()
    );
}

#[test]
fn exact_resolution_prefers_preset_then_vendor_and_rejects_ambiguity() {
    let (_dir, catalog) = catalog(json!({
        "one": {"models": {"shared": {"id": "shared", "cost": {"input": 1, "output": 2}}}},
        "two": {"models": {"shared": {"id": "shared", "cost": {"input": 3, "output": 4}}}},
        "openai": {"models": {"shared": {"id": "shared", "cost": {"input": 5, "output": 6}}}},
        "unique": {"models": {"unique-model": {"id": "unique-model", "cost": {"input": 7, "output": 8}}}}
    }));
    assert!(resolve_pricing(Some(&catalog), &provider(None), "shared").is_none());
    assert_eq!(
        resolve_pricing(Some(&catalog), &provider(Some("two")), "shared")
            .unwrap()
            .input,
        3.0
    );
    let mut p = provider(Some("two"));
    p.preset_key = Some("openai".into());
    assert_eq!(
        resolve_pricing(Some(&catalog), &p, "shared")
            .unwrap()
            .provider,
        "openai"
    );
    assert_eq!(
        resolve_pricing(Some(&catalog), &p, "UNIQUE-MODEL")
            .unwrap()
            .model,
        "unique-model"
    );
    for (variant, base) in [
        ("unique-model-latest", "unique-model"),
        ("shared-suffix", "shared"),
    ] {
        assert_eq!(
            resolve_pricing(Some(&catalog), &p, variant).unwrap().model,
            base
        );
    }
    for unknown in ["", "unique", "unique-modelish", "sharedsuffix", "model"] {
        assert!(
            resolve_pricing(Some(&catalog), &p, unknown).is_none(),
            "{unknown}"
        );
    }
}

#[test]
fn popular_models_through_proxies_and_oauth_require_official_base_match() {
    let (_dir, catalog) = catalog(json!({
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 2, "output": 8}}}},
        "anthropic": {"models": {"claude-known": {"id": "claude-known", "cost": {"input": 3, "output": 15}}}},
        "reseller": {"models": {
            "gpt-known": {"id": "gpt-known", "cost": {"input": 99, "output": 99}},
            "claude-known": {"id": "claude-known", "cost": {"input": 99, "output": 99}},
            "gpt-reseller-only": {"id": "gpt-reseller-only", "cost": {"input": 99, "output": 99}}
        }}
    }));
    let mut p = provider(None);
    for (model, vendor) in [
        ("gpt-known", "openai"),
        ("claude-known", "anthropic"),
        ("openai/gpt-known", "openai"),
        ("anthropic/claude-known", "anthropic"),
        ("ANTHROPIC/CLAUDE-KNOWN-medium", "anthropic"),
        ("gpt-known:thinking", "openai"),
    ] {
        assert_eq!(
            resolve_pricing(Some(&catalog), &p, model).unwrap().provider,
            vendor
        );
    }
    assert!(resolve_pricing(Some(&catalog), &p, "gpt-reseller-only").is_none());
    p.vendor = Some("openai".into());
    p.preset_key = Some("openai".into());
    p.channel = Some("codex".into());
    p.auth_mode = "oauth".into();
    assert_eq!(
        resolve_pricing(Some(&catalog), &p, "gpt-known")
            .unwrap()
            .provider,
        "openai"
    );
    // Same-named models share the official API price on every provider; a
    // reseller's own catalogue rate never overrides it.
    p = provider(Some("reseller"));
    for model in ["gpt-known", "claude-known"] {
        let price = resolve_pricing(Some(&catalog), &p, model).unwrap();
        assert_ne!(price.input, 99.0, "{model}");
        assert_ne!(price.provider, "reseller", "{model}");
    }
    assert_eq!(
        resolve_pricing(Some(&catalog), &p, "gpt-known")
            .unwrap()
            .input,
        2.0
    );
}

#[test]
fn subscription_zero_placeholders_use_api_base_references_or_remain_unpriced() {
    let (_dir, catalog) = catalog(json!({
        "alibaba-coding-plan": {"models": {
            "glm-5": {"id": "glm-5", "cost": {"input": 0, "output": 0}},
            "plan-only": {"id": "plan-only", "cost": {"input": 0, "output": 0}}
        }},
        "kimi-for-coding": {"models": {
            "kimi-k2.5": {"id": "kimi-k2.5", "cost": {"input": 0, "output": 0}},
            "kimi-for-coding": {"id": "kimi-for-coding", "cost": {"input": 0, "output": 0}}
        }},
        "opencode-go": {"models": {"glm-5": {"id": "glm-5", "cost": {"input": 0, "output": 0}}}},
        "github-copilot": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 0, "output": 0}}}},
        "zai": {"models": {"glm-5": {"id": "glm-5", "cost": {"input": 1, "output": 3}}}},
        "moonshotai": {"models": {"kimi-k2.5": {"id": "kimi-k2.5", "cost": {"input": 2, "output": 4}}}},
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 3, "output": 6}}}},
        "free-api": {"models": {"free-model": {"id": "free-model", "cost": {"input": 0, "output": 0}}}}
    }));
    let mut bailian = provider(Some("bailian"));
    bailian.preset_key = Some("bailian".into());
    bailian.channel = Some("coding".into());
    assert_eq!(
        resolve_pricing(Some(&catalog), &bailian, "glm-5")
            .unwrap()
            .input,
        1.0
    );
    let mut kimi = provider(Some("kimi-code"));
    kimi.preset_key = Some("kimi-code".into());
    assert_eq!(
        resolve_pricing(Some(&catalog), &kimi, "kimi-k2.5")
            .unwrap()
            .input,
        2.0
    );
    assert!(resolve_pricing(Some(&catalog), &kimi, "kimi-for-coding").is_none());
    for vendor in [None, Some("alibaba-coding-plan"), Some("opencode-go")] {
        for model in ["plan-only", "plan-only-medium"] {
            assert!(resolve_pricing(Some(&catalog), &provider(vendor), model).is_none());
        }
    }
    assert_eq!(
        resolve_pricing(Some(&catalog), &bailian, "glm-5-medium")
            .unwrap()
            .input,
        1.0
    );
    assert_eq!(
        resolve_pricing(
            Some(&catalog),
            &provider(Some("github-copilot")),
            "gpt-known"
        )
        .unwrap()
        .input,
        3.0
    );
    // Genuine free API rates are still accepted; only plan catalogues are excluded.
    assert_eq!(
        resolve_pricing(Some(&catalog), &provider(Some("free-api")), "free-model")
            .unwrap()
            .input,
        0.0
    );
}

#[test]
fn invalid_reseller_rates_are_ignored_in_favor_of_official_price() {
    let (_dir, catalog) = catalog(json!({
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 2, "output": 8}}}},
        "reseller": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": -1, "output": 4}}}}
    }));
    let price = resolve_pricing(Some(&catalog), &provider(Some("reseller")), "gpt-known").unwrap();
    assert_eq!((price.provider.as_str(), price.input), ("openai", 2.0));
}

#[test]
fn invalid_official_rates_do_not_fall_through_to_reseller() {
    let (_dir, catalog) = catalog(json!({
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": -1, "output": 8}}}},
        "reseller": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 3, "output": 4}}}}
    }));
    assert!(resolve_pricing(Some(&catalog), &provider(Some("reseller")), "gpt-known").is_none());
}

#[test]
fn reseller_only_model_with_official_family_hint_stays_unpriced_even_for_that_reseller() {
    let (_dir, catalog) = catalog(json!({
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 2, "output": 8}}}},
        "reseller": {"models": {"gpt-reseller-only": {"id": "gpt-reseller-only", "cost": {"input": 3, "output": 4}}}}
    }));
    // Not in the official catalogue: fall back to the provider's own entry.
    let price = resolve_pricing(
        Some(&catalog),
        &provider(Some("reseller")),
        "gpt-reseller-only",
    )
    .unwrap();
    assert_eq!((price.provider.as_str(), price.input), ("reseller", 3.0));
    // But a provider without that vendor still cannot guess a reseller rate.
    assert!(resolve_pricing(Some(&catalog), &provider(None), "gpt-reseller-only").is_none());
}

#[test]
fn case_collisions_and_inconsistent_catalog_ids_are_unpriced() {
    let (_dir, catalog) = catalog(json!({
        "one": {"models": {
            "model": {"id": "model", "cost": {"input": 1, "output": 2}},
            "MODEL": {"id": "MODEL", "cost": {"input": 3, "output": 4}},
            "wrong-id": {"id": "another-id", "cost": {"input": 3, "output": 4}}
        }}
    }));
    for model in [
        "model",
        "MODEL",
        "model-medium",
        "MODEL/variant",
        "wrong-id",
        "another-id",
    ] {
        assert!(resolve_pricing(Some(&catalog), &provider(Some("one")), model).is_none());
    }
}

#[test]
fn ambiguous_reseller_entries_do_not_affect_official_price() {
    let (_dir, catalog) = catalog(json!({
        "reseller": {"models": {
            "gpt-known": {"id": "gpt-known", "cost": {"input": 1, "output": 2}},
            "GPT-KNOWN": {"id": "GPT-KNOWN", "cost": {"input": 3, "output": 4}}
        }},
        "openai": {"models": {"gpt-known": {"id": "gpt-known", "cost": {"input": 5, "output": 6}}}}
    }));
    let price = resolve_pricing(Some(&catalog), &provider(Some("reseller")), "gpt-known").unwrap();
    assert_eq!((price.provider.as_str(), price.input), ("openai", 5.0));
}

#[test]
fn native_bigmodel_is_cny_exact_then_longest_and_does_not_leak_to_zai_or_spoof_urls() {
    let (_dir, catalog) = catalog(json!({
        "zhipuai": {"models": {"glm-5.3": {"id": "glm-5.3", "cost": {"input": 1.4, "output": 4.4}}}},
        "zai": {"models": {"glm-5.3": {"id": "glm-5.3", "cost": {"input": 1.5, "output": 5}}}}
    }));
    let mut p = provider(Some("zhipuai"));
    p.base_url = "https://open.bigmodel.cn/api/paas/v4".into();
    let pricing = resolve_pricing(None, &p, "GLM-5.3").unwrap();
    let cost = pricing.cost(&row(1_000_000, 500_000, 1_000_000)).unwrap();
    assert_eq!(cost.currency, "CNY");
    assert_eq!(cost.pricing_provider, "zhipuai");
    assert_eq!(cost.pricing_model, "glm-5.3");
    assert_close(cost.total_cost, 33.15);
    let flash = resolve_pricing(None, &p, "glm-5.3-flash").unwrap();
    assert_eq!(
        (flash.input, flash.output, flash.cache_read),
        (0.8, 2.8, Some(0.23))
    );
    for suffix in ["-medium", "_medium", ":medium", "/medium"] {
        for (base, input) in [("glm-5.3", 8.0), ("glm-5.3-flash", 0.8)] {
            let price =
                resolve_pricing(None, &p, &format!("{base}{suffix}").to_ascii_uppercase()).unwrap();
            assert_eq!(price.input, input);
            assert_eq!(price.model, base);
        }
    }
    for model in ["glm-5.30", "glm-5.3.1", "glm-5.3flash", "glm-5.3+medium"] {
        assert!(
            resolve_pricing(Some(&catalog), &p, model).is_none(),
            "{model}"
        );
    }
    for url in [
        "https://bigmodel.cn.evil.example/v1",
        "https://notbigmodel.cn/v1",
        "https://bigmodel.cn@evil.example/v1",
        "https://evil.example/bigmodel.cn",
        "https://evil.example/?upstream=bigmodel.cn",
        "not a URL bigmodel.cn",
    ] {
        p.base_url = url.into();
        // Not a real bigmodel.cn base: no native CNY table. The glm family
        // maps to its official vendor (zai), regardless of provider vendor.
        let price = resolve_pricing(Some(&catalog), &p, "glm-5.3").unwrap();
        assert_eq!(price.currency, "USD", "{url}");
        assert_eq!(price.provider, "zai", "{url}");
        assert_eq!(price.input, 1.5);
    }
    p.vendor = Some("zai".into());
    p.base_url = "https://api.z.ai/api/paas/v4".into();
    let price = resolve_pricing(Some(&catalog), &p, "glm-5.3").unwrap();
    assert_eq!(price.currency, "USD");
    assert_eq!(price.input, 1.5);
}

#[test]
fn official_bigmodel_without_native_rate_does_not_use_converted_usd_catalogue() {
    let (_dir, catalog) = catalog(json!({
        "zhipuai": {"models": {"glm-4.7": {"id": "glm-4.7", "cost": {"input": 0.6, "output": 2.2}}}},
        "zai": {"models": {"glm-4.7": {"id": "glm-4.7", "cost": {"input": 0.6, "output": 2.2}}}}
    }));
    let mut p = provider(Some("zhipuai"));
    p.base_url = "https://open.bigmodel.cn/api/paas/v4".into();
    assert!(resolve_pricing(Some(&catalog), &p, "glm-4.7").is_none());
    p.vendor = Some("zai".into());
    p.base_url = "https://api.z.ai/api/paas/v4".into();
    assert_eq!(
        resolve_pricing(Some(&catalog), &p, "glm-4.7")
            .unwrap()
            .currency,
        "USD"
    );
}

#[test]
fn batch_prices_each_day_and_partial_values_but_not_deleted_providers() {
    let (dir, _) = catalog(json!({
        "vendor": {"models": {"test-model": {"id": "test-model", "cost": {"input": 2, "output": 8}}}}
    }));
    let mut rows = vec![
        row(1_000_000, 250_000, 0),
        row(500_000, 0, 0),
        row(100, 0, 0),
    ];
    rows[1].date = "2024-03-01".into();
    rows[1].missing_usage_count = 1;
    rows[2].provider_id = "deleted".into();
    let providers = HashMap::from([("provider".into(), provider(Some("vendor")))]);
    enrich_token_costs(&mut rows, &providers, dir.path(), &HashMap::new());
    assert_eq!(rows[0].cost.as_ref().unwrap().total_cost, 2.0);
    assert!(rows[0].cost.as_ref().unwrap().cache_rate_fallback);
    assert_eq!(rows[1].cost.as_ref().unwrap().total_cost, 1.0);
    assert_eq!(rows[1].missing_usage_count, 1);
    assert!(rows[2].cost.is_none());
}

#[test]
fn missing_or_bad_catalogue_preserves_tokens_and_unmatched_rows() {
    let dir = tempfile::tempdir().unwrap();
    let providers = HashMap::from([("provider".into(), provider(None))]);
    for raw in [None, Some("not JSON"), Some("[]"), Some("{}")].into_iter() {
        if let Some(raw) = raw {
            std::fs::write(dir.path().join("models.dev.json"), raw).unwrap();
        }
        let mut rows = vec![row(100, 40, 20)];
        enrich_token_costs(&mut rows, &providers, dir.path(), &HashMap::new());
        assert!(rows[0].cost.is_none());
        assert_eq!(
            (
                rows[0].input_tokens,
                rows[0].cache_read_tokens,
                rows[0].output_tokens
            ),
            (100, 40, 20)
        );
    }
    assert!(resolve_pricing(None, &provider(None), "gpt-known").is_none());
}

#[test]
fn catalog_prefix_exact_longest_boundaries_missing_zero_and_cache_fallback() {
    let (_dir, catalog) = catalog(json!({
        "anthropic": {"models": {
            "claude-haiku": {"id":"claude-haiku", "cost":{"input":9,"output":9}},
            "claude-haiku-5-5": {"id":"claude-haiku-5-5", "cost":{"input":1,"output":5}},
            "claude-haiku-5-5-medium": {"id":"claude-haiku-5-5-medium", "cost":{"input":2,"output":6}},
            "claude-haiku-5-5-zero": {"id":"claude-haiku-5-5-zero", "cost":{"input":0,"output":0,"cache_read":0}},
            "claude-haiku-5-5-missing": {"id":"claude-haiku-5-5-missing", "cost":{"input":1}},
            "claude-haiku-5-5-invalid": {"id":"claude-haiku-5-5-invalid", "cost":{"input":-1,"output":5}}
        }},
        "openai": {"models": {"gpt-4":{"id":"gpt-4","cost":{"input":1,"output":5}}}},
        "zai": {"models": {"glm-5.3":{"id":"glm-5.3","cost":{"input":1,"output":5}}}}
    }));
    let p = provider(None);
    for (model, base, input) in [
        ("claude-haiku-5-5", "claude-haiku-5-5", 1.0),
        ("claude-haiku-5-5-medium", "claude-haiku-5-5-medium", 2.0),
        (
            "CLAUDE-HAIKU-5-5-MEDIUM-high",
            "claude-haiku-5-5-medium",
            2.0,
        ),
        ("claude-haiku-5-5_high", "claude-haiku-5-5", 1.0),
        ("claude-haiku-5-5:high", "claude-haiku-5-5", 1.0),
        ("claude-haiku-5-5/high", "claude-haiku-5-5", 1.0),
    ] {
        let price = resolve_pricing(Some(&catalog), &p, model).unwrap();
        assert_eq!(price.model, base);
        assert_eq!(price.input, input);
        assert!(
            price
                .cost(&row(1_000_000, 10, 0))
                .unwrap()
                .cache_rate_fallback
        );
    }
    let zero = resolve_pricing(Some(&catalog), &p, "claude-haiku-5-5-zero-medium")
        .unwrap()
        .cost(&row(1_000_000, 250_000, 100))
        .unwrap();
    assert_eq!(zero.total_cost, 0.0);
    assert!(!zero.cache_rate_fallback);
    for model in [
        "claude-haiku-5-5-missing",
        "claude-haiku-5-5-missing-high",
        "claude-haiku-5-5-invalid-high",
        "gpt-40",
        "gpt-4.1",
        "gpt-4x",
        "glm-5.30",
        "glm-5.3.1",
        "glm-5.3+medium",
        "unknown",
    ] {
        assert!(
            resolve_pricing(Some(&catalog), &p, model).is_none(),
            "{model}"
        );
    }
}

#[test]
fn claude_haiku_medium_inherits_registered_base_without_variant_entry() {
    let (_dir, catalog) = catalog(json!({"anthropic":{"models":{
        "claude-haiku-5-5":{"id":"claude-haiku-5-5","cost":{"input":1,"output":5}}
    }}}));
    let price =
        resolve_pricing(Some(&catalog), &provider(None), "claude-haiku-5-5-medium").unwrap();
    assert_eq!(price.model, "claude-haiku-5-5");
    assert_eq!(price.input, 1.0);
}

#[test]
fn longest_identity_precedes_ambiguity_validation_and_vendor_fallback() {
    let (_dir, catalog) = catalog(json!({
        "one":{"models":{
            "base":{"id":"base","cost":{"input":1,"output":2}},
            "base-long":{"id":"base-long","cost":{"input":3,"output":4}},
            "base-bad":{"id":"base-bad","cost":{"input":1}},
            "base-collision":{"id":"base-collision","cost":{"input":1,"output":2}},
            "BASE-COLLISION":{"id":"BASE-COLLISION","cost":{"input":1,"output":2}},
            "base-exact":{"id":"base-exact","cost":{"input":9,"output":9}}
        }},
        "two":{"models":{
            "base":{"id":"base","cost":{"input":5,"output":6}},
            "base-collision":{"id":"base-collision","cost":{"input":1,"output":2}}
        }}
    }));
    // Shorter ambiguous prefixes cannot defeat the unique longest identity.
    assert_eq!(
        resolve_pricing(Some(&catalog), &provider(None), "base-long-more")
            .unwrap()
            .input,
        3.0
    );
    assert_eq!(
        resolve_pricing(Some(&catalog), &provider(None), "base-exact")
            .unwrap()
            .input,
        9.0
    );
    for model in ["base-more", "base-bad-more", "base-collision-more"] {
        assert!(resolve_pricing(Some(&catalog), &provider(None), model).is_none());
    }
    assert!(
        resolve_pricing(
            Some(&catalog),
            &provider(Some("one")),
            "base-collision-more"
        )
        .is_none()
    );
    // Explicit vendor choice is preserved even when another vendor has a longer ID.
    assert_eq!(
        resolve_pricing(Some(&catalog), &provider(Some("two")), "base-long-more")
            .unwrap()
            .input,
        5.0
    );
}

#[test]
fn serde_contract_defaults_absent_cost_and_keeps_null_distinct_from_zero() {
    let mut value = serde_json::to_value(row(0, 0, 0)).unwrap();
    assert!(value.get("cost").unwrap().is_null());
    value.as_object_mut().unwrap().remove("cost");
    let mut row: TokenUsageDay = serde_json::from_value(value).unwrap();
    assert!(row.cost.is_none());
    row.cost = Pricing::from_capability(&capability()).unwrap().cost(&row);
    let value = serde_json::to_value(row).unwrap();
    assert_eq!(value["cost"]["total_cost"], 0.0);
    let keys: Vec<_> = value["cost"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(
        keys,
        vec![
            "cache_rate_fallback",
            "cache_read_cost",
            "cache_read_rate",
            "currency",
            "input_cost",
            "input_rate",
            "is_manual",
            "output_cost",
            "output_rate",
            "pricing_model",
            "pricing_provider",
            "total_cost"
        ]
    );
}

#[test]
fn summarize_costs_sums_per_native_currency_and_counts_unpriced() {
    let priced = |currency: &str, input: f64, count: i64| {
        let mut r = row(1_000_000, 0, 0);
        r.request_count = count;
        let mut p = Pricing::from_capability(&capability()).unwrap();
        p.currency = currency.into();
        p.input = input;
        r.cost = p.cost(&r);
        r
    };
    let mut unpriced = row(5, 0, 5);
    unpriced.request_count = 4;
    let rows = vec![
        priced("USD", 2.0, 1),
        priced("CNY", 20.0, 2),
        priced("USD", 3.0, 3),
        unpriced,
    ];
    let (totals, unpriced_count) = summarize_costs(&rows);
    assert_eq!(unpriced_count, 4);
    assert_eq!(
        totals.iter().map(|t| t.currency.as_str()).collect::<Vec<_>>(),
        ["CNY", "USD"]
    );
    assert_close(totals[0].total_cost, 20.0);
    assert_eq!(totals[0].priced_request_count, 2);
    assert_close(totals[1].total_cost, 5.0);
    assert_eq!(totals[1].priced_request_count, 4);
    assert!(summarize_costs(&[]).0.is_empty());
}
