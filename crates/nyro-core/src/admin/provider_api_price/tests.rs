use super::*;
use serde_json::json;

fn ms(value: &str) -> i64 {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .timestamp_millis()
}

fn tier(name: &str, reset: Option<&str>, used: f64) -> ProviderUsageTier {
    ProviderUsageTier {
        name: name.into(),
        used_percent: used,
        resets_at: reset.map(str::to_owned),
    }
}

#[test]
fn provider_api_price_weekly_wins_over_short_monthly_and_feature_windows() {
    let now = ms("2026-10-31T13:48:02+08:00");
    let end = "2026-11-06T17:59:59+08:00";
    let tiers = [
        tier("five_hour", Some("2026-10-31T16:09:59+08:00"), 80.0),
        tier("monthly", Some("2026-11-01T00:00:00Z"), 20.0),
        tier(
            "feature:sonnet:weekly_limit",
            Some("2026-11-01T00:00:00Z"),
            10.0,
        ),
        tier("weekly_limit", Some(end), 4.0),
    ];
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        let period = provider_period(kind, &tiers, now).unwrap();
        assert_eq!(
            (period.start, period.end),
            (ms("2026-10-30T17:59:59+08:00"), ms(end))
        );
        assert_eq!(
            (period.tier, period.used_percent),
            ("weekly_limit", Some(4.0))
        );
        assert!(provider_period(kind, &tiers, period.start).is_some());
        assert!(provider_period(kind, &tiers, period.start - 1).is_none());
        assert!(provider_period(kind, &tiers, period.end).is_none());
        assert!(provider_period(kind, &tiers, period.end + 1).is_none());
    }
    assert!(provider_period("kimi_coding_plan", &tiers, now).is_none());
}

#[test]
fn provider_api_price_present_invalid_weekly_never_falls_back() {
    let now = ms("2026-10-31T05:48:02Z");
    let fallback = [
        tier("five_hour", Some("2026-10-31T07:00:00Z"), 20.0),
        tier("monthly", Some("2026-11-01T00:00:00Z"), 20.0),
    ];
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        assert!(provider_period(kind, &fallback, now).is_some());
        for reset in [
            None,
            Some("bad"),
            Some("2026-10-30T00:00:00Z"),
            Some("2027-11-06T00:00:00Z"),
            Some("1970-01-02T00:00:00Z"),
        ] {
            let tiers = [
                fallback[0].clone(),
                fallback[1].clone(),
                tier("weekly_limit", reset, 10.0),
            ];
            assert!(
                provider_period(kind, &tiers, now).is_none(),
                "{kind}: {reset:?}"
            );
        }
        let weekly = tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 10.0);
        let tiers = [
            fallback[0].clone(),
            fallback[1].clone(),
            weekly.clone(),
            weekly,
        ];
        assert!(
            provider_period(kind, &tiers, now).is_none(),
            "duplicated weekly is ambiguous"
        );
        assert!(provider_period(kind, &[], now).is_none());
        assert!(
            provider_period(
                kind,
                &[tier(
                    "feature:model:weekly_limit",
                    Some("2026-11-06T00:00:00Z"),
                    10.0
                )],
                now
            )
            .is_none()
        );
    }
}

#[test]
fn provider_api_price_codex_weekly_only_and_known_five_hour_fallback() {
    let now = ms("2026-11-03T10:29:00+08:00");
    let reset = "2026-11-04T18:29:00+08:00";
    let weekly = tier("weekly_limit", Some(reset), 76.0);
    let period = provider_period("openai_codex", std::slice::from_ref(&weekly), now).unwrap();
    assert_eq!(period.start, ms("2026-10-28T18:29:00+08:00"));
    assert_eq!(period.end, ms(reset));
    assert_eq!(period.used_percent, Some(76.0));

    let short = tier("five_hour", Some("2026-11-03T13:29:00+08:00"), 20.0);
    let tiers = [
        tier("feature:codex_bengalfox:weekly_limit", Some(reset), 99.0),
        tier("primary_window", Some(reset), 50.0),
        short.clone(),
    ];
    let period = provider_period("openai_codex", &tiers, now).unwrap();
    assert_eq!(period.tier, "five_hour");
    assert_eq!(period.start, ms("2026-11-03T08:29:00+08:00"));
    assert_eq!(period.end, ms("2026-11-03T13:29:00+08:00"));
    assert!(provider_period("openai_codex", &[short.clone(), short], now).is_none());

    for name in [
        "primary_window",
        "secondary_window",
        "daily",
        "monthly",
        "feature:codex_bengalfox:weekly_limit",
        "feature:codex_bengalfox:five_hour",
    ] {
        assert!(
            provider_period("openai_codex", &[tier(name, Some(reset), 76.0)], now).is_none(),
            "unknown-duration or feature quota is not an account cycle: {name}"
        );
    }
}

#[test]
fn provider_api_price_google_weekly_and_five_hour_require_canonical_account_windows() {
    let now = ms("2026-11-03T10:29:00+08:00");
    let reset = "2026-11-04T18:29:00+08:00";
    let weekly = tier("weekly_limit", Some(reset), 76.0);
    let period =
        provider_period("google_subscription", std::slice::from_ref(&weekly), now).unwrap();
    assert_eq!(period.start, ms("2026-10-28T18:29:00+08:00"));
    assert_eq!(period.end, ms(reset));
    assert_eq!(period.used_percent, Some(76.0));

    let short = tier("five_hour", Some("2026-11-03T13:29:00+08:00"), 20.0);
    let tiers = [
        tier("gemini", Some(reset), 50.0),
        tier("feature:gemini:weekly_limit", Some(reset), 99.0),
        short.clone(),
    ];
    let period = provider_period("google_subscription", &tiers, now).unwrap();
    assert_eq!(period.tier, "five_hour");
    assert_eq!(period.start, ms("2026-11-03T08:29:00+08:00"));
    assert_eq!(period.end, ms("2026-11-03T13:29:00+08:00"));
    assert!(provider_period("google_subscription", &[short.clone(), short], now).is_none());

    for name in [
        "gemini",
        "gemini-3.8-flash",
        "primary_window",
        "secondary_window",
        "daily",
        "monthly",
        "window",
        "feature:gemini:weekly_limit",
        "feature:gemini:five_hour",
    ] {
        assert!(
            provider_period("google_subscription", &[tier(name, Some(reset), 76.0)], now).is_none(),
            "catalog, feature and unknown-duration quotas must not define a Google account cycle: {name}"
        );
    }
}

#[test]
fn provider_api_price_five_hour_and_grok_utc_date_only_month_clamping() {
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "openai_codex",
        "google_subscription",
    ] {
        let tiers = [tier("five_hour", Some("2026-10-31T07:00:00Z"), 25.0)];
        let period = provider_period(kind, &tiers, ms("2026-10-31T05:48:02Z")).unwrap();
        assert_eq!(
            (period.start, period.end),
            (ms("2026-10-31T02:00:00Z"), ms("2026-10-31T07:00:00Z"))
        );
        assert!(
            provider_period(
                kind,
                &[tier("five_hour", Some("2026-11-01"), 1.0)],
                ms("2026-10-31T23:00:00Z")
            )
            .is_none()
        );
        assert!(
            provider_period(kind, &[tiers[0].clone(), tiers[0].clone()], period.start).is_none()
        );
    }
    for (reset, start, now) in [
        ("2026-03-31", "2026-02-28T00:00:00Z", "2026-03-10T00:00:00Z"),
        (
            "2024-03-31T09:59:59Z",
            "2024-02-29T09:59:59Z",
            "2024-03-10T00:00:00Z",
        ),
        (
            "2027-01-06T09:59:59+08:00",
            "2026-12-06T01:59:59Z",
            "2027-01-01T00:00:00Z",
        ),
        (
            "2026-03-01T00:30:00+08:00",
            "2026-01-31T16:30:00Z",
            "2026-02-10T00:00:00Z",
        ),
        (
            "2026-03-31T23:30:00-08:00",
            "2026-03-01T07:30:00Z",
            "2026-03-10T00:00:00Z",
        ),
    ] {
        let tiers = [tier("monthly", Some(reset), 25.0)];
        let period = provider_period("grok_plan", &tiers, ms(now)).unwrap();
        assert_eq!(period.start, ms(start));
        assert_eq!(
            period.end,
            reset_time("grok_plan", reset).unwrap().timestamp_millis()
        );
        assert_eq!(period.tier, "monthly");
    }
    let weekly = [tier("weekly_limit", Some("2026-08-21"), 63.5)];
    let period = provider_period("grok_plan", &weekly, ms("2026-08-20T00:00:00Z")).unwrap();
    assert_eq!(
        (period.start, period.end),
        (ms("2026-08-14T00:00:00Z"), ms("2026-08-21T00:00:00Z"))
    );
    assert!(reset_time("grok_plan", "2026-8-21").is_none());
    assert!(reset_time("grok_plan", "2026-02-30").is_none());
    assert!(reset_time("grok_plan", "2026-08-21T00:00:00").is_none());
    assert!(
        provider_period(
            "grok_plan",
            &[tier("five_hour", Some("2026-08-20T01:00:00Z"), 1.0)],
            period.start
        )
        .is_none()
    );
}

fn totals(cost: f64, priced: i64, unpriced: i64) -> AccountUsageTotals {
    AccountUsageTotals {
        total_tokens: 100,
        request_count: priced + unpriced,
        equivalent_costs: vec![EquivalentCostTotal {
            currency: "USD".into(),
            total_cost: cost,
            priced_request_count: priced,
        }],
        unpriced_request_count: unpriced,
    }
}

#[test]
fn provider_api_price_projection_requires_finite_positive_usage_and_priced_sample() {
    for used in [0.0, -1.0, 100.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(estimate_capacity(&totals(1.0, 1, 0), used).is_none());
    }
    assert!(estimate_capacity(&totals(1.0, 0, 0), 25.0).is_none());
    assert!(
        estimate_capacity(&totals(0.0, 0, 1), 25.0).is_none(),
        "unpriced zero is not free"
    );
    for cost in [-0.1, f64::NAN, f64::INFINITY, f64::MAX] {
        assert!(estimate_capacity(&totals(cost, 1, 0), 25.0).is_none());
    }
    assert!(estimate_capacity(&totals(1.0, 1, 0), f64::MIN_POSITIVE).is_none());
    assert_eq!(
        estimate_capacity(&totals(0.0, 1, 0), f64::MIN_POSITIVE).unwrap()[0].total_cost,
        0.0,
        "priced zero remains valid even for a tiny fraction"
    );
    let partial = estimate_capacity(&totals(2.5, 1, 9), 25.0).unwrap();
    assert_eq!(
        (partial[0].total_cost, partial[0].priced_request_count),
        (10.0, 1),
        "partial prices remain a partial priced sample"
    );
    assert_eq!(
        estimate_capacity(&totals(2.5, 1, 0), 100.0).unwrap()[0].total_cost,
        2.5
    );
    let mut multiple = totals(1.0, 1, 1);
    multiple.equivalent_costs.push(EquivalentCostTotal {
        currency: "CNY".into(),
        total_cost: 7.0,
        priced_request_count: 2,
    });
    let capacity = estimate_capacity(&multiple, 20.0).unwrap();
    assert_eq!(
        capacity
            .iter()
            .map(|c| (c.currency.as_str(), c.total_cost))
            .collect::<Vec<_>>(),
        [("USD", 5.0), ("CNY", 35.0)]
    );
    multiple.equivalent_costs[1].total_cost = f64::MAX;
    assert!(
        estimate_capacity(&multiple, 20.0).is_none(),
        "overflow suppresses the complete projection, not only one currency"
    );
}

fn bucket(id: &str, scope: &str, now: i64, reset: &str) -> crate::quota_buckets::QuotaBucket {
    crate::quota_buckets::QuotaBucket {
        id: id.to_owned(),
        scope_id: scope.to_owned(),
        label: id.to_owned(),
        models: vec![format!("wire-{id}")],
        binding: crate::quota_buckets::QuotaBucketBinding {
            status: "resolved".into(),
            source: "configured".into(),
            revision: 1,
        },
        tiers: vec![tier("weekly_limit", Some(reset), 25.0)],
        constraint_ids: vec![],
        source: "enforced_summary".into(),
        queried_at: DateTime::from_timestamp_millis(now).unwrap().to_rfc3339(),
        stale: false,
        scheduling: ProviderScheduling::default(),
        api_price: None,
    }
}

#[test]
fn google_display_prices_split_mixed_exact_models_without_reassigning_raw_history() {
    use crate::quota_buckets::{google_display_buckets, is_google_display_bucket};
    let now = ms("2026-11-03T10:29:00+08:00");
    let reset = "2026-11-04T18:29:00+08:00";
    let mut raw = bucket("immutable-mixed", "owner", now, reset);
    raw.models = vec!["gemini-3.8-flash-high".into(), "gemini-4-argon-low".into()];
    let mut stats = ProviderApiPriceStats {
        all_time: totals(30.0, 2, 0),
        period: Some(totals(20.0, 2, 0)),
        period_start: Some(now - 1),
        period_end: Some(ms(reset)),
        period_tier: Some("weekly_limit".into()),
        used_percent: Some(25.0),
        estimated_capacity: Some(totals(80.0, 2, 0).equivalent_costs),
        quota_adjusted: None,
        all_time_coverage: Some(ProviderApiPriceCoverage {
            input_tokens: 200,
            priced_request_count: 2,
            attributed_request_count: 2,
            ..Default::default()
        }),
        period_coverage: None,
        estimated_capacity_partial: None,
        unassigned: None,
        unassigned_coverage: None,
        retained_buckets: Vec::new(),
        models: vec![],
    };
    for (model, cost) in [
        ("gemini-3.8-flash-high", 10.0),
        ("gemini-4-argon-low", 20.0),
    ] {
        stats.models.push(QuotaModelPriceStats {
            model: model.into(),
            all_time: totals(cost, 1, 0),
            coverage: ProviderApiPriceCoverage {
                input_tokens: 100,
                priced_request_count: 1,
                attributed_request_count: 1,
                ..Default::default()
            },
        });
    }
    raw.api_price = Some(stats);
    let original = raw.clone();
    let projected = google_display_buckets(std::slice::from_ref(&raw));
    let families = projected
        .iter()
        .filter(|bucket| is_google_display_bucket(bucket))
        .collect::<Vec<_>>();
    assert_eq!(families.len(), 2);
    assert_eq!(
        families
            .iter()
            .map(|bucket| bucket.api_price.as_ref().unwrap().all_time.request_count)
            .sum::<i64>(),
        2
    );
    assert_eq!(
        families[0]
            .api_price
            .as_ref()
            .unwrap()
            .all_time
            .equivalent_costs[0]
            .total_cost,
        10.0
    );
    assert_eq!(
        families[1]
            .api_price
            .as_ref()
            .unwrap()
            .all_time
            .equivalent_costs[0]
            .total_cost,
        20.0
    );
    assert!(families.iter().all(|bucket| {
        let stats = bucket.api_price.as_ref().unwrap();
        stats.period.is_none() && stats.estimated_capacity.is_none() && stats.models.len() == 1
    }));
    let retained = projected.iter().find(|bucket| bucket.id == raw.id).unwrap();
    assert_eq!(retained.api_price, raw.api_price);
    assert_eq!(retained.scope_id, "owner");
    assert_eq!(
        raw, original,
        "display projection must never rewrite immutable accounting evidence"
    );
}

#[test]
fn google_display_prices_keep_hydrogen_and_flash_in_their_shared_pool() {
    use crate::quota_buckets::{google_display_buckets, is_google_display_bucket};
    let now = ms("2026-11-15T22:00:00+08:00");
    let reset = "2026-11-16T03:00:00+08:00";
    let mut raw = bucket("immutable-mixed", "owner", now, reset);
    raw.models = vec![
        "gemini-3.8-flash-high".into(),
        "gemini-4-argon-low".into(),
        "gemini-4-hydrogen-high".into(),
    ];
    let mut stats = ProviderApiPriceStats {
        all_time: totals(45.0, 3, 0),
        period: Some(totals(45.0, 3, 0)),
        period_start: Some(now - 1),
        period_end: Some(ms(reset)),
        period_tier: Some("five_hour".into()),
        used_percent: Some(25.0),
        estimated_capacity: None,
        quota_adjusted: None,
        all_time_coverage: Some(ProviderApiPriceCoverage::default()),
        period_coverage: None,
        estimated_capacity_partial: None,
        unassigned: None,
        unassigned_coverage: None,
        retained_buckets: Vec::new(),
        models: vec![],
    };
    for (model, cost) in [
        ("gemini-3.8-flash-high", 10.0),
        ("gemini-4-argon-low", 20.0),
        ("gemini-4-hydrogen-high", 15.0),
    ] {
        stats.models.push(QuotaModelPriceStats {
            model: model.into(),
            all_time: totals(cost, 1, 0),
            coverage: ProviderApiPriceCoverage::default(),
        });
    }
    raw.api_price = Some(stats);
    let projected = google_display_buckets(std::slice::from_ref(&raw));
    let cards = projected
        .iter()
        .filter(|bucket| is_google_display_bucket(bucket))
        .collect::<Vec<_>>();
    // Hydrogen draws from the same Gemini Models allowance as Flash: one card,
    // headlined by Hydrogen, carrying both exact model rows.
    assert_eq!(cards.len(), 2);
    let gemini = cards
        .iter()
        .find(|card| card.label == "gemini-4-hydrogen")
        .unwrap();
    let gemini_price = gemini.api_price.as_ref().unwrap();
    assert_eq!(gemini_price.all_time.request_count, 2);
    assert_eq!(gemini_price.all_time.equivalent_costs[0].total_cost, 25.0);
    assert_eq!(
        gemini_price
            .models
            .iter()
            .map(|row| row.model.as_str())
            .collect::<Vec<_>>(),
        vec!["gemini-3.8-flash-high", "gemini-4-hydrogen-high"]
    );
    let argon = cards
        .iter()
        .find(|card| card.label == "gemini-4-argon")
        .unwrap();
    assert_eq!(
        argon.api_price.as_ref().unwrap().all_time.equivalent_costs[0].total_cost,
        20.0
    );
}

#[tokio::test]
async fn bucket_prices_partition_history_periods_exact_prices_currencies_and_missing_evidence()
-> anyhow::Result<()> {
    use crate::config::GatewayConfig;
    use crate::storage::{SqliteStorage, Storage};
    use std::sync::Arc;
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    let provider = storage.providers().create(serde_json::from_value(json!({
        "name":"buckets", "protocol":"openai/chat", "base_url":"https://example.invalid", "api_key":"test"
    }))?).await?;
    let now = ms("2026-11-05T08:00:00Z");
    let attribution = |scope: &str, bucket: &str, model: &str, dispatched_at: i64| {
        json!({"quota_attribution": {
        "version":1,"scope_id":scope,"bucket_id":bucket,"backend_model":"alias","wire_model":model,
        "source":"configured","revision":1,"dispatched_at":dispatched_at
    }}).to_string()
    };
    for (index, decision, at, input, output) in [
        (
            0,
            Some(attribution("owner", "flash", "wire-flash", now - 1)),
            now,
            Some(1_000_000_i64),
            Some(0_i64),
        ),
        (
            1,
            Some(attribution("owner", "argon", "wire-argon", now - 1)),
            now,
            Some(1_000_000),
            Some(0),
        ),
        (
            2,
            Some(attribution("old-owner", "flash", "wire-flash", now - 1)),
            now,
            Some(1_000_000),
            Some(0),
        ),
        (3, None, now, Some(1_000_000), Some(0)),
        (
            4,
            Some(attribution("owner", "flash", "wire-flash", now - 1)),
            now,
            None,
            None,
        ),
        (
            5,
            Some(attribution("owner", "argon", "no-argon-price", now - 1)),
            now,
            Some(100),
            Some(0),
        ),
        // Emitted inside Flash's week but dispatched before it: all-time only.
        (
            6,
            Some(attribution(
                "owner",
                "flash",
                "wire-flash",
                ms("2026-10-29T00:00:00Z"),
            )),
            now,
            Some(1_000_000),
            Some(0),
        ),
        (
            7,
            Some(attribution("owner", "flash", "wire-flash", now)),
            now + 1,
            Some(1_000_000),
            Some(0),
        ),
    ] {
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, route_decision, input_tokens, output_tokens, cache_read_tokens) VALUES (?, ?, 'wire-flash', ?, ?, ?, ?, 400000)")
            .bind(format!("bucket-{index}")).bind(&provider.id).bind(at).bind(decision).bind(input).bind(output).execute(storage.pool()).await?;
    }
    for (model, currency, input_rate) in [("wire-flash", "USD", 2.0), ("wire-argon", "CNY", 7.0)] {
        admin
            .set_token_price(TokenPriceInput {
                provider_id: provider.id.clone(),
                model: model.into(),
                price: Some(TokenPriceRates {
                    currency: currency.into(),
                    input_rate,
                    output_rate: 8.0,
                    cache_read_rate: None,
                }),
            })
            .await?;
    }
    let mut usage = snapshot(
        &provider.id,
        "google_subscription",
        now,
        vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 25.0)],
    );
    usage.quota_buckets = vec![
        bucket("flash", "owner", now, "2026-11-06T00:00:00Z"),
        bucket("argon", "owner", now, "2026-11-10T00:00:00Z"),
    ];
    admin.enrich_provider_api_price(&mut usage).await;
    let stats = usage.api_price.as_ref().unwrap();
    assert_eq!(stats.all_time.request_count, 7);
    assert!(stats.period.is_none() && stats.estimated_capacity.is_none());
    assert_eq!(
        stats
            .all_time
            .equivalent_costs
            .iter()
            .map(|cost| (cost.currency.as_str(), cost.total_cost))
            .collect::<Vec<_>>(),
        [("CNY", 7.0), ("USD", 8.0)]
    );
    assert_eq!(stats.unassigned.as_ref().unwrap().request_count, 1);
    assert_eq!(stats.retained_buckets[0].scope_id, "old-owner");
    let flash = usage.quota_buckets[0].api_price.as_ref().unwrap();
    assert_eq!(flash.all_time.request_count, 3);
    assert_eq!(
        flash.all_time.unpriced_request_count, 1,
        "missing evidence is not a priced free zero"
    );
    assert_eq!(flash.period.as_ref().unwrap().request_count, 2);
    assert_eq!(
        flash.period.as_ref().unwrap().equivalent_costs[0].total_cost,
        2.0
    );
    assert_eq!(
        flash.estimated_capacity.as_ref().unwrap()[0].total_cost,
        8.0
    );
    assert_eq!(flash.estimated_capacity_partial, Some(true));
    assert_eq!(
        flash
            .period_coverage
            .as_ref()
            .unwrap()
            .missing_usage_request_count,
        1
    );
    let argon = usage.quota_buckets[1].api_price.as_ref().unwrap();
    assert_eq!(
        argon.all_time.unpriced_request_count, 1,
        "unknown Argon pricing never borrows Flash's rate"
    );
    assert_ne!(flash.period_start, argon.period_start);
    // Exact log/detail predicate also uses immutable identity, not current model membership.
    let filtered = admin
        .get_provider_usage_detail_filtered(
            &provider.id,
            StatsRange {
                start_at: Some(now - 1000),
                end_at: Some(now),
                hours: None,
            },
            Some("argon"),
            Some("owner"),
        )
        .await?;
    assert_eq!(filtered.request_count, 2);
    assert_eq!(
        filtered
            .models
            .iter()
            .map(|model| model.upstream_model.as_str())
            .collect::<Vec<_>>(),
        ["no-argon-price", "wire-argon"]
    );
    usage.quota_buckets[0].stale = true;
    usage.quota_buckets[1].source = "catalog_exact_model_v1".into();
    admin.enrich_provider_api_price(&mut usage).await;
    for bucket in &usage.quota_buckets {
        let stats = bucket.api_price.as_ref().unwrap();
        assert!(
            stats.period.is_none() && stats.estimated_capacity.is_none(),
            "stale/catalog reset cannot fabricate a cycle"
        );
    }
    storage.logs().clear_all().await?;
    admin.enrich_provider_api_price(&mut usage).await;
    assert_eq!(usage.api_price.unwrap().all_time.request_count, 0);
    assert!(
        usage.quota_buckets.iter().all(|bucket| bucket
            .api_price
            .as_ref()
            .unwrap()
            .all_time
            .request_count
            == 0)
    );
    Ok(())
}

#[tokio::test]
async fn provider_api_price_codex_weights_retained_tiers_without_repricing_base()
-> anyhow::Result<()> {
    use crate::config::GatewayConfig;
    use crate::storage::{SqliteStorage, Storage};
    use std::sync::Arc;

    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    let provider = storage.providers().create(serde_json::from_value(json!({
        "name":"codex", "protocol":"openai-responses/responses/v1", "base_url":"https://example.invalid", "api_key":"test", "fast_mode":"ultrafast"
    }))?).await?;
    // Each attempt costs 2 USD: 0.5M uncached at $2, 0.5M cached at $1, 0.25M output at $2.
    admin
        .set_token_price(TokenPriceInput {
            provider_id: provider.id.clone(),
            model: "test-model".into(),
            price: Some(TokenPriceRates {
                currency: "USD".into(),
                input_rate: 2.0,
                cache_read_rate: Some(1.0),
                output_rate: 2.0,
            }),
        })
        .await?;
    let now = ms("2026-11-05T08:00:00Z");
    let mut usage = snapshot(
        &provider.id,
        "openai_codex",
        now,
        vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 25.0)],
    );
    for (id, mode, source, at) in [
        ("normal", Some("normal"), "upstream_request", now),
        ("fast", Some("fast"), "upstream_response", now),
        ("ultrafast", Some("ultrafast"), "upstream_request", now),
        ("older", None, "", now - 7 * 86_400_000),
        ("future", Some("ultrafast"), "upstream_response", now + 1),
    ] {
        let metadata = mode.map(|value| {
            json!({"codex_service_tier": {"value":value,"source":source}}).to_string()
        });
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, payload_metadata_json, input_tokens, output_tokens, cache_read_tokens) VALUES (?, ?, 'test-model', ?, ?, 1000000, 250000, 500000)")
            .bind(id).bind(&provider.id).bind(at).bind(metadata).execute(storage.pool()).await?;
    }
    admin.enrich_provider_api_price(&mut usage).await;
    let stats = usage.api_price.as_ref().unwrap();
    assert_eq!(stats.all_time.equivalent_costs[0].total_cost, 8.0);
    assert_eq!(
        stats.period.as_ref().unwrap().equivalent_costs[0].total_cost,
        6.0
    );
    assert_eq!(
        stats.estimated_capacity.as_ref().unwrap()[0].total_cost,
        24.0
    );
    let adjusted = stats.quota_adjusted.as_ref().unwrap();
    assert_eq!(adjusted.all_time[0].total_cost, 25.0);
    assert!(
        adjusted.all_time_partial,
        "legacy attempt retains its base price"
    );
    let current = &adjusted.period.as_ref().unwrap()[0];
    assert_eq!(
        (current.total_cost, current.priced_request_count),
        (23.0, 3)
    );
    assert_eq!(adjusted.period_partial, Some(false));
    assert_eq!(
        adjusted.estimated_capacity.as_ref().unwrap()[0].total_cost,
        92.0
    );

    // A historical request within this period changes coverage, not its base price.
    sqlx::query("UPDATE request_logs SET created_at = ? WHERE id = 'older'")
        .bind(now)
        .execute(storage.pool())
        .await?;
    usage.tiers[0].used_percent = 0.0;
    admin.enrich_provider_api_price(&mut usage).await;
    let adjusted = usage
        .api_price
        .as_ref()
        .unwrap()
        .quota_adjusted
        .as_ref()
        .unwrap();
    assert_eq!(adjusted.period.as_ref().unwrap()[0].total_cost, 25.0);
    assert_eq!(adjusted.period_partial, Some(true));
    assert!(adjusted.estimated_capacity.is_none());

    sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, payload_metadata_json, input_tokens, output_tokens) VALUES ('no-price', ?, 'no-reference', ?, ?, 1, 1), ('no-usage', ?, 'test-model', ?, ?, NULL, NULL)")
        .bind(&provider.id).bind(now).bind(r#"{"codex_service_tier":{"value":"fast","source":"upstream_request"}}"#)
        .bind(&provider.id).bind(now).bind(r#"{"codex_service_tier":{"value":"ultrafast","source":"upstream_response"}}"#)
        .execute(storage.pool()).await?;
    usage.tiers[0].used_percent = 25.0;
    admin.enrich_provider_api_price(&mut usage).await;
    let adjusted = usage
        .api_price
        .as_ref()
        .unwrap()
        .quota_adjusted
        .as_ref()
        .unwrap();
    let projection = &adjusted.estimated_capacity.as_ref().unwrap()[0];
    assert_eq!(
        (projection.total_cost, projection.priced_request_count),
        (100.0, 4)
    );
    assert_eq!(adjusted.period_partial, Some(true));

    usage.tiers.clear();
    admin.enrich_provider_api_price(&mut usage).await;
    let adjusted = usage
        .api_price
        .as_ref()
        .unwrap()
        .quota_adjusted
        .as_ref()
        .unwrap();
    assert!(adjusted.period.is_none() && adjusted.estimated_capacity.is_none());
    usage.kind = "grok_plan".into();
    admin.enrich_provider_api_price(&mut usage).await;
    assert!(
        serde_json::to_value(usage.api_price.unwrap())?
            .get("quota_adjusted")
            .is_none()
    );
    Ok(())
}

fn snapshot(provider: &str, kind: &str, now: i64, tiers: Vec<ProviderUsageTier>) -> ProviderUsage {
    ProviderUsage {
        provider_id: provider.into(),
        kind: kind.into(),
        site: "global".into(),
        level: None,
        tiers,
        balances: vec![],
        spends: vec![],
        is_available: Some(true),
        scheduling: ProviderScheduling::default(),
        queried_at: DateTime::from_timestamp_millis(now).unwrap().to_rfc3339(),
        api_price: None,
        quota_buckets: vec![],
        account_pool: None,
        pool_quota: None,
        upstream: None,
    }
}

#[tokio::test]
async fn provider_api_price_all_kinds_retained_history_unknown_cycles_and_fail_open()
-> anyhow::Result<()> {
    use crate::config::GatewayConfig;
    use crate::storage::{SqliteStorage, Storage};
    use std::sync::Arc;

    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    let provider = storage.providers().create(serde_json::from_value(json!({
        "name": "price-test", "protocol": "openai/chat", "base_url": "https://example.invalid", "api_key": "test"
    }))?).await?;
    let now = ms("2026-10-31T05:48:02Z");
    for (model, at, input) in [
        ("priced", 1, 1_000_000),
        ("priced", now, 1_000_000),
        ("unknown", now, 0),
        ("priced", now + 1, 100_000_000),
    ] {
        sqlx::query("INSERT INTO request_logs (id, provider_id, upstream_model, created_at, input_tokens, output_tokens, route_decision, outcome_version, attempt_outcome) VALUES (?, ?, ?, ?, ?, 0, NULL, 1, 'failed')")
            .bind(uuid::Uuid::new_v4().to_string()).bind(&provider.id).bind(model).bind(at).bind(input)
            .execute(storage.pool()).await?;
    }
    admin
        .set_token_price(TokenPriceInput {
            provider_id: provider.id.clone(),
            model: "priced".into(),
            price: Some(TokenPriceRates {
                currency: "USD".into(),
                input_rate: 2.0,
                output_rate: 8.0,
                cache_read_rate: None,
            }),
        })
        .await?;
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        for reset in [None, Some("invalid"), Some("2026-10-30T00:00:00Z")] {
            let mut usage = snapshot(
                &provider.id,
                kind,
                now,
                vec![
                    tier("weekly_limit", reset, 25.0),
                    tier("five_hour", Some("2026-10-31T07:00:00Z"), 80.0),
                    tier("monthly", Some("2026-11-01T00:00:00Z"), 20.0),
                ],
            );
            let scheduling = usage.scheduling.clone();
            admin.enrich_provider_api_price(&mut usage).await;
            assert_eq!(usage.scheduling, scheduling);
            let stats = usage.api_price.unwrap();
            assert_eq!(
                (stats.all_time.request_count, stats.all_time.total_tokens),
                (3, 2_000_000)
            );
            assert_eq!(stats.all_time.equivalent_costs[0].total_cost, 4.0);
            assert_eq!(stats.all_time.unpriced_request_count, 1);
            assert!(
                stats.period.is_none()
                    && stats.period_start.is_none()
                    && stats.period_end.is_none()
            );
            assert!(
                stats.period_tier.is_none()
                    && stats.used_percent.is_none()
                    && stats.estimated_capacity.is_none()
            );
        }
        let mut usage = snapshot(
            &provider.id,
            kind,
            now,
            vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 25.0)],
        );
        admin.enrich_provider_api_price(&mut usage).await;
        let stats = usage.api_price.unwrap();
        assert_eq!(stats.period.as_ref().unwrap().request_count, 2);
        assert_eq!(stats.period.as_ref().unwrap().unpriced_request_count, 1);
        assert_eq!(stats.estimated_capacity.unwrap()[0].total_cost, 8.0);
        for used in [0.0, -1.0, 100.1, f64::NAN, f64::INFINITY] {
            usage.api_price = None;
            usage.tiers[0].used_percent = used;
            admin.enrich_provider_api_price(&mut usage).await;
            let stats = usage.api_price.as_ref().unwrap();
            assert_eq!(stats.period.as_ref().unwrap().request_count, 2);
            assert_eq!(stats.used_percent, (used == 0.0).then_some(0.0));
            assert!(stats.estimated_capacity.is_none(), "{kind}: {used}");
        }
    }
    // Every supported kind reuses current manual rates, not historical billing
    // or a persisted projection; retention controls these same totals.
    admin
        .set_token_price(TokenPriceInput {
            provider_id: provider.id.clone(),
            model: "priced".into(),
            price: Some(TokenPriceRates {
                currency: "USD".into(),
                input_rate: 3.0,
                output_rate: 8.0,
                cache_read_rate: None,
            }),
        })
        .await?;
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        let mut usage = snapshot(
            &provider.id,
            kind,
            now,
            vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 25.0)],
        );
        admin.enrich_provider_api_price(&mut usage).await;
        let stats = usage.api_price.unwrap();
        assert_eq!(stats.all_time.equivalent_costs[0].total_cost, 6.0);
        assert_eq!(stats.period.unwrap().equivalent_costs[0].total_cost, 3.0);
        assert_eq!(stats.estimated_capacity.unwrap()[0].total_cost, 12.0);
    }
    storage.logs().clear_all().await?;
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        let mut usage = snapshot(
            &provider.id,
            kind,
            now,
            vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 25.0)],
        );
        admin.enrich_provider_api_price(&mut usage).await;
        let stats = usage.api_price.unwrap();
        assert_eq!(stats.all_time.request_count, 0);
        assert_eq!(stats.period.unwrap().request_count, 0);
        assert!(stats.all_time.equivalent_costs.is_empty());
        assert!(stats.estimated_capacity.is_none());
    }
    let mut unsupported = snapshot(&provider.id, "kimi_coding_plan", now, vec![]);
    admin.enrich_provider_api_price(&mut unsupported).await;
    assert!(
        serde_json::to_value(&unsupported)?
            .get("api_price")
            .is_none()
    );
    // Corrupt stored reserved data must not switch back to automatic pricing or
    // break an otherwise successful quota/scheduling response.
    storage
        .settings()
        .set("token_price.v1.corrupt", "not-json")
        .await?;
    for kind in [
        "glm_coding_plan",
        "minimax_coding_plan",
        "grok_plan",
        "openai_codex",
        "google_subscription",
    ] {
        let mut usage = snapshot(
            &provider.id,
            kind,
            now,
            vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 100.0)],
        );
        let before = usage.clone();
        admin.enrich_provider_api_price(&mut usage).await;
        assert_eq!(usage, before);
        assert!(usage.api_price.is_none());
        assert!(serde_json::to_value(&usage)?.get("api_price").is_none());
    }
    Ok(())
}

#[tokio::test]
async fn sub2api_retained_prices_follow_utc_month_and_shared_cutoff() -> anyhow::Result<()> {
    use crate::config::GatewayConfig;
    use crate::storage::{SqliteStorage, Storage};
    use std::sync::Arc;

    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let provider = storage
        .providers()
        .create(serde_json::from_value(json!({
            "name": "Relay", "channel": "sub2api", "protocol": "openai-compatible",
            "base_url": "https://relay.example.invalid/v1", "api_key": ""
        }))?)
        .await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    for (model, currency, input_rate) in [
        ("wire-usd", "USD", 2.0),
        ("wire-cny", "CNY", 8.0),
        ("client-alias", "USD", 999.0),
    ] {
        admin
            .set_token_price(TokenPriceInput {
                provider_id: provider.id.clone(),
                model: model.into(),
                price: Some(TokenPriceRates {
                    currency: currency.into(),
                    input_rate,
                    output_rate: 8.0,
                    cache_read_rate: Some(1.0),
                }),
            })
            .await?;
    }

    let dec = ms("2025-12-01T00:00:00Z");
    let jan = ms("2026-01-01T00:00:00Z");
    let feb = ms("2026-02-01T00:00:00Z");
    let known = (Some(1_000_000_i64), Some(500_000_i64), Some(250_000_i64));
    let zero = (Some(0), Some(0), None);
    let old_attribution = json!({"quota_attribution": {
        "version": 1, "scope_id": "retired-owner", "bucket_id": "retired-bucket",
        "backend_model": "client-alias", "wire_model": "wire-usd",
        "source": "configured", "revision": 1, "dispatched_at": jan - 1
    }})
    .to_string();
    for (index, (id, model, at, tokens, completed_at, attribution)) in [
        (provider.id.as_str(), "wire-usd", dec - 1, known, None, None),
        (&provider.id, "wire-usd", dec, known, None, None),
        (
            &provider.id,
            "wire-usd",
            dec + 1,
            (Some(500_000), None, None),
            None,
            None,
        ),
        (&provider.id, "wire-cny", jan - 1, known, None, None),
        (&provider.id, "wire-usd", jan - 1, zero, None, None),
        (
            &provider.id,
            "wire-usd",
            jan - 1,
            (None, None, None),
            None,
            None,
        ),
        (&provider.id, "no-reference", jan - 1, zero, None, None),
        (
            &provider.id,
            "wire-cny",
            jan - 1,
            known,
            Some(jan + 1),
            None,
        ),
        (
            &provider.id,
            "wire-usd",
            jan,
            known,
            None,
            Some(old_attribution.as_str()),
        ),
        (&provider.id, "wire-usd", feb, known, None, None),
        ("another-provider", "wire-usd", dec, known, None, None),
    ]
    .into_iter()
    .enumerate()
    {
        let failed = index == 1;
        sqlx::query(
            "INSERT INTO request_logs (id, provider_id, client_model, upstream_model, created_at,
             performance_completed_at, route_decision, input_tokens, output_tokens, cache_read_tokens,
             client_request_id, attempt_index, outcome_version, attempt_outcome, upstream_status_code)
             VALUES (?, ?, 'client-alias', ?, ?, ?, ?, ?, ?, ?, 'one-request', ?, 1, ?, ?)",
        )
        .bind(format!("relay-attempt-{index}"))
        .bind(id)
        .bind(model)
        .bind(at)
        .bind(completed_at)
        .bind(attribution)
        .bind(tokens.0)
        .bind(tokens.1)
        .bind(tokens.2)
        .bind(index as i32)
        .bind(if failed { "failed" } else { "completed" })
        .bind(if failed { 502 } else { 200 })
        .execute(storage.pool())
        .await?;
    }

    let stats_at = |at| {
        admin.sub2api_api_price_stats(
            provider.clone(),
            DateTime::from_timestamp_millis(at).unwrap(),
        )
    };
    let costs = |totals: &AccountUsageTotals| {
        totals
            .equivalent_costs
            .iter()
            .map(|cost| {
                (
                    cost.currency.clone(),
                    cost.total_cost,
                    cost.priced_request_count,
                )
            })
            .collect::<Vec<_>>()
    };
    let december = stats_at(jan - 1).await?;
    assert_eq!(
        (december.period_start, december.period_end),
        (Some(dec), Some(jan))
    );
    assert_eq!(december.period_tier.as_deref(), Some("calendar_month"));
    assert_eq!(december.all_time.request_count, 7);
    assert_eq!(december.all_time.total_tokens, 5_000_000);
    assert_eq!(
        costs(&december.all_time),
        [("CNY".to_owned(), 10.25, 1), ("USD".to_owned(), 12.5, 4)]
    );
    let period = december.period.as_ref().unwrap();
    assert_eq!(period.request_count, 6);
    assert_eq!(period.total_tokens, 3_500_000);
    assert_eq!(period.unpriced_request_count, 2);
    assert_eq!(
        costs(period),
        [("CNY".to_owned(), 10.25, 1), ("USD".to_owned(), 6.75, 3)]
    );
    let coverage = december.period_coverage.unwrap();
    assert_eq!(coverage.missing_usage_request_count, 2);
    assert_eq!(coverage.priced_request_count, 4);
    assert_eq!(coverage.unpriced_request_count, 2);
    assert!(december.used_percent.is_none());
    assert!(december.estimated_capacity.is_none());
    assert!(december.quota_adjusted.is_none());

    let january = stats_at(jan).await?;
    assert_eq!(
        (january.period_start, january.period_end),
        (Some(jan), Some(feb))
    );
    assert_eq!(january.all_time.request_count, 8);
    let period = january.period.unwrap();
    assert_eq!((period.request_count, period.total_tokens), (1, 1_500_000));
    assert_eq!(costs(&period), [("USD".to_owned(), 5.75, 1)]);

    storage.logs().clear_all().await?;
    let cleared = stats_at(jan).await?;
    assert_eq!(cleared.all_time.request_count, 0);
    assert!(cleared.all_time.equivalent_costs.is_empty());
    assert_eq!(cleared.period.unwrap().request_count, 0);
    Ok(())
}

#[tokio::test]
async fn sub2api_usage_is_admin_only_and_independent_of_quota_state() -> anyhow::Result<()> {
    use crate::config::GatewayConfig;
    use crate::storage::{SqliteStorage, Storage};
    use std::sync::Arc;

    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let mut providers = Vec::new();
    for (name, channel, base_url, vendor) in [
        (
            "Local relay",
            "  SuB2ApI  ",
            "https://relay.example.invalid/v1",
            "openai",
        ),
        // The vendor URL fallback would select Codex if the full-admin path
        // did not first consider the explicit Sub2API channel.
        (
            "Disabled relay",
            "sub2api",
            "https://chatgpt.com/backend-api/codex",
            "openai",
        ),
        (
            "Sub2API lookalike",
            "default",
            "https://sub2api.example.invalid/v1",
            "openai",
        ),
        (
            "Nyro relay",
            "sub2api",
            "https://nyro.example.invalid/v1",
            "nyro-gateway",
        ),
    ] {
        providers.push(
            storage
                .providers()
                .create(serde_json::from_value(json!({
                    "name": name, "channel": channel, "protocol": "openai-compatible",
                    "base_url": base_url, "vendor": vendor, "api_key": ""
                }))?)
                .await?,
        );
    }
    let [relay, disabled, lookalike, nyro] = providers.as_slice() else {
        unreachable!()
    };
    storage
        .providers()
        .update(
            &disabled.id,
            UpdateProvider {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    admin
        .create_model(serde_json::from_value(json!({
            "name": "local-relay-model", "target_provider": relay.id, "target_model": "wire-usd",
            "enable_auth": false
        }))?)
        .await?;
    let old_scheduling = gw.quota_registry.observe(&relay.id, &[], Some(false));
    let old_snapshot = snapshot(&relay.id, "old-vendor", 1, Vec::new());
    gw.usage_snapshots.store(&old_snapshot);

    for provider in [relay, disabled] {
        let usage = admin.get_provider_usage(&provider.id).await?;
        assert_eq!(usage.provider_id, provider.id);
        assert_eq!(usage.kind, "sub2api");
        assert!(usage.tiers.is_empty());
        assert!(usage.quota_buckets.is_empty());
        assert!(usage.balances.is_empty());
        assert!(usage.spends.is_empty());
        assert_eq!(usage.scheduling, ProviderScheduling::default());
        assert!(usage.upstream.is_none());
        let price = usage.api_price.unwrap();
        let queried_at = ms(&usage.queried_at);
        assert_eq!(price.period_tier.as_deref(), Some("calendar_month"));
        assert!(
            price.period_start.unwrap() <= queried_at && queried_at < price.period_end.unwrap()
        );
        assert_eq!(price.all_time.request_count, 0);
        assert_eq!(price.period.unwrap().request_count, 0);
    }

    let rows = admin.list_provider_usage().await?;
    for (provider, enabled) in [(relay, true), (disabled, false)] {
        let row = rows
            .iter()
            .find(|row| row.provider_id == provider.id)
            .unwrap();
        assert_eq!(row.status, ProviderUsageQueryStatus::Ok);
        assert_eq!(row.is_enabled, enabled);
        let usage = row.usage.as_ref().unwrap();
        assert_eq!(usage.kind, "sub2api");
        assert!(usage.api_price.is_some());
    }
    let row = rows
        .iter()
        .find(|row| row.provider_id == lookalike.id)
        .unwrap();
    assert_eq!(row.status, ProviderUsageQueryStatus::Unsupported);
    assert!(row.usage.is_none());
    let row = rows.iter().find(|row| row.provider_id == nyro.id).unwrap();
    assert_eq!(row.status, ProviderUsageQueryStatus::Error);
    assert_eq!(row.error.as_deref(), Some("provider api key is empty"));

    let published = published_usage_items(rows);
    let usage = published
        .iter()
        .find(|row| row.provider_id == relay.id)
        .and_then(|row| row.usage.as_ref())
        .unwrap();
    assert_eq!(usage.kind, "sub2api");
    assert!(usage.api_price.is_some());

    for access in [RouteAccess::default(), RouteAccess::privileged()] {
        let report = admin.forwarded_provider_usage(&access).await?;
        assert_eq!(report.providers.len(), 1);
        let row = &report.providers[0];
        assert_eq!(row.id, relay.id);
        assert_eq!(row.status, ProviderUsageQueryStatus::Unsupported);
        assert!(row.usage.is_none() && row.usage_entry.is_none());
    }
    assert_eq!(gw.quota_registry.snapshot(&relay.id), old_scheduling);
    assert_eq!(
        gw.quota_registry.snapshot(&disabled.id),
        ProviderScheduling::default()
    );
    let max_age = Duration::from_secs(60);
    assert_eq!(
        gw.usage_snapshots.fresh(&relay.id, max_age),
        Some(old_snapshot)
    );
    assert!(gw.usage_snapshots.fresh(&disabled.id, max_age).is_none());
    Ok(())
}
