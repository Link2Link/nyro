use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{TokenPriceError, TokenPriceInput, TokenPriceRates};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::{CreateProvider, TokenUsageCost};
use nyro_core::storage::{SqliteStorage, Storage};
use serde_json::json;

fn rates(input: f64, output: f64, cache: Option<f64>) -> TokenPriceRates {
    TokenPriceRates {
        currency: "USD".into(),
        input_rate: input,
        output_rate: output,
        cache_read_rate: cache,
    }
}

fn input(provider: &str, model: &str, price: Option<TokenPriceRates>) -> TokenPriceInput {
    TokenPriceInput {
        provider_id: provider.into(),
        model: model.into(),
        price,
    }
}

async fn open(dir: &std::path::Path) -> anyhow::Result<(Gateway, SqliteStorage)> {
    let config = GatewayConfig {
        data_dir: dir.into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gateway, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    Ok((gateway, storage))
}

async fn provider(storage: &SqliteStorage, name: &str, url: &str) -> anyhow::Result<String> {
    Ok(storage
        .providers()
        .create(CreateProvider {
            name: name.into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test-only".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

#[tokio::test]
async fn token_price_default_save_reset_and_restart_persistence() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let id = provider(&storage, "BigModel", "https://open.bigmodel.cn/api/paas/v4").await?;
    let admin = gw.admin();
    let automatic = admin.get_token_price(&id, "glm-5.3").await?;
    assert!(automatic.override_price.is_none());
    let reference = automatic.default_price.clone().unwrap();
    assert_eq!(reference.rates.currency, "CNY");
    assert_eq!(reference.pricing_provider, "zhipuai");
    let saved = admin
        .set_token_price(input(&id, "glm-5.3", Some(rates(2.0, 8.0, None))))
        .await?;
    assert_eq!(saved.default_price, automatic.default_price);
    assert_eq!(saved.override_price, Some(rates(2.0, 8.0, None)));
    let json = serde_json::to_value(&saved)?;
    assert_eq!(json["default_price"]["currency"], "CNY");
    assert!(
        json["default_price"].get("rates").is_none(),
        "reference JSON is flat"
    );
    assert_eq!(
        serde_json::from_value::<nyro_core::admin::TokenPriceConfig>(json)?,
        saved
    );
    admin
        .set_token_price(input(
            "historical",
            "unlisted",
            Some(rates(0.0, 0.0, Some(0.0))),
        ))
        .await?;
    // A second gateway/storage instance sees both writes without an in-memory reload.
    let (other, _) = open(dir.path()).await?;
    assert_eq!(other.admin().get_token_price(&id, "glm-5.3").await?, saved);
    drop(other);
    drop(admin);
    drop(gw);
    storage.pool().close().await;
    drop(storage);
    let (gw, storage) = open(dir.path()).await?;
    let admin = gw.admin();
    assert_eq!(admin.get_token_price(&id, "glm-5.3").await?, saved);
    assert_eq!(
        admin
            .get_token_price("historical", "unlisted")
            .await?
            .override_price,
        Some(rates(0.0, 0.0, Some(0.0)))
    );
    for _ in 0..2 {
        assert_eq!(
            admin.set_token_price(input(&id, "glm-5.3", None)).await?,
            automatic
        );
    }
    drop(admin);
    drop(gw);
    storage.pool().close().await;
    let (gw, _) = open(dir.path()).await?;
    assert_eq!(gw.admin().get_token_price(&id, "glm-5.3").await?, automatic);
    assert!(
        gw.admin()
            .get_token_price("historical", "unlisted")
            .await?
            .override_price
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn token_price_exact_pair_isolation_and_concurrent_writes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let pairs = [
        ("p", "Model"),
        ("P", "Model"),
        ("p", "model"),
        ("p", "Model "),
        ("p/Model", "x"),
        ("p", "Model/x"),
        ("p:+# /模型\0", "m:+# /模型\0"),
    ];
    let futures = pairs.iter().enumerate().map(|(i, (p, m))| {
        let admin = gw.admin();
        async move {
            admin
                .set_token_price(input(p, m, Some(rates(i as f64, 0.0, None))))
                .await
        }
    });
    for result in futures::future::join_all(futures).await {
        result?;
    }
    for (i, (p, m)) in pairs.iter().enumerate() {
        let result = gw.admin().get_token_price(p, m).await?;
        assert_eq!(result.provider_id, *p);
        assert_eq!(result.model, *m);
        assert!(result.default_price.is_none());
        assert_eq!(result.override_price.unwrap().input_rate, i as f64);
    }
    let settings = storage.settings().list_all().await?;
    let keys: Vec<_> = settings
        .iter()
        .filter(|(key, _)| key.starts_with("token_price.v1."))
        .collect();
    assert_eq!(keys.len(), pairs.len());
    assert!(keys.iter().all(|(key, _)| key.len() == 79));
    gw.admin()
        .set_token_price(input("p", "Model", None))
        .await?;
    assert!(
        gw.admin()
            .get_token_price("P", "Model")
            .await?
            .override_price
            .is_some()
    );
    assert!(
        gw.admin()
            .get_token_price("p", "model")
            .await?
            .override_price
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn token_price_inheritance_config_stats_own_override_reset_and_deleted_provider()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let id = provider(&storage, "BigModel", "https://open.bigmodel.cn/api/paas/v4").await?;
    let other_id = provider(&storage, "Other", "http://127.0.0.1:1/v1").await?;
    let admin = gw.admin();
    let base = "glm-5.3";
    let longer = "glm-5.3-flash";
    let variant = "glm-5.3-flash-medium";
    for model in [
        variant,
        "glm-5.3_high",
        "glm-5.3:high",
        "glm-5.3/high",
        "GLM-5.3-medium",
        "glm-5.30",
        "glm-5.3.1",
    ] {
        log(&storage, &id, model, 0).await?;
    }
    log(&storage, &other_id, variant, 0).await?;
    admin
        .set_token_price(input(&id, base, Some(rates(1.0, 3.0, None))))
        .await?;
    admin
        .set_token_price(input(&id, longer, Some(rates(2.0, 8.0, None))))
        .await?;
    let inherited = admin.get_token_price(&id, variant).await?;
    assert!(inherited.override_price.is_none());
    let parent = inherited.inherited_price.as_ref().unwrap();
    assert_eq!(parent.pricing_model, longer);
    assert_eq!(parent.pricing_provider, "manual");
    assert_eq!(parent.rates, rates(2.0, 8.0, None));
    assert_eq!(
        inherited.default_price.as_ref().unwrap().rates.currency,
        "CNY"
    );
    assert_eq!(
        inherited.default_price.as_ref().unwrap().rates.input_rate,
        0.8
    );
    for suffix in ["_high", ":high", "/high"] {
        assert_eq!(
            admin
                .get_token_price(&id, &format!("{base}{suffix}"))
                .await?
                .inherited_price
                .unwrap()
                .pricing_model,
            base
        );
    }
    for model in ["GLM-5.3-medium", "glm-5.30", "glm-5.3.1", "glm-5.3+high"] {
        assert!(
            admin
                .get_token_price(&id, model)
                .await?
                .inherited_price
                .is_none()
        );
    }
    assert!(
        admin
            .get_token_price(&other_id, variant)
            .await?
            .inherited_price
            .is_none()
    );
    let stats = admin.get_token_usage_stats(2024).await?;
    let cost = stats
        .rows
        .iter()
        .find(|row| row.provider_id == id && row.model == variant)
        .unwrap()
        .cost
        .as_ref()
        .unwrap();
    assert!(cost.is_manual);
    assert_eq!(cost.pricing_model, longer);
    assert_eq!(cost.total_cost, 6.0);
    assert!(cost.cache_rate_fallback);
    assert!(
        stats
            .rows
            .iter()
            .find(|row| row.provider_id == other_id)
            .unwrap()
            .cost
            .is_none()
    );
    assert!(
        !stats
            .rows
            .iter()
            .find(|row| row.model == "GLM-5.3-medium")
            .unwrap()
            .cost
            .as_ref()
            .unwrap()
            .is_manual
    );
    // Saving an own zero-rate override still reports both alternative references.
    let own = rates(0.0, 0.0, Some(0.0));
    let saved = admin
        .set_token_price(input(&id, variant, Some(own.clone())))
        .await?;
    assert_eq!(saved.override_price, Some(own));
    assert_eq!(saved.inherited_price, inherited.inherited_price);
    assert_eq!(saved.default_price, inherited.default_price);
    assert_eq!(admin.get_token_price(&id, variant).await?, saved);
    let stats = admin.get_token_usage_stats(2024).await?;
    let cost = stats
        .rows
        .iter()
        .find(|row| row.provider_id == id && row.model == variant)
        .unwrap()
        .cost
        .as_ref()
        .unwrap();
    assert!(cost.is_manual);
    assert_eq!(cost.pricing_model, variant);
    assert_eq!(cost.total_cost, 0.0);
    assert!(!cost.cache_rate_fallback);
    // Exact reset reveals the parent; it never clears the shared base rule.
    assert_eq!(
        admin.set_token_price(input(&id, variant, None)).await?,
        inherited
    );
    assert!(
        admin
            .get_token_price(&id, longer)
            .await?
            .override_price
            .is_some()
    );
    storage.providers().delete(&id).await?;
    let deleted = admin.get_token_price(&id, variant).await?;
    assert!(deleted.default_price.is_none());
    assert_eq!(deleted.inherited_price, inherited.inherited_price);
    let stats = admin.get_token_usage_stats(2024).await?;
    assert_eq!(
        stats
            .rows
            .iter()
            .find(|row| row.provider_id == id && row.model == variant)
            .unwrap()
            .cost
            .as_ref()
            .unwrap()
            .pricing_model,
        longer
    );
    admin.set_token_price(input(&id, longer, None)).await?;
    assert_eq!(
        admin
            .get_token_price(&id, variant)
            .await?
            .inherited_price
            .unwrap()
            .pricing_model,
        base
    );
    admin.set_token_price(input(&id, base, None)).await?;
    let reset = admin.set_token_price(input(&id, variant, None)).await?;
    assert!(reset.inherited_price.is_none());
    assert!(reset.default_price.is_none());
    assert!(reset.override_price.is_none());
    assert!(
        admin
            .get_token_usage_stats(2024)
            .await?
            .rows
            .iter()
            .filter(|row| row.provider_id == id)
            .all(|row| row.cost.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn token_price_validation_and_corrupt_settings_fail_closed() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let admin = gw.admin();
    for (p, m) in [("", "m"), (" \n", "m"), ("p", ""), ("p", "\t")] {
        assert!(admin.get_token_price(p, m).await.is_err());
        assert!(admin.set_token_price(input(p, m, None)).await.is_err());
    }
    for currency in ["", "usd", "EUR", "CNY "] {
        let mut rate = rates(0.0, 0.0, None);
        rate.currency = currency.into();
        assert!(
            admin
                .set_token_price(input("p", "m", Some(rate)))
                .await
                .is_err()
        );
    }
    for bad in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -0.1,
        1_000_000_001.0,
    ] {
        for rate in [
            rates(bad, 0.0, None),
            rates(0.0, bad, None),
            rates(0.0, 0.0, Some(bad)),
        ] {
            let error = admin
                .set_token_price(input("p", "m", Some(rate)))
                .await
                .unwrap_err();
            assert!(matches!(
                error.downcast_ref::<TokenPriceError>(),
                Some(TokenPriceError::InvalidInput(_))
            ));
        }
    }
    let valid = rates(1_000_000_000.0, 0.0, Some(0.0));
    admin.set_token_price(input("p", "m", Some(valid))).await?;
    let (key, good) = storage
        .settings()
        .list_all()
        .await?
        .into_iter()
        .find(|(key, _)| key.starts_with("token_price.v1."))
        .unwrap();
    admin
        .set_setting("some.unknown.setting", "arbitrary string")
        .await?;
    assert!(admin.set_setting("TOKEN_PRICE.V1.bad", "{}").await.is_err());
    assert!(
        admin
            .set_setting("token_price.v1.bad", &good)
            .await
            .is_err()
    );
    let mut wrong_identity: serde_json::Value = serde_json::from_str(&good)?;
    wrong_identity["model"] = json!("M");
    for bad in ["not json".to_string(), "null".into(), "{}".into(),
        json!({"provider_id":"p","model":"m"}).to_string(),
        json!({"provider_id":"p","model":"m","price":{"currency":"USD","input_rate":0}}).to_string(),
        json!({"provider_id":"p","model":"m","price":{"currency":"USD","input_rate":-1,"output_rate":0}}).to_string(),
        json!({"provider_id":"p","model":"m","price":{"currency":"EUR","input_rate":0,"output_rate":0}}).to_string(),
        wrong_identity.to_string(),
    ] {
        assert!(admin.set_setting(&key, &bad).await.is_err(), "{bad}");
        // Simulate storage corruption or an old importer bypassing AdminService.
        storage.settings().set(&key, &bad).await?;
        let error = admin.get_token_price("p", "m").await.unwrap_err();
        assert!(matches!(error.downcast_ref::<TokenPriceError>(), Some(TokenPriceError::InvalidStored(_))));
        assert!(admin.get_token_usage_stats(2024).await.is_err());
        // Explicit reset repairs a corrupt override; no need to parse it first.
        admin.set_token_price(input("p", "m", None)).await?;
        assert!(admin.get_token_usage_stats(2024).await.is_ok());
    }
    admin.set_setting(&key, &good).await?;
    let mut export = admin.export_config().await?;
    export.settings = vec![
        ("must-not-import".into(), "value".into()),
        (key.clone(), "bad JSON".into()),
    ];
    assert!(admin.import_config(export).await.is_err());
    assert!(admin.get_setting("must-not-import").await?.is_none());
    assert_eq!(admin.get_setting(&key).await?, Some(good));
    Ok(())
}

async fn log(storage: &SqliteStorage, p: &str, m: &str, day: i64) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, upstream_model, input_tokens, output_tokens, cache_read_tokens, upstream_status_code, client_status_code, outcome_version, attempt_outcome) VALUES (?, ?, ?, ?, 1000000, 500000, 250000, 200, 200, 1, 'completed')")
        .bind(uuid::Uuid::new_v4().to_string()).bind(1_704_067_200_000_i64 + day * 86_400_000)
        .bind(p).bind(m).execute(storage.pool()).await?;
    Ok(())
}

#[tokio::test]
async fn token_price_stats_recalculate_manual_unknown_deleted_and_bigmodel() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let id = provider(&storage, "BigModel", "https://open.bigmodel.cn/api/paas/v4").await?;
    let admin = gw.admin();
    for day in 0..2 {
        for (p, m) in [
            (id.as_str(), "glm-5.3"),
            (id.as_str(), "GLM-5.3"),
            (id.as_str(), "unpriced"),
            ("deleted", "glm-5.3"),
            ("Deleted", "glm-5.3"),
        ] {
            log(&storage, p, m, day).await?;
        }
    }
    let before = admin.get_token_usage_stats(2024).await?;
    assert_eq!(before.rows.len(), 10);
    for row in &before.rows {
        if row.provider_id == id && row.model != "unpriced" {
            assert!(!row.cost.as_ref().unwrap().is_manual);
        } else {
            assert!(row.cost.is_none());
        }
    }
    admin
        .set_token_price(input(&id, "glm-5.3", Some(rates(2.0, 8.0, None))))
        .await?;
    let mut zero = rates(0.0, 0.0, Some(0.0));
    zero.currency = "CNY".into();
    admin
        .set_token_price(input(&id, "unpriced", Some(zero)))
        .await?;
    admin
        .set_token_price(input(
            "deleted",
            "glm-5.3",
            Some(rates(2.0, 8.0, Some(0.0))),
        ))
        .await?;
    let after = admin.get_token_usage_stats(2024).await?;
    for (old, row) in before.rows.iter().zip(&after.rows) {
        assert_eq!(old.input_tokens, row.input_tokens);
        assert_eq!(old.request_count, row.request_count);
        assert_eq!(old.cache_read_tokens, row.cache_read_tokens);
        assert_eq!(old.date, row.date);
        if row.provider_id == "Deleted" {
            assert!(row.cost.is_none());
            continue;
        }
        let cost = row.cost.as_ref().unwrap();
        if row.model == "GLM-5.3" {
            assert!(!cost.is_manual);
            continue;
        }
        assert!(cost.is_manual);
        assert_eq!(cost.pricing_provider, "manual");
        assert_eq!(cost.pricing_model, row.model);
        if row.model == "unpriced" {
            assert_eq!(cost.currency, "CNY");
            assert_eq!(cost.total_cost, 0.0);
        } else if row.provider_id == "deleted" {
            assert_eq!(cost.total_cost, 5.5);
            assert!(!cost.cache_rate_fallback);
            assert_eq!(cost.cache_read_rate, 0.0);
        } else {
            assert_eq!(cost.total_cost, 6.0);
            assert_eq!(cost.cache_read_rate, 2.0);
            assert!(cost.cache_rate_fallback);
        }
    }
    // Reset also recalculates already-retained rows at their automatic reference.
    admin.set_token_price(input(&id, "glm-5.3", None)).await?;
    let restored = admin.get_token_usage_stats(2024).await?;
    for row in restored
        .rows
        .iter()
        .filter(|row| row.provider_id == id && row.model == "glm-5.3")
    {
        let original = before
            .rows
            .iter()
            .find(|old| old.provider_id == id && old.model == row.model && old.date == row.date)
            .unwrap();
        assert!(!row.cost.as_ref().unwrap().is_manual);
        assert_eq!(
            row.cost.as_ref().unwrap().total_cost,
            original.cost.as_ref().unwrap().total_cost
        );
    }
    admin
        .set_token_price(input(&id, "glm-5.3", Some(rates(2.0, 8.0, None))))
        .await?;
    storage.providers().delete(&id).await?;
    let config = admin.get_token_price(&id, "glm-5.3").await?;
    assert!(config.default_price.is_none());
    assert!(config.override_price.is_some());
    for row in admin.get_token_usage_stats(2024).await?.rows {
        if row.provider_id == id && row.model == "glm-5.3" {
            assert!(row.cost.unwrap().is_manual);
        }
    }
    admin.set_token_price(input(&id, "glm-5.3", None)).await?;
    assert!(
        admin
            .get_token_usage_stats(2024)
            .await?
            .rows
            .iter()
            .filter(|row| row.provider_id == id && row.model == "glm-5.3")
            .all(|row| row.cost.is_none())
    );
    Ok(())
}

#[tokio::test]
async fn token_price_storage_errors_are_not_missing_or_validation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    storage.pool().close().await;
    for error in [
        gw.admin().get_token_price("p", "m").await.unwrap_err(),
        gw.admin()
            .set_token_price(input("p", "m", None))
            .await
            .unwrap_err(),
    ] {
        assert!(!matches!(
            error.downcast_ref::<TokenPriceError>(),
            Some(TokenPriceError::InvalidInput(_))
        ));
    }
    assert!(gw.admin().get_token_usage_stats(2024).await.is_err());
    Ok(())
}

#[tokio::test]
async fn token_price_export_import_remaps_new_provider_ids_and_preserves_history()
-> anyhow::Result<()> {
    let source_dir = tempfile::tempdir()?;
    let (source, source_storage) = open(source_dir.path()).await?;
    let source_id = provider(
        &source_storage,
        "BigModel",
        "https://open.bigmodel.cn/api/paas/v4",
    )
    .await?;
    let price = rates(2.0, 8.0, None);
    source
        .admin()
        .set_token_price(input(&source_id, "glm-5.3", Some(price.clone())))
        .await?;
    source
        .admin()
        .set_token_price(input(
            &source_id,
            "GLM-5.3",
            Some(rates(0.0, 0.0, Some(0.0))),
        ))
        .await?;
    source
        .admin()
        .set_token_price(input(&source_id, "reset-model", None))
        .await?;
    source
        .admin()
        .set_token_price(input("deleted-source", "glm-5.3", Some(price.clone())))
        .await?;
    let export = source.admin().export_config().await?;
    assert_eq!(
        export.providers[0].source_id.as_deref(),
        Some(source_id.as_str())
    );
    let export: nyro_core::db::models::ExportData =
        serde_json::from_value(serde_json::to_value(export)?)?;
    let dest_dir = tempfile::tempdir()?;
    let (dest, dest_storage) = open(dest_dir.path()).await?;
    let result = dest.admin().import_config(export).await?;
    assert_eq!(result.providers_imported, 1);
    let dest_id = dest_storage.providers().list().await?[0].id.clone();
    assert_ne!(dest_id, source_id);
    let config = dest.admin().get_token_price(&dest_id, "glm-5.3").await?;
    assert_eq!(config.override_price, Some(price.clone()));
    assert_eq!(config.default_price.unwrap().rates.currency, "CNY");
    assert_eq!(
        dest.admin()
            .get_token_price(&dest_id, "GLM-5.3")
            .await?
            .override_price,
        Some(rates(0.0, 0.0, Some(0.0)))
    );
    assert!(
        dest.admin()
            .get_token_price(&source_id, "glm-5.3")
            .await?
            .override_price
            .is_none()
    );
    assert_eq!(
        dest.admin()
            .get_token_price("deleted-source", "glm-5.3")
            .await?
            .override_price,
        Some(price)
    );
    let settings = dest_storage.settings().list_all().await?;
    let prices: Vec<TokenPriceInput> = settings
        .iter()
        .filter(|(key, _)| key.starts_with("token_price.v1."))
        .map(|(_, value)| serde_json::from_str(value).unwrap())
        .collect();
    assert_eq!(prices.len(), 4);
    assert!(prices.iter().all(|entry| entry.provider_id != source_id));
    assert!(prices.iter().any(|entry| entry.provider_id == dest_id
        && entry.model == "reset-model"
        && entry.price.is_none()));
    log(&dest_storage, &dest_id, "glm-5.3", 0).await?;
    let stats = dest.admin().get_token_usage_stats(2024).await?;
    assert!(stats.rows[0].cost.as_ref().unwrap().is_manual);
    assert_eq!(stats.rows[0].cost.as_ref().unwrap().total_cost, 6.0);
    // Reopening verifies the remapped identity/key is durable, not an import cache.
    let (reopened, _) = open(dest_dir.path()).await?;
    assert_eq!(
        reopened
            .admin()
            .get_token_price(&dest_id, "glm-5.3")
            .await?
            .override_price,
        Some(rates(2.0, 8.0, None))
    );
    Ok(())
}

#[tokio::test]
async fn token_price_import_conflicts_skip_rules_and_preserve_existing_overrides()
-> anyhow::Result<()> {
    let source_dir = tempfile::tempdir()?;
    let (source, source_storage) = open(source_dir.path()).await?;
    let source_id = provider(&source_storage, "Same name", "http://127.0.0.1:1/v1").await?;
    source
        .admin()
        .set_token_price(input(&source_id, "model", Some(rates(2.0, 8.0, None))))
        .await?;
    source
        .admin()
        .set_token_price(input(
            &source_id,
            "unconfigured",
            Some(rates(2.0, 8.0, None)),
        ))
        .await?;
    source
        .admin()
        .set_token_price(input(&source_id, "reset", None))
        .await?;
    let export = source.admin().export_config().await?;
    let dest_dir = tempfile::tempdir()?;
    let (dest, dest_storage) = open(dest_dir.path()).await?;
    let dest_id = provider(&dest_storage, "Same name", "http://127.0.0.1:2/v1").await?;
    let existing = rates(9.0, 10.0, Some(1.0));
    for (id, model) in [
        (dest_id.as_str(), "model"),
        (dest_id.as_str(), "reset"),
        (source_id.as_str(), "model"),
    ] {
        dest.admin()
            .set_token_price(input(id, model, Some(existing.clone())))
            .await?;
    }
    let result = dest.admin().import_config(export.clone()).await?;
    assert_eq!(result.providers_imported, 0);
    assert_eq!(
        result.settings_imported, 0,
        "all source-provider rules skipped, including tombstones"
    );
    for (id, model) in [
        (dest_id.as_str(), "model"),
        (dest_id.as_str(), "reset"),
        (source_id.as_str(), "model"),
    ] {
        assert_eq!(
            dest.admin()
                .get_token_price(id, model)
                .await?
                .override_price,
            Some(existing.clone())
        );
    }
    assert!(
        dest.admin()
            .get_token_price(&source_id, "unconfigured")
            .await?
            .override_price
            .is_none()
    );
    assert!(
        dest.admin()
            .get_token_price(&dest_id, "unconfigured")
            .await?
            .override_price
            .is_none()
    );
    // Same-instance backup import must not overwrite newer prices either.
    source
        .admin()
        .set_token_price(input(&source_id, "model", Some(existing.clone())))
        .await?;
    source.admin().import_config(export).await?;
    assert_eq!(
        source
            .admin()
            .get_token_price(&source_id, "model")
            .await?
            .override_price,
        Some(existing)
    );
    Ok(())
}

#[tokio::test]
async fn token_price_legacy_import_preserves_exact_unknown_ids_without_guessing()
-> anyhow::Result<()> {
    let source_dir = tempfile::tempdir()?;
    let (source, source_storage) = open(source_dir.path()).await?;
    let source_id = provider(&source_storage, "Legacy", "http://127.0.0.1:1/v1").await?;
    let price = rates(2.0, 8.0, None);
    source
        .admin()
        .set_token_price(input(&source_id, "model", Some(price.clone())))
        .await?;
    let mut json = serde_json::to_value(source.admin().export_config().await?)?;
    json["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("source_id");
    let legacy: nyro_core::db::models::ExportData = serde_json::from_value(json)?;
    assert!(legacy.providers[0].source_id.is_none());
    let dest_dir = tempfile::tempdir()?;
    let (dest, dest_storage) = open(dest_dir.path()).await?;
    assert_eq!(
        dest.admin()
            .import_config(legacy.clone())
            .await?
            .providers_imported,
        1
    );
    let new_id = dest_storage.providers().list().await?[0].id.clone();
    assert!(
        dest.admin()
            .get_token_price(&new_id, "model")
            .await?
            .override_price
            .is_none()
    );
    assert_eq!(
        dest.admin()
            .get_token_price(&source_id, "model")
            .await?
            .override_price,
        Some(price)
    );
    let current = rates(9.0, 9.0, None);
    source
        .admin()
        .set_token_price(input(&source_id, "model", Some(current.clone())))
        .await?;
    source.admin().import_config(legacy).await?;
    assert_eq!(
        source
            .admin()
            .get_token_price(&source_id, "model")
            .await?
            .override_price,
        Some(current)
    );
    Ok(())
}

#[tokio::test]
async fn token_price_import_rejects_ambiguous_source_ids_before_writes() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    provider(&storage, "Source", "http://127.0.0.1:1/v1").await?;
    let export = gw.admin().export_config().await?;
    for duplicate in [false, true] {
        let mut invalid = export.clone();
        if duplicate {
            let mut second = invalid.providers[0].clone();
            second.name = "New name".into();
            invalid.providers.push(second);
        } else {
            invalid.providers[0].source_id = Some(" \t".into());
        }
        let dest_dir = tempfile::tempdir()?;
        let (dest, dest_storage) = open(dest_dir.path()).await?;
        assert!(dest.admin().import_config(invalid).await.is_err());
        assert!(dest_storage.providers().list().await?.is_empty());
    }
    Ok(())
}

#[test]
fn token_price_old_cost_json_defaults_to_automatic() -> anyhow::Result<()> {
    let cost: TokenUsageCost = serde_json::from_value(json!({
        "currency":"USD", "input_cost":0, "cache_read_cost":0, "output_cost":0,
        "total_cost":0, "cache_rate_fallback":false, "pricing_provider":"openai",
        "pricing_model":"m", "input_rate":0, "output_rate":0, "cache_read_rate":0
    }))?;
    assert!(!cost.is_manual);
    Ok(())
}

/// Insert a completed attempt at `age_hours` ago (rolling-window tests).
async fn recent_log(
    storage: &SqliteStorage,
    provider: &str,
    model: &str,
    age_hours: i64,
) -> anyhow::Result<()> {
    let at = chrono::Utc::now().timestamp_millis() - age_hours * 3_600_000 - 60_000;
    sqlx::query("INSERT INTO request_logs (id, created_at, provider_id, upstream_model, input_tokens, output_tokens, cache_read_tokens, upstream_status_code, client_status_code, outcome_version, attempt_outcome) VALUES (?, ?, ?, ?, 1000000, 500000, 250000, 200, 200, 1, 'completed')")
        .bind(uuid::Uuid::new_v4().to_string()).bind(at)
        .bind(provider).bind(model).execute(storage.pool()).await?;
    Ok(())
}

#[tokio::test]
async fn provider_stats_and_detail_carry_equivalent_api_price() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, storage) = open(dir.path()).await?;
    let id = provider(&storage, "BigModel", "https://open.bigmodel.cn/api/paas/v4").await?;
    let admin = gw.admin();
    // Two priced attempts in the 24h window, one priced attempt outside it, and
    // one model with no reference price.
    recent_log(&storage, &id, "glm-5.3", 1).await?;
    recent_log(&storage, &id, "glm-5.3", 2).await?;
    recent_log(&storage, &id, "glm-5.3", 100).await?;
    recent_log(&storage, &id, "unpriced-model", 3).await?;

    let stats = admin.get_stats_by_provider(Some(24)).await?;
    let item = stats.iter().find(|s| s.provider_id == id).unwrap();
    assert_eq!(
        item.request_count, 3,
        "window excludes the 100h-old attempt"
    );
    assert_eq!(item.unpriced_request_count, 1);
    assert_eq!(item.equivalent_costs.len(), 1);
    let total = &item.equivalent_costs[0];
    assert_eq!(total.currency, "CNY");
    assert_eq!(total.priced_request_count, 2);
    // 1M gross input (250k cached) + 500k output, per attempt, at the registered
    // native CNY rates for glm-5.3: input 8.0, cache hit 2.3, output 28.0.
    let one = 750_000.0 / 1e6 * 8.0 + 250_000.0 / 1e6 * 2.3 + 500_000.0 / 1e6 * 28.0;
    assert!(
        (total.total_cost - 2.0 * one).abs() < 1e-9,
        "{}",
        total.total_cost
    );
    // A wider window picks up the older attempt as well.
    let wide = admin.get_stats_by_provider(Some(720)).await?;
    let wide = wide.iter().find(|s| s.provider_id == id).unwrap();
    assert_eq!(wide.equivalent_costs[0].priced_request_count, 3);
    assert_eq!(wide.unpriced_request_count, 1);

    let detail = admin.get_provider_usage_detail(&id, Some(24)).await?;
    assert_eq!(detail.request_count, 3);
    assert_eq!(detail.unpriced_request_count, 1);
    assert_eq!(detail.equivalent_costs, item.equivalent_costs);
    let glm = detail
        .models
        .iter()
        .find(|m| m.upstream_model == "glm-5.3")
        .unwrap();
    assert_eq!(glm.equivalent_costs, item.equivalent_costs);
    assert_eq!(glm.unpriced_request_count, 0);
    let unpriced = detail
        .models
        .iter()
        .find(|m| m.upstream_model == "unpriced-model")
        .unwrap();
    assert!(unpriced.equivalent_costs.is_empty());
    assert_eq!(unpriced.unpriced_request_count, 1);
    // Per-model totals add up to the provider total.
    let by_model: f64 = detail
        .models
        .iter()
        .flat_map(|m| &m.equivalent_costs)
        .map(|c| c.total_cost)
        .sum();
    assert!((by_model - detail.equivalent_costs[0].total_cost).abs() < 1e-9);

    // A manual price takes precedence and flows through to both views.
    admin
        .set_token_price(input(&id, "glm-5.3", Some(rates(2.0, 8.0, Some(1.0)))))
        .await?;
    let manual = admin.get_provider_usage_detail(&id, Some(24)).await?;
    assert_eq!(manual.equivalent_costs[0].currency, "USD");
    let manual_one = 750_000.0 / 1e6 * 2.0 + 250_000.0 / 1e6 * 1.0 + 500_000.0 / 1e6 * 8.0;
    assert!((manual.equivalent_costs[0].total_cost - 2.0 * manual_one).abs() < 1e-9);

    // Wire format: the UI reads these exact field names.
    let json = serde_json::to_value(&manual)?;
    assert!(json["equivalent_costs"][0]["total_cost"].is_number());
    assert_eq!(json["unpriced_request_count"], 1);
    assert!(json["models"][0].get("equivalent_costs").is_some());
    Ok(())
}
