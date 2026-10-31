use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{TokenPriceInput, TokenPriceRates};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::{
    CreateProvider, OAuthAccountUpdate, OAuthCredential, UpsertOAuthCredential,
};
use nyro_core::storage::{SqliteStorage, Storage};
use serde_json::json;

async fn provider(storage: &SqliteStorage, vendor: &str) -> anyhow::Result<String> {
    Ok(storage
        .providers()
        .create(CreateProvider {
            name: format!("{vendor} test pool"),
            vendor: Some(
                if vendor == "claude_code" {
                    "anthropic"
                } else {
                    vendor
                }
                .into(),
            ),
            protocol: "openai/chat".into(),
            base_url: "https://example.invalid".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: (vendor == "claude_code").then(|| "claude-code".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

async fn log(
    storage: &SqliteStorage,
    provider: &str,
    account: Option<&str>,
    model: &str,
    at: i64,
) -> anyhow::Result<()> {
    let decision = account.map(|id| {
        json!({"account": {
            "account_id": id, "label": "Old label", "credential_version": 1
        }})
        .to_string()
    });
    // Includes known usage even on failed attempts. Gross input includes the cache.
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, upstream_model, route_decision, input_tokens, output_tokens, cache_read_tokens, outcome_version, attempt_outcome, client_status_code) VALUES (?, ?, ?, ?, ?, 1000000, 500000, 250000, 1, 'failed', 502)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(at).bind(provider).bind(model)
        .bind(decision).execute(storage.pool()).await?;
    Ok(())
}

async fn set_state(
    storage: &SqliteStorage,
    account: &OAuthCredential,
    state: serde_json::Value,
) -> anyhow::Result<()> {
    assert!(
        storage
            .oauth_credentials()
            .compare_account_state(
                &account.provider_id,
                &account.account_id,
                account.status_version,
                &account.account_state,
                &state.to_string(),
            )
            .await?
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_all_time_and_period_reuse_prices_without_mixing_accounts()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    let id = provider(&storage, "devin").await?;
    let mut accounts = Vec::new();
    for n in 0..3 {
        accounts.push(
            storage
                .oauth_credentials()
                .create_account(
                    &id,
                    "Same label",
                    UpsertOAuthCredential {
                        driver_key: "devin".into(),
                        scheme: "session_token".into(),
                        access_token: format!("test-account-{n}"),
                        ..Default::default()
                    },
                )
                .await?,
        );
    }
    let a = &accounts[0].account_id;
    let b = &accounts[1].account_id;
    let empty = &accounts[2].account_id;
    let start = (chrono::Utc::now().timestamp() - 86_400) * 1000;
    let end = start + 7 * 86_400_000;
    for (index, account) in accounts.iter().enumerate() {
        // Each account uses its own reset, even with identical labels.
        let reset = if index == 1 { end + 1000 } else { end };
        set_state(&storage, account, json!({"usage": {
            "plan_name": "Pro", "weekly": {"remaining_percent": 80, "resets_at_unix": reset / 1000}
        }})).await?;
    }
    // Far older than a year is still included if retained; no hidden 24h/year window.
    log(&storage, &id, Some(a), "priced-usd", 1).await?;
    log(&storage, &id, Some(a), "priced-usd", start).await?;
    log(&storage, &id, Some(a), "priced-cny", start + 1).await?;
    log(&storage, &id, Some(a), "no-reference-price", start + 2).await?;
    log(&storage, &id, Some(b), "priced-usd", start - 1).await?;
    log(&storage, &id, Some(b), "priced-usd", start + 500).await?;
    log(&storage, &id, Some(b), "priced-usd", start + 1000).await?;
    log(&storage, &id, None, "priced-usd", start).await?;
    log(&storage, &id, Some("removed-account"), "priced-usd", start).await?;
    log(&storage, "another-provider", Some(a), "priced-usd", start).await?;
    log(&storage, &id, Some(a), "priced-usd", i64::MAX).await?;
    for (model, currency, input_rate, output_rate, cache_read_rate) in [
        ("priced-usd", "USD", 2.0, 8.0, 1.0),
        ("priced-cny", "CNY", 7.0, 14.0, 3.0),
    ] {
        admin
            .set_token_price(TokenPriceInput {
                provider_id: id.clone(),
                model: model.into(),
                price: Some(TokenPriceRates {
                    currency: currency.into(),
                    input_rate,
                    output_rate,
                    cache_read_rate: Some(cache_read_rate),
                }),
            })
            .await?;
    }
    // A rename, version increment and disabled scheduling never reset usage.
    storage
        .oauth_credentials()
        .update_account(
            &id,
            a,
            accounts[0].status_version,
            OAuthAccountUpdate {
                label: Some("Renamed account".into()),
                is_enabled: Some(false),
                priority: None,
            },
        )
        .await?;
    let before = serde_json::to_value(storage.oauth_credentials().list_accounts(&id).await?)?;
    let result = admin.get_provider_account_usage_stats(&id).await?;
    assert_eq!(result.len(), 3);
    let stats = result.iter().find(|s| s.account_id == *a).unwrap();
    assert_eq!(stats.all_time.request_count, 4);
    assert_eq!(
        stats.all_time.total_tokens, 6_000_000,
        "cache is not counted twice"
    );
    assert_eq!(stats.period_start, Some(start));
    assert_eq!(stats.period_end, Some(end));
    assert_eq!(stats.period.as_ref().unwrap().request_count, 3);
    assert_eq!(stats.period.as_ref().unwrap().total_tokens, 4_500_000);
    assert_eq!(stats.all_time.unpriced_request_count, 1);
    assert_eq!(stats.period.as_ref().unwrap().unpriced_request_count, 1);
    let usd = stats
        .all_time
        .equivalent_costs
        .iter()
        .find(|c| c.currency == "USD")
        .unwrap();
    assert_eq!(usd.priced_request_count, 2);
    assert!((usd.total_cost - 11.5).abs() < 1e-9);
    assert_eq!(
        stats.all_time.equivalent_costs.len(),
        2,
        "native currencies stay separate"
    );
    assert_eq!(
        stats
            .period
            .as_ref()
            .unwrap()
            .equivalent_costs
            .iter()
            .find(|c| c.currency == "USD")
            .unwrap()
            .total_cost,
        5.75
    );
    assert_eq!(
        stats
            .period
            .as_ref()
            .unwrap()
            .equivalent_costs
            .iter()
            .find(|c| c.currency == "CNY")
            .unwrap()
            .total_cost,
        13.0
    );
    let other = result.iter().find(|s| s.account_id == *b).unwrap();
    assert_eq!(other.all_time.total_tokens, 4_500_000);
    assert_eq!(other.period_start, Some(start + 1000));
    assert_eq!(other.period_end, Some(end + 1000));
    assert_eq!(other.period.as_ref().unwrap().total_tokens, 1_500_000);
    assert_eq!(
        other.period.as_ref().unwrap().equivalent_costs[0].total_cost,
        5.75
    );
    let no_logs = result.iter().find(|s| s.account_id == *empty).unwrap();
    assert_eq!(no_logs.all_time.request_count, 0);
    assert_eq!(no_logs.all_time.total_tokens, 0);
    assert!(no_logs.all_time.equivalent_costs.is_empty());
    assert_eq!(no_logs.period.as_ref().unwrap().total_tokens, 0);
    assert_eq!(
        before,
        serde_json::to_value(storage.oauth_credentials().list_accounts(&id).await?)?,
        "reading statistics must not mutate credentials/quota"
    );
    let wire = serde_json::to_value(stats)?;
    assert_eq!(wire["period"]["total_tokens"], 4_500_000);
    assert!(wire.get("access_token").is_none());
    // Deleted logs no longer contribute; this is not a permanent accounting ledger.
    storage.logs().clear_all().await?;
    assert!(
        admin
            .get_provider_account_usage_stats(&id)
            .await?
            .iter()
            .all(|s| s.all_time.total_tokens == 0 && s.period.as_ref().unwrap().total_tokens == 0)
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_kiro_unpriced_is_not_a_priced_zero() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let id = provider(&storage, "kiro").await?;
    let account = storage
        .oauth_credentials()
        .create_account(
            &id,
            "Kiro",
            UpsertOAuthCredential {
                driver_key: "kiro".into(),
                scheme: "bearer".into(),
                access_token: "test-kiro-token".into(),
                ..Default::default()
            },
        )
        .await?;
    let reset = chrono::Utc::now() + chrono::Duration::days(1);
    set_state(
        &storage,
        &account,
        json!({"usage": {
            "resets_at": reset.to_rfc3339(), "overage_enabled": false, "items": []
        }}),
    )
    .await?;
    log(
        &storage,
        &id,
        Some(&account.account_id),
        "unknown-model",
        chrono::Utc::now().timestamp_millis() - 1,
    )
    .await?;
    let stats = gw.admin().get_provider_account_usage_stats(&id).await?;
    assert_eq!(stats[0].all_time.total_tokens, 1_500_000);
    assert_eq!(stats[0].period.as_ref().unwrap().unpriced_request_count, 1);
    assert!(
        stats[0]
            .period
            .as_ref()
            .unwrap()
            .equivalent_costs
            .is_empty()
    );
    assert!(
        gw.admin()
            .get_provider_account_usage_stats("missing")
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn account_usage_claude_and_monthly_cycles_filter_tokens_and_prices() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let admin = gw.admin();
    let now = chrono::Utc::now();
    let reset = now + chrono::Duration::days(1);
    for (vendor, driver) in [("claude_code", "claude-code"), ("kiro", "kiro")] {
        let id = provider(&storage, vendor).await?;
        let account = storage
            .oauth_credentials()
            .create_account(
                &id,
                "Account",
                UpsertOAuthCredential {
                    driver_key: driver.into(),
                    scheme: "bearer".into(),
                    access_token: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                },
            )
            .await?;
        let start = if vendor == "kiro" {
            reset.checked_sub_months(chrono::Months::new(1)).unwrap()
        } else {
            reset - chrono::Duration::days(7)
        }
        .timestamp_millis();
        let usage = if vendor == "kiro" {
            json!({"resets_at": reset.to_rfc3339(), "overage_enabled": false, "items": []})
        } else {
            json!({"windows": [
                {"name": "weekly_limit", "used_percent": 10, "resets_at": reset.to_rfc3339()},
                {"name": "five_hour", "used_percent": 0, "resets_at": (now + chrono::Duration::hours(2)).to_rfc3339()}
            ]})
        };
        set_state(&storage, &account, json!({"usage": usage})).await?;
        for at in [
            start - 1,
            start,
            now.timestamp_millis() - 1,
            reset.timestamp_millis(),
            reset.timestamp_millis() + 1,
        ] {
            log(&storage, &id, Some(&account.account_id), "priced", at).await?;
        }
        admin
            .set_token_price(TokenPriceInput {
                provider_id: id.clone(),
                model: "priced".into(),
                price: Some(TokenPriceRates {
                    currency: "USD".into(),
                    input_rate: 2.0,
                    output_rate: 8.0,
                    cache_read_rate: Some(1.0),
                }),
            })
            .await?;
        let stats = admin.get_provider_account_usage_stats(&id).await?;
        let stats = &stats[0];
        assert_eq!(stats.period_start, Some(start));
        assert_eq!(stats.period_end, Some(reset.timestamp_millis()));
        assert_eq!(stats.all_time.total_tokens, 4_500_000);
        let period = stats.period.as_ref().unwrap();
        assert_eq!(period.request_count, 2);
        assert_eq!(period.total_tokens, 3_000_000);
        assert_eq!(period.equivalent_costs[0].total_cost, 11.5);
    }
    Ok(())
}

#[tokio::test]
async fn account_usage_unknown_cycle_preserves_all_time_without_a_fake_zero() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let id = provider(&storage, "claude_code").await?;
    for reset in [
        None,
        Some("invalid".to_string()),
        Some(chrono::Utc::now().to_rfc3339()),
    ] {
        let account = storage
            .oauth_credentials()
            .create_account(
                &id,
                "Account",
                UpsertOAuthCredential {
                    driver_key: "claude-code".into(),
                    scheme: "bearer".into(),
                    access_token: uuid::Uuid::new_v4().to_string(),
                    ..Default::default()
                },
            )
            .await?;
        set_state(
            &storage,
            &account,
            json!({"usage": {"windows": [
                {"name": "weekly_limit", "used_percent": 0, "resets_at": reset}
            ]}}),
        )
        .await?;
        log(&storage, &id, Some(&account.account_id), "unknown", 1).await?;
    }
    let stats = gw.admin().get_provider_account_usage_stats(&id).await?;
    assert_eq!(stats.len(), 3);
    for stats in stats {
        assert_eq!(stats.all_time.total_tokens, 1_500_000);
        assert!(stats.period.is_none());
        assert!(stats.period_start.is_none());
        assert!(stats.period_end.is_none());
        let wire = serde_json::to_value(stats)?;
        for field in ["period", "period_start", "period_end"] {
            assert!(wire[field].is_null());
        }
    }
    Ok(())
}
