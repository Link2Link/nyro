use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{
    ProviderModelRatingError, ProviderModelRatingState, SetProviderModelRating,
};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::*;
use nyro_core::storage::{MemoryStorage, SqliteStorage};
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
async fn rating_zero_unrated_and_failure_have_distinct_contracts() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("ratings")).await?;
    let absent = serde_json::to_value(admin.get_provider_model_rating(&p.id, "absent").await?)?;
    assert_eq!(
        absent,
        json!({
            "provider_id": p.id, "upstream_model": "absent", "status": "unrated",
            "score": null, "updated_at": null
        })
    );
    let zero = admin
        .set_provider_model_rating(&p.id, "absent", SetProviderModelRating { score: 0 })
        .await?;
    let state = serde_json::to_value(admin.get_provider_model_rating(&p.id, "absent").await?)?;
    assert_eq!(state["status"], "rated");
    assert_eq!(state["score"], 0);
    assert_eq!(state["updated_at"], zero.updated_at);
    assert_eq!(admin.list_provider_model_ratings(None).await?.len(), 1);
    admin.delete_provider_model_rating(&p.id, "absent").await?;
    admin.delete_provider_model_rating(&p.id, "absent").await?;
    assert!(matches!(
        admin.get_provider_model_rating(&p.id, "absent").await?,
        ProviderModelRatingState::Unrated { .. }
    ));
    assert!(matches!(
        admin
            .get_provider_model_rating("missing-provider", "absent")
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(ProviderModelRatingError::ProviderNotFound)
    ));
    sqlx::query("DROP TABLE provider_model_ratings")
        .execute(&app.pool)
        .await?;
    assert!(
        admin
            .get_provider_model_rating(&p.id, "absent")
            .await
            .is_err()
    );
    assert!(admin.list_provider_model_ratings(None).await.is_err());
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
                .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score })
                .await
                .unwrap_err()
                .downcast_ref(),
            Some(ProviderModelRatingError::InvalidInput(_))
        ));
    }
    for model in ["", " \t ", "a\0b", &"a".repeat(1025)] {
        assert!(
            admin
                .set_provider_model_rating(&p.id, model, SetProviderModelRating { score: 80 })
                .await
                .is_err()
        );
    }
    for model in [
        "model-x",
        "Model-X",
        "model-x ",
        " model-x",
        "vendor/模型-é+#",
    ] {
        admin
            .set_provider_model_rating(&p.id, model, SetProviderModelRating { score: 100 })
            .await?;
    }
    let saved = admin.list_provider_model_ratings(Some(&p.id)).await?;
    assert_eq!(saved.len(), 5);
    assert!(saved.iter().any(|row| row.upstream_model == "model-x "));
    let before = saved.clone();
    admin
        .update_provider(
            &p.id,
            UpdateProvider {
                base_url: Some("http://127.0.0.1:2/v1".to_string()),
                ..Default::default()
            },
        )
        .await?;
    assert_eq!(
        admin.list_provider_model_ratings(Some(&p.id)).await?,
        before
    );
    Ok(())
}

#[tokio::test]
async fn rating_changes_do_not_affect_routes_or_epoch_and_outlive_mappings() -> anyhow::Result<()> {
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
        .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score: 75 })
        .await?;
    admin
        .set_provider_model_rating(&p2.id, "x", SetProviderModelRating { score: 0 })
        .await?;
    admin.delete_provider_model_rating(&p.id, "x").await?;
    admin
        .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score: 85 })
        .await?;
    assert_eq!(serde_json::to_value(admin.list_models().await?)?, snapshot);
    assert_eq!(admin.get_setting("config_epoch").await?, epoch);
    assert_eq!(
        admin.list_provider_model_ratings(Some(&p.id)).await?.len(),
        1
    );
    admin.delete_model(&m1.id).await?;
    assert_eq!(
        admin.list_provider_model_ratings(Some(&p.id)).await?.len(),
        1
    );
    admin.delete_provider(&p.id).await?;
    let remaining = admin.list_provider_model_ratings(None).await?;
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].provider_id, p2.id);
    assert_eq!(remaining[0].score, 0);
    Ok(())
}

#[tokio::test]
async fn rating_copy_preserves_snapshot_and_time_but_changes_are_independent() -> anyhow::Result<()>
{
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("source")).await?;
    let old = ProviderModelRating {
        provider_id: p.id.clone(),
        upstream_model: "no-longer-listed".to_string(),
        effort: "common".to_string(),
        score: 0,
        updated_at: "2024-01-01T00:00:00.000Z".to_string(),
    };
    app.gateway
        .storage
        .provider_model_ratings()
        .unwrap()
        .upsert(old.clone())
        .await?;
    let copy = admin.copy_provider(&p.id).await?;
    let copied = admin.list_provider_model_ratings(Some(&copy.id)).await?;
    assert_eq!(copied.len(), 1);
    assert_eq!(copied[0].score, old.score);
    assert_eq!(copied[0].updated_at, old.updated_at);
    assert_eq!(copied[0].upstream_model, old.upstream_model);
    assert_ne!(copied[0].provider_id, old.provider_id);
    let renewed = admin
        .set_provider_model_rating(
            &copy.id,
            &old.upstream_model,
            SetProviderModelRating { score: 0 },
        )
        .await?;
    assert_ne!(
        renewed.updated_at, old.updated_at,
        "reconfirming a score updates its timestamp"
    );
    admin
        .delete_provider_model_rating(&copy.id, &old.upstream_model)
        .await?;
    assert_eq!(
        admin.list_provider_model_ratings(Some(&p.id)).await?,
        vec![old]
    );
    Ok(())
}

#[tokio::test]
async fn rating_copy_failure_rolls_back_new_provider() -> anyhow::Result<()> {
    let app = app().await?;
    let admin = app.gateway.admin();
    let p = admin.create_provider(provider("source")).await?;
    admin
        .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score: 95 })
        .await?;
    sqlx::query("CREATE TRIGGER fail_rating_copy BEFORE INSERT ON provider_model_ratings WHEN (SELECT name FROM providers WHERE id = NEW.provider_id) LIKE '%_Copy%' BEGIN SELECT RAISE(ABORT, 'test rating copy failure'); END").execute(&app.pool).await?;
    let error = admin.copy_provider(&p.id).await.unwrap_err();
    assert!(error.to_string().contains("rolled back"));
    assert_eq!(admin.list_providers().await?.len(), 1);
    assert_eq!(
        admin.list_provider_model_ratings(Some(&p.id)).await?.len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn rating_backup_roundtrip_rebinds_ids_preserves_time_and_skips_existing_names()
-> anyhow::Result<()> {
    let source = app().await?;
    let source_admin = source.gateway.admin();
    let p = source_admin.create_provider(provider("portable")).await?;
    let old = ProviderModelRating {
        provider_id: p.id.clone(),
        upstream_model: "old/x ".to_string(),
        effort: "common".to_string(),
        score: 0,
        updated_at: "2024-01-01T00:00:00.000Z".to_string(),
    };
    source
        .gateway
        .storage
        .provider_model_ratings()
        .unwrap()
        .upsert(old.clone())
        .await?;
    let export = source_admin.export_config().await?;
    assert_eq!(export.providers[0].model_ratings[0].score, 0);
    let dest = app().await?;
    let admin = dest.gateway.admin();
    let result = admin.import_config(export.clone()).await?;
    assert_eq!(result.providers_imported, 1);
    assert_eq!(result.ratings_imported, 1);
    let restored = admin.list_provider_model_ratings(None).await?;
    assert_eq!(restored[0].updated_at, old.updated_at);
    assert_eq!(restored[0].upstream_model, old.upstream_model);
    assert_ne!(restored[0].provider_id, old.provider_id);
    admin
        .set_provider_model_rating(
            &restored[0].provider_id,
            &old.upstream_model,
            SetProviderModelRating { score: 90 },
        )
        .await?;
    let result = admin.import_config(export.clone()).await?;
    assert_eq!(result.providers_imported, 0);
    assert_eq!(result.ratings_imported, 0);
    assert_eq!(admin.list_provider_model_ratings(None).await?[0].score, 90);
    let legacy_dest = app().await?;
    let mut legacy = serde_json::to_value(export)?;
    legacy["providers"][0]
        .as_object_mut()
        .unwrap()
        .remove("model_ratings");
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
            .list_provider_model_ratings(None)
            .await?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn rating_import_rejects_invalid_entries_before_writes_and_rolls_back_failures()
-> anyhow::Result<()> {
    let source = app().await?;
    let p = source
        .gateway
        .admin()
        .create_provider(provider("portable"))
        .await?;
    source
        .gateway
        .admin()
        .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score: 100 })
        .await?;
    let export = source.gateway.admin().export_config().await?;
    let dest = app().await?;
    for kind in ["score", "duplicate", "timestamp", "model"] {
        let mut invalid = export.clone();
        match kind {
            "score" => invalid.providers[0].model_ratings[0].score = 101,
            "duplicate" => {
                let duplicate = invalid.providers[0].model_ratings[0].clone();
                invalid.providers[0].model_ratings.push(duplicate);
            }
            "timestamp" => {
                invalid.providers[0].model_ratings[0].updated_at = "yesterday".to_string()
            }
            _ => invalid.providers[0].model_ratings[0].upstream_model = "\0".to_string(),
        }
        assert!(dest.gateway.admin().import_config(invalid).await.is_err());
        assert!(dest.gateway.admin().list_providers().await?.is_empty());
    }
    sqlx::query("CREATE TRIGGER fail_rating_import BEFORE INSERT ON provider_model_ratings BEGIN SELECT RAISE(ABORT, 'test rating import failure'); END").execute(&dest.pool).await?;
    let error = dest
        .gateway
        .admin()
        .import_config(export)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("rolled back"));
    assert!(dest.gateway.admin().list_providers().await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn rating_rows_persist_across_reopening_storage() -> anyhow::Result<()> {
    let app = app().await?;
    let p = app
        .gateway
        .admin()
        .create_provider(provider("durable"))
        .await?;
    let saved = app
        .gateway
        .admin()
        .set_provider_model_rating(&p.id, "x", SetProviderModelRating { score: 80 })
        .await?;
    let reopened = SqliteStorage::from_config(&app.gateway.config).await?;
    use nyro_core::storage::Storage;
    assert_eq!(
        reopened
            .provider_model_ratings()
            .unwrap()
            .get(&p.id, "x")
            .await?,
        Some(saved)
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
    let error = gw
        .admin()
        .list_provider_model_ratings(None)
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref(),
        Some(ProviderModelRatingError::UnsupportedStorage)
    ));
    assert!(
        gw.admin()
            .set_provider_model_rating("p", "x", SetProviderModelRating { score: 50 })
            .await
            .is_err()
    );
    Ok(())
}
