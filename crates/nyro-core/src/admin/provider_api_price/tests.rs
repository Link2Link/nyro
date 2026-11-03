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
    ] {
        let period = provider_period(kind, &tiers, now).unwrap();
        assert_eq!(
            (period.start, period.end),
            (ms("2026-10-30T17:59:59+08:00"), ms(end))
        );
        assert_eq!(
            (period.tier.name.as_str(), period.tier.used_percent),
            ("weekly_limit", 4.0)
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
    assert_eq!(period.tier.used_percent, 76.0);

    let short = tier("five_hour", Some("2026-11-03T13:29:00+08:00"), 20.0);
    let tiers = [
        tier("feature:codex_bengalfox:weekly_limit", Some(reset), 99.0),
        tier("primary_window", Some(reset), 50.0),
        short.clone(),
    ];
    let period = provider_period("openai_codex", &tiers, now).unwrap();
    assert_eq!(period.tier.name, "five_hour");
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
fn provider_api_price_five_hour_and_grok_utc_date_only_month_clamping() {
    for kind in ["glm_coding_plan", "minimax_coding_plan", "openai_codex"] {
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
        assert_eq!(period.tier.name, "monthly");
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
    let mut usage = snapshot(
        &provider.id,
        "glm_coding_plan",
        now,
        vec![tier("weekly_limit", Some("2026-11-06T00:00:00Z"), 100.0)],
    );
    let before = usage.clone();
    admin.enrich_provider_api_price(&mut usage).await;
    assert_eq!(usage, before);
    assert!(usage.api_price.is_none());
    Ok(())
}
