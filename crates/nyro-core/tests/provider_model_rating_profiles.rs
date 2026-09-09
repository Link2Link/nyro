//! Compatibility regression coverage after simplifying ratings to one score.
use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{ProviderModelRatingState, SetProviderModelRating};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::{CreateProvider, ExportProviderModelRating, ProviderModelRating};
use nyro_core::storage::SqliteStorage;
use serde_json::json;

async fn app() -> anyhow::Result<(tempfile::TempDir, Gateway, sqlx::SqlitePool)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let pool = storage.pool().clone();
    let (gateway, _) = Gateway::from_storage(config, Arc::new(storage)).await?;
    Ok((dir, gateway, pool))
}

fn provider(name: &str) -> CreateProvider {
    serde_json::from_value(json!({
        "name": name, "protocol": "openai-compatible", "base_url": "http://127.0.0.1:1/v1",
        "api_key": "test-only", "auth_mode": "apikey", "use_proxy": false, "fast_mode": false
    }))
    .unwrap()
}

#[test]
fn backup_wire_format_is_unscoped_and_rejects_effort_score_conversion() {
    let old =
        json!({"upstream_model":"exact model", "score":0, "updated_at":"2024-01-01T00:00:00.000Z"});
    let rating: ExportProviderModelRating = serde_json::from_value(old.clone()).unwrap();
    assert_eq!(serde_json::to_value(&rating).unwrap(), old);
    let mut common = old.clone();
    common["effort"] = json!("common");
    assert_eq!(
        serde_json::from_value::<ExportProviderModelRating>(common).unwrap(),
        rating
    );
    for effort in ["low", "medium", "high", "xhigh", "max", "minimal", ""] {
        let mut scoped = old.clone();
        scoped["effort"] = json!(effort);
        let error = serde_json::from_value::<ExportProviderModelRating>(scoped).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("cannot be imported as comprehensive")
        );
    }
    assert!(
        serde_json::from_value::<SetProviderModelRating>(json!({"score":80,"effort":"high"}))
            .is_err()
    );
}

#[tokio::test]
async fn historical_overrides_remain_hidden_and_untouched() -> anyhow::Result<()> {
    let (_dir, gateway, pool) = app().await?;
    let admin = gateway.admin();
    let p = admin.create_provider(provider("legacy")).await?;
    for model in ["same", "override-only"] {
        sqlx::query("INSERT INTO provider_model_ratings(provider_id, upstream_model, effort, score, updated_at) VALUES (?, ?, 'high', 91, 'original')")
            .bind(&p.id).bind(model).execute(&pool).await?;
    }
    assert!(
        admin
            .list_provider_model_ratings(Some(&p.id))
            .await?
            .is_empty()
    );
    assert!(matches!(
        admin
            .get_provider_model_rating(&p.id, "override-only")
            .await?,
        ProviderModelRatingState::Unrated { .. }
    ));
    assert!(admin.get_model_performance(None).await?.models.is_empty());
    let saved = admin
        .set_provider_model_rating(&p.id, "same", SetProviderModelRating { score: 0 })
        .await?;
    assert!(serde_json::to_value(&saved)?.get("effort").is_none());
    assert_eq!(
        admin.list_provider_model_ratings(None).await?,
        vec![saved.clone()]
    );
    let performance = serde_json::to_value(admin.get_model_performance(None).await?)?;
    let models = performance["models"].as_array().unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0]["rating"], serde_json::to_value(&saved)?);
    assert!(models[0].get("mixed").is_some());
    assert!(models[0].get("tiers").is_none());
    assert!(models[0].get("profile").is_none());
    let export = admin.export_config().await?;
    assert_eq!(export.providers[0].model_ratings.len(), 1);
    assert_eq!(export.providers[0].model_ratings[0].score, 0);
    assert!(
        serde_json::to_value(&export)?["providers"][0]["model_ratings"][0]
            .get("effort")
            .is_none()
    );
    let copy = admin.copy_provider(&p.id).await?;
    assert_eq!(
        admin
            .list_provider_model_ratings(Some(&copy.id))
            .await?
            .len(),
        1
    );
    let copied_scopes: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM provider_model_ratings WHERE provider_id=? AND effort!='common'",
    )
    .bind(&copy.id)
    .fetch_one(&pool)
    .await?;
    assert_eq!(copied_scopes, 0);
    admin.delete_provider_model_rating(&p.id, "same").await?;
    let store = gateway.storage.provider_model_ratings().unwrap();
    store
        .restore(
            &p.id,
            &[ProviderModelRating {
                provider_id: p.id.clone(),
                upstream_model: "same".into(),
                score: 75,
                updated_at: "restored".into(),
            }],
        )
        .await?;
    assert_eq!(store.get(&p.id, "same").await?.unwrap().score, 75);
    store.restore(&p.id, &[]).await?;
    assert!(store.list(Some(&p.id)).await?.is_empty());
    let retained: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM provider_model_ratings WHERE provider_id=? AND effort='high' AND score=91 AND updated_at='original'").bind(&p.id).fetch_one(&pool).await?;
    assert_eq!(retained, 2);
    Ok(())
}
