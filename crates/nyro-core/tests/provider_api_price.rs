//! Public full-admin usage queries with local Grok quota responses; no vendor
//! traffic or guessed model prices. Shared cycle validation is tested internally.
use std::sync::{Arc, RwLock};

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use chrono::{Duration, Utc};
use nyro_core::db::models::{CreateModel, CreateProvider, UpsertOAuthCredential};
use nyro_core::storage::{SqliteStorage, Storage};
use nyro_core::{
    Gateway,
    admin::{TokenPriceInput, TokenPriceRates},
    config::GatewayConfig,
};
use serde_json::{Value, json};

struct BillingServer {
    body: Arc<RwLock<Value>>,
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for BillingServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn billing(
    State(body): State<Arc<RwLock<Value>>>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let body = body.read().unwrap();
    let key = if query
        .get("format")
        .is_some_and(|format| format == "credits")
    {
        "weekly"
    } else {
        "monthly"
    };
    Json(match body.get(key) {
        Some(config) => json!({"config": config}),
        None => json!({}),
    })
}

async fn server(body: Value) -> anyhow::Result<BillingServer> {
    let body = Arc::new(RwLock::new(body));
    let app = Router::new()
        .route("/v1/billing", get(billing))
        .with_state(body.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    Ok(BillingServer { body, url, task })
}

async fn provider(storage: &SqliteStorage, url: &str) -> anyhow::Result<String> {
    let provider = storage
        .providers()
        .create(CreateProvider {
            name: "Grok pricing fixture".into(),
            vendor: Some("xai".into()),
            protocol: "openai-responses/responses/v1".into(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: Some("xai".into()),
            channel: Some("grok".into()),
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "grok".into(),
                scheme: "oauth_pkce".into(),
                access_token: "synthetic-local-token".into(),
                resource_url: Some(url.into()),
                ..Default::default()
            },
        )
        .await?;
    Ok(provider.id)
}

async fn attempt(
    storage: &SqliteStorage,
    provider: &str,
    model: &str,
    at: i64,
    tokens: (Option<i64>, Option<i64>, Option<i64>),
    index: i32,
) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, client_model, upstream_model, route_decision, input_tokens, output_tokens, cache_read_tokens, client_request_id, attempt_index, outcome_version, attempt_outcome, upstream_status_code) VALUES (?, ?, ?, 'client-alias', ?, NULL, ?, ?, ?, 'one-request', ?, 1, ?, ?)")
        .bind(uuid::Uuid::new_v4().to_string()).bind(at).bind(provider).bind(model)
        .bind(tokens.0).bind(tokens.1).bind(tokens.2).bind(index)
        .bind(if index == 0 { "failed" } else { "completed" })
        .bind(if index == 0 { 502 } else { 200 }).execute(storage.pool()).await?;
    Ok(())
}

async fn price(
    gw: &Gateway,
    provider: &str,
    model: &str,
    currency: &str,
    rates: (f64, f64, f64),
) -> anyhow::Result<()> {
    gw.admin()
        .set_token_price(TokenPriceInput {
            provider_id: provider.into(),
            model: model.into(),
            price: Some(TokenPriceRates {
                currency: currency.into(),
                input_rate: rates.0,
                output_rate: rates.1,
                cache_read_rate: Some(rates.2),
            }),
        })
        .await?;
    Ok(())
}

#[tokio::test]
async fn provider_api_price_public_usage_retained_prices_cycles_projection_and_failure_isolation()
-> anyhow::Result<()> {
    let now = Utc::now();
    let reset = (now + Duration::days(2)).date_naive().to_string();
    let end = chrono::NaiveDate::parse_from_str(&reset, "%Y-%m-%d")?
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_utc();
    let start = (end - Duration::days(7)).timestamp_millis();
    let billing = server(json!({
        "weekly": {"currentPeriod": {"type": "weekly", "end": reset}, "creditUsagePercent": 25},
        "monthly": {"monthlyLimit": {"val": 15000}, "used": {"val": 3000}, "billingPeriodEnd": (now + Duration::days(15)).to_rfc3339()}
    })).await?;
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let id = provider(&storage, &billing.url).await?;
    // Manual prices match exact upstream identities, not client-facing aliases.
    price(&gw, &id, "client-alias", "USD", (999.0, 999.0, 999.0)).await?;
    price(&gw, &id, "priced-usd", "USD", (2.0, 8.0, 1.0)).await?;
    price(&gw, &id, "priced-cny", "CNY", (7.0, 14.0, 3.0)).await?;
    let known = (Some(1_000_000), Some(500_000), Some(250_000));
    for (model, at, tokens, index) in [
        ("priced-usd", 1, known, 0), // Far older retained attempts are included.
        ("priced-usd", start - 1, known, 0),
        ("priced-usd", start, known, 0), // Failed and retried upstream attempts both count.
        ("priced-usd", start + 1, known, 1),
        ("priced-cny", start + 2, known, 1),
        ("no-reference", start + 3, (Some(0), Some(0), None), 1),
        ("priced-usd", start + 4, (None, None, None), 1),
        ("priced-usd", end.timestamp_millis(), known, 1),
        ("priced-usd", i64::MAX, known, 1),
    ] {
        attempt(&storage, &id, model, at, tokens, index).await?;
    }
    attempt(&storage, "another-provider", "priced-usd", start, known, 0).await?;
    sqlx::query("INSERT INTO request_results (client_request_id, final_outcome, attempt_count, finished_at) VALUES ('one-request', 'completed', 5, ?)")
        .bind(now.timestamp_millis()).execute(storage.pool()).await?;
    let credentials_before =
        serde_json::to_value(storage.oauth_credentials().list_accounts(&id).await?)?;
    let usage = gw.admin().get_provider_usage(&id).await?;
    let stats = usage.api_price.as_ref().unwrap();
    assert_eq!(
        (stats.period_start, stats.period_end),
        (Some(start), Some(end.timestamp_millis()))
    );
    assert_eq!(stats.period_tier.as_deref(), Some("weekly_limit"));
    assert_eq!(stats.used_percent, Some(25.0));
    assert_eq!(
        (stats.all_time.request_count, stats.all_time.total_tokens),
        (7, 7_500_000)
    );
    assert_eq!(stats.all_time.unpriced_request_count, 1);
    let period = stats.period.as_ref().unwrap();
    assert_eq!(
        (
            period.request_count,
            period.total_tokens,
            period.unpriced_request_count
        ),
        (5, 4_500_000, 1)
    );
    assert_eq!(
        period
            .equivalent_costs
            .iter()
            .map(|c| (c.currency.as_str(), c.total_cost, c.priced_request_count))
            .collect::<Vec<_>>(),
        [("CNY", 13.0, 1), ("USD", 11.5, 3)]
    );
    assert_eq!(
        stats
            .estimated_capacity
            .as_ref()
            .unwrap()
            .iter()
            .map(|c| (c.currency.as_str(), c.total_cost, c.priced_request_count))
            .collect::<Vec<_>>(),
        [("CNY", 52.0, 1), ("USD", 46.0, 3)]
    );
    assert_eq!(
        serde_json::to_value(storage.oauth_credentials().list_accounts(&id).await?)?,
        credentials_before
    );
    assert!(
        chrono::DateTime::parse_from_rfc3339(&usage.queried_at)?.timestamp_millis()
            < end.timestamp_millis()
    );
    // Unknown/expired weekly cycles preserve retained totals, never fall back to monthly.
    for reset in [
        Value::Null,
        json!("bad"),
        json!((now - Duration::days(1)).to_rfc3339()),
    ] {
        *billing.body.write().unwrap() = json!({
            "weekly": {"currentPeriod": {"end": reset}, "creditUsagePercent": 25},
            "monthly": {"monthlyLimit": {"val": 15000}, "used": 3000, "billingPeriodEnd": (now + Duration::days(15)).to_rfc3339()}
        });
        let next = gw.admin().get_provider_usage(&id).await?.api_price.unwrap();
        assert_eq!(next.all_time, stats.all_time);
        assert!(
            next.period.is_none()
                && next.period_tier.is_none()
                && next.estimated_capacity.is_none()
        );
    }
    // Forwarding omits admin statistics even for privileged proxy callers.
    gw.admin()
        .create_model(CreateModel {
            name: "route".into(),
            balance: Some("priority".into()),
            target_provider: id.clone(),
            target_model: "priced-usd".into(),
            targets: vec![],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    let forwarded = gw
        .admin()
        .forwarded_provider_usage(&nyro_core::admin::RouteAccess::privileged())
        .await?;
    assert!(
        forwarded.providers[0]
            .usage
            .as_ref()
            .unwrap()
            .api_price
            .is_none()
    );
    // Corrupt retained-stat settings cannot fail quota observation/scheduling.
    storage
        .settings()
        .set("token_price.v1.corrupt", "not-json")
        .await?;
    *billing.body.write().unwrap() =
        json!({"weekly": {"currentPeriod": {"end": end.to_rfc3339()}, "creditUsagePercent": 100}});
    let quota_only = gw.admin().get_provider_usage(&id).await?;
    assert!(quota_only.api_price.is_none());
    assert_eq!(
        quota_only.scheduling.status,
        nyro_core::admin::ProviderSchedulingStatus::QuotaExhausted
    );
    assert!(
        serde_json::to_value(&quota_only)?
            .get("api_price")
            .is_none()
    );
    sqlx::query("DELETE FROM settings WHERE name = 'token_price.v1.corrupt'")
        .execute(storage.pool())
        .await?;
    storage.logs().clear_all().await?;
    let cleared = gw.admin().get_provider_usage(&id).await?.api_price.unwrap();
    assert_eq!(cleared.all_time.request_count, 0);
    assert_eq!(cleared.period.unwrap().request_count, 0);
    assert!(cleared.estimated_capacity.is_none());
    Ok(())
}

#[tokio::test]
async fn provider_api_price_public_monthly_fallback_and_priced_zero_are_distinct_from_unpriced_zero()
-> anyhow::Result<()> {
    let now = Utc::now();
    let end = now + Duration::days(1);
    let start = end.checked_sub_months(chrono::Months::new(1)).unwrap();
    let billing = server(json!({"monthly": {
        "monthlyLimit": {"val": 15000}, "used": {"val": 3750}, "billingPeriodEnd": end.to_rfc3339()
    }}))
    .await?;
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let id = provider(&storage, &billing.url).await?;
    attempt(
        &storage,
        &id,
        "unpriced",
        now.timestamp_millis() - 1,
        (Some(0), Some(0), Some(0)),
        1,
    )
    .await?;
    let unpriced = gw.admin().get_provider_usage(&id).await?.api_price.unwrap();
    assert_eq!(unpriced.period_tier.as_deref(), Some("monthly"));
    assert_eq!(
        (unpriced.period_start, unpriced.period_end),
        (Some(start.timestamp_millis()), Some(end.timestamp_millis()))
    );
    assert_eq!(unpriced.period.as_ref().unwrap().unpriced_request_count, 1);
    assert!(
        unpriced
            .period
            .as_ref()
            .unwrap()
            .equivalent_costs
            .is_empty()
    );
    assert!(unpriced.estimated_capacity.is_none());
    price(&gw, &id, "unpriced", "USD", (0.0, 0.0, 0.0)).await?;
    let priced = gw.admin().get_provider_usage(&id).await?.api_price.unwrap();
    assert_eq!(priced.period.as_ref().unwrap().unpriced_request_count, 0);
    let capacity = priced.estimated_capacity.unwrap();
    assert_eq!(
        (capacity[0].total_cost, capacity[0].priced_request_count),
        (0.0, 1)
    );
    // Zero utilization still permits actual cycle totals, but not projection.
    billing.body.write().unwrap()["monthly"]["used"] = json!(0);
    let zero_usage = gw.admin().get_provider_usage(&id).await?.api_price.unwrap();
    assert_eq!(zero_usage.used_percent, Some(0.0));
    assert!(zero_usage.period.is_some() && zero_usage.estimated_capacity.is_none());
    Ok(())
}
