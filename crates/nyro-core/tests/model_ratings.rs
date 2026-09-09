use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{ModelRatingError, SetModelRating};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::*;
use nyro_core::storage::{MemoryStorage, SqliteStorage, Storage};
use serde_json::json;

struct TestApp {
    _dir: tempfile::TempDir,
    gateway: Gateway,
    pool: sqlx::SqlitePool,
}

async fn app() -> anyhow::Result<TestApp> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let pool = storage.pool().clone();
    let (gateway, _) = Gateway::from_storage(config, Arc::new(storage)).await?;
    Ok(TestApp {
        _dir: dir,
        gateway,
        pool,
    })
}

fn provider(name: &str) -> CreateProvider {
    CreateProvider {
        name: name.to_string(),
        vendor: None,
        protocol: "openai-compatible".to_string(),
        base_url: "http://127.0.0.1:1/v1".to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: vec![],
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: "test-only".to_string(),
        auth_mode: "apikey".to_string(),
        use_proxy: false,
        fast_mode: false,
    }
}

fn mapping(name: &str, provider_id: &str, model: &str) -> CreateModel {
    CreateModel {
        name: name.to_string(),
        balance: Some("weighted".to_string()),
        target_provider: provider_id.to_string(),
        target_model: model.to_string(),
        targets: vec![],
        enable_auth: None,
        enable_payload: None,
        force_max_reasoning: None,
        vision_shim: None,
    }
}

#[tokio::test]
async fn rating_strict_catalog_distinguishes_outage_from_empty_directory() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let router = axum::Router::new()
        .route(
            "/empty",
            axum::routing::get(|| async { axum::Json(json!({ "data": [] })) }),
        )
        .route(
            "/broken",
            axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
        )
        .route(
            "/malformed",
            axum::routing::get(|| async { axum::Json(json!({ "error": "oops" })) }),
        )
        .route(
            "/bad-entry",
            axum::routing::get(|| async { axum::Json(json!({ "data": [{ "id": null }] })) }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });
    let mut input = provider("catalog");
    input.models_source = Some(format!("http://{address}/empty"));
    input.static_models = Some("fallback-model".to_string());
    let p = admin.create_provider(input).await?;
    assert!(
        admin
            .get_provider_models_with_catalog_validation(&p.id, true)
            .await?
            .is_empty()
    );
    for path in ["broken", "malformed", "bad-entry"] {
        admin
            .update_provider(
                &p.id,
                UpdateProvider {
                    models_source: Some(format!("http://{address}/{path}")),
                    ..Default::default()
                },
            )
            .await?;
        assert!(
            admin
                .get_provider_models_with_catalog_validation(&p.id, true)
                .await
                .is_err()
        );
        assert_eq!(
            admin.get_provider_models(&p.id).await?,
            vec!["fallback-model"]
        );
    }
    let _ = shutdown_tx.send(());
    server.await??;
    Ok(())
}

#[tokio::test]
async fn rating_zero_is_a_stored_score_and_failures_are_errors() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    admin.create_provider(provider("ratings")).await?;
    assert!(admin.list_model_ratings().await?.is_empty());
    let zero = admin
        .set_model_rating("absent", SetModelRating { score: 0 })
        .await?;
    assert_eq!(zero.score, 0);
    assert_eq!(admin.list_model_ratings().await?, vec![zero.clone()]);
    // Upserting the same prefix replaces the row in place.
    let renewed = admin
        .set_model_rating("absent", SetModelRating { score: 100 })
        .await?;
    assert_eq!(admin.list_model_ratings().await?, vec![renewed.clone()]);
    assert_ne!(renewed.updated_at, zero.updated_at);
    admin.delete_model_rating("absent").await?;
    admin.delete_model_rating("absent").await?;
    assert!(admin.list_model_ratings().await?.is_empty());
    sqlx::query("DROP TABLE model_rating_prefixes")
        .execute(&app.pool)
        .await?;
    assert!(admin.list_model_ratings().await.is_err());
    assert!(admin.set_model_rating("x", SetModelRating { score: 1 }).await.is_err());
    assert!(
        admin.export_config().await.is_err(),
        "backup must not silently lose ratings on a DB error"
    );
    Ok(())
}

#[tokio::test]
async fn rating_validation_retains_exact_identity_without_catalog_requests() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("offline")).await?;
    admin
        .update_provider(
            &p.id,
            UpdateProvider {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;
    for score in [-1, 101] {
        assert!(matches!(
            admin
                .set_model_rating("x", SetModelRating { score })
                .await
                .unwrap_err()
                .downcast_ref(),
            Some(ModelRatingError::InvalidInput(_))
        ));
    }
    for prefix in ["", " \t ", "a\0b", &"a".repeat(1025)] {
        assert!(
            admin
                .set_model_rating(prefix, SetModelRating { score: 80 })
                .await
                .is_err()
        );
    }
    for prefix in [
        "model-x",
        "Model-X",
        "model-x ",
        " model-x",
        "vendor/模型-é+#",
        "deepseek-v4-pro-",
    ] {
        admin
            .set_model_rating(prefix, SetModelRating { score: 100 })
            .await?;
    }
    let saved = admin.list_model_ratings().await?;
    // "Model-X" canonicalizes onto "model-x"; only case collapses.
    assert_eq!(saved.len(), 5);
    assert!(saved.iter().any(|row| row.model_prefix == "model-x "));
    assert!(saved.iter().all(|row| row.model_prefix == row.model_prefix.to_lowercase()));
    // Case-insensitive upsert and delete address the same canonical entry.
    let renewed = admin
        .set_model_rating("MODEL-X", SetModelRating { score: 42 })
        .await?;
    assert_eq!(renewed.model_prefix, "model-x");
    assert_eq!(renewed.score, 42);
    admin.delete_model_rating("MODEL-x").await?;
    let after = admin.list_model_ratings().await?;
    assert_eq!(after.len(), 4);
    assert!(after.iter().all(|row| row.model_prefix != "model-x"));
    Ok(())
}

#[tokio::test]
async fn rating_changes_do_not_affect_routes_or_epoch_and_outlive_providers()
-> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("one")).await?;
    let p2 = admin.create_provider(provider("two")).await?;
    let m1 = admin
        .create_model(mapping("mapping-one", &p.id, "x"))
        .await?;
    admin
        .create_model(mapping("mapping-two", &p.id, "x"))
        .await?;
    let snapshot = serde_json::to_value(admin.list_models().await?)?;
    let epoch = admin.get_setting("config_epoch").await?;
    admin
        .set_model_rating("x", SetModelRating { score: 75 })
        .await?;
    admin
        .set_model_rating("deepseek-v4-pro", SetModelRating { score: 0 })
        .await?;
    admin.delete_model_rating("x").await?;
    let kept = admin
        .set_model_rating("x", SetModelRating { score: 85 })
        .await?;
    assert_eq!(serde_json::to_value(admin.list_models().await?)?, snapshot);
    assert_eq!(admin.get_setting("config_epoch").await?, epoch);
    admin.delete_model(&m1.id).await?;
    admin.delete_provider(&p.id).await?;
    admin.delete_provider(&p2.id).await?;
    let remaining = admin.list_model_ratings().await?;
    assert_eq!(remaining.len(), 2);
    assert_eq!(
        remaining
            .iter()
            .find(|row| row.model_prefix == "x")
            .unwrap()
            .score,
        kept.score
    );
    assert_eq!(
        remaining
            .iter()
            .find(|row| row.model_prefix == "deepseek-v4-pro")
            .unwrap()
            .score,
        0
    );
    Ok(())
}

#[tokio::test]
async fn rating_copy_shares_prefix_entries_without_touching_them() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("source")).await?;
    let saved = admin
        .set_model_rating("no-longer-listed", SetModelRating { score: 0 })
        .await?;
    let _copy = admin.copy_provider(&p.id).await?;
    assert_eq!(admin.list_model_ratings().await?, vec![saved]);
    Ok(())
}

#[tokio::test]
async fn rating_backup_roundtrip_preserves_time_and_replaces_on_reimport()
-> anyhow::Result<()> {
    let source = app().await?;
    let source_admin = source.gateway.admin();
    source_admin
        .create_provider(provider("portable"))
        .await?;
    let old = source_admin
        .set_model_rating("old/x ", SetModelRating { score: 0 })
        .await?;
    let export = source_admin.export_config().await?;
    assert_eq!(export.model_ratings.len(), 1);
    assert_eq!(export.model_ratings[0].score, 0);
    let dest = app().await?;
    let admin = dest.gateway.admin();
    let result = admin.import_config(export.clone()).await?;
    assert_eq!(result.providers_imported, 1);
    assert_eq!(result.ratings_imported, 1);
    let restored = admin.list_model_ratings().await?;
    assert_eq!(restored[0].updated_at, old.updated_at);
    assert_eq!(restored[0].model_prefix, "old/x ");
    admin
        .set_model_rating("old/x ", SetModelRating { score: 90 })
        .await?;
    // Import replaces the whole prefix table; the backup's scores win again.
    let result = admin.import_config(export.clone()).await?;
    assert_eq!(result.providers_imported, 0);
    assert_eq!(result.ratings_imported, 1);
    assert_eq!(admin.list_model_ratings().await?[0].score, 0);
    // Old backups with per-provider nested ratings import providers but not scores.
    let legacy_dest = app().await?;
    let mut legacy = serde_json::to_value(export.clone())?;
    let legacy_object = legacy.as_object_mut().unwrap();
    legacy_object.remove("model_ratings");
    legacy_object["providers"][0]
        .as_object_mut()
        .unwrap()
        .insert(
            "model_ratings".to_string(),
            json!([{ "upstream_model": "legacy", "score": 50, "updated_at": "2024-01-01T00:00:00.000Z" }]),
        );
    let result = legacy_dest
        .gateway
        .admin()
        .import_config(serde_json::from_value(legacy)?)
        .await?;
    assert_eq!(result.providers_imported, 1);
    assert_eq!(result.ratings_imported, 0);
    assert!(
        legacy_dest
            .gateway
            .admin()
            .list_model_ratings()
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn rating_import_rejects_invalid_entries_before_any_writes() -> anyhow::Result<()> {
    let source = app().await?;
    source
        .gateway
        .admin()
        .create_provider(provider("portable"))
        .await?;
    source
        .gateway
        .admin()
        .set_model_rating("x", SetModelRating { score: 100 })
        .await?;
    let export = source.gateway.admin().export_config().await?;
    let dest = app().await?;
    for kind in ["score", "duplicate", "timestamp", "prefix"] {
        let mut invalid = export.clone();
        match kind {
            "score" => invalid.model_ratings[0].score = 101,
            "duplicate" => {
                let duplicate = invalid.model_ratings[0].clone();
                invalid.model_ratings.push(duplicate);
            }
            "timestamp" => invalid.model_ratings[0].updated_at = "yesterday".to_string(),
            _ => invalid.model_ratings[0].model_prefix = "\0".to_string(),
        }
        assert!(dest.gateway.admin().import_config(invalid).await.is_err());
        assert!(dest.gateway.admin().list_providers().await?.is_empty());
        assert!(dest.gateway.admin().list_model_ratings().await?.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn rating_rows_persist_across_reopening_storage() -> anyhow::Result<()> {
    let app = app().await?;
    app.gateway
        .admin()
        .create_provider(provider("durable"))
        .await?;
    let saved = app
        .gateway
        .admin()
        .set_model_rating("x", SetModelRating { score: 80 })
        .await?;
    let reopened = SqliteStorage::from_config(&app.gateway.config).await?;
    assert_eq!(
        reopened.model_ratings().unwrap().list().await?,
        vec![saved]
    );
    Ok(())
}

#[tokio::test]
async fn rating_yaml_memory_mode_is_explicitly_unsupported() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::from_storage(
        GatewayConfig {
            data_dir: dir.path().to_path_buf(),
            ..Default::default()
        },
        Arc::new(MemoryStorage::new(vec![], vec![], vec![])),
    )
    .await?;
    let error = gw.admin().list_model_ratings().await.unwrap_err();
    assert!(matches!(
        error.downcast_ref(),
        Some(ModelRatingError::UnsupportedStorage)
    ));
    assert!(
        gw.admin()
            .set_model_rating("x", SetModelRating { score: 50 })
            .await
            .is_err()
    );
    Ok(())
}
