use std::sync::Arc;

use nyro_core::Gateway;
use nyro_core::admin::{
    ProviderModelRatingError, ProviderModelRatingState, SetProviderModelRating,
    SetProviderModelRatingProfile,
};
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::{CreateProvider, ExportData, ProviderModelRating};
use nyro_core::storage::SqliteStorage;
use serde_json::{Value, json};

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

fn input(common: Option<i32>, overrides: [Option<i32>; 5]) -> SetProviderModelRatingProfile {
    serde_json::from_value(json!({"common": common, "overrides": {
        "low": overrides[0], "medium": overrides[1], "high": overrides[2],
        "xhigh": overrides[3], "max": overrides[4]
    }}))
    .unwrap()
}

#[test]
fn profile_input_requires_all_keys_and_rejects_coercion_unknowns() {
    let valid = json!({"common": null, "overrides": {"low": 0, "medium": null, "high": 100, "xhigh": null, "max": null}});
    assert!(serde_json::from_value::<SetProviderModelRatingProfile>(valid.clone()).is_ok());
    for key in ["common", "overrides"] {
        let mut invalid = valid.clone();
        invalid.as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<SetProviderModelRatingProfile>(invalid).is_err(),
            "missing {key}"
        );
    }
    for key in ["low", "medium", "high", "xhigh", "max"] {
        let mut invalid = valid.clone();
        invalid["overrides"].as_object_mut().unwrap().remove(key);
        assert!(
            serde_json::from_value::<SetProviderModelRatingProfile>(invalid).is_err(),
            "missing {key}"
        );
        for value in [json!(1.5), json!("50"), json!(true), json!([]), json!({})] {
            let mut invalid = valid.clone();
            invalid["overrides"][key] = value;
            assert!(serde_json::from_value::<SetProviderModelRatingProfile>(invalid).is_err());
        }
    }
    for value in [json!(1.5), json!("50"), json!(true)] {
        let mut invalid = valid.clone();
        invalid["common"] = value;
        assert!(serde_json::from_value::<SetProviderModelRatingProfile>(invalid).is_err());
    }
    for nested in [false, true] {
        let mut invalid = valid.clone();
        let object = if nested {
            &mut invalid["overrides"]
        } else {
            &mut invalid
        };
        object["minimal"] = Value::Null;
        assert!(serde_json::from_value::<SetProviderModelRatingProfile>(invalid).is_err());
    }
    assert!(serde_json::from_str::<SetProviderModelRatingProfile>(r#"{"common":null,"common":1,"overrides":{"low":null,"medium":null,"high":null,"xhigh":null,"max":null}}"#).is_err());
}

#[tokio::test]
async fn profile_effective_zero_explicit_equal_and_legacy_common_only() -> anyhow::Result<()> {
    let (_dir, gw, _) = app().await?;
    let admin = gw.admin();
    let p = admin.create_provider(provider("profiles")).await?;
    let epoch = admin.get_setting("config_epoch").await?;
    let initial = admin
        .get_provider_model_rating_profile(&p.id, " model/X ")
        .await?;
    assert_eq!(initial.display_mode, "common");
    assert_eq!(initial.effective.low.status, "unrated");
    assert_eq!(initial.effective.low.score, None);
    let saved = admin
        .set_provider_model_rating_profile(
            &p.id,
            " model/X ",
            input(Some(50), [Some(0), Some(50), None, None, None]),
        )
        .await?;
    assert_eq!(saved.display_mode, "per_effort");
    assert_eq!(saved.effective.low.score, Some(0));
    assert_eq!(saved.effective.low.source, "override");
    assert_eq!(
        saved.effective.medium.source, "override",
        "equal-valued override stays explicit"
    );
    assert_eq!(saved.effective.high.source, "common");
    assert_eq!(admin.list_provider_model_ratings(None).await?.len(), 1);
    admin
        .delete_provider_model_rating(&p.id, " model/X ")
        .await?;
    let cleared = admin
        .get_provider_model_rating_profile(&p.id, " model/X ")
        .await?;
    assert_eq!(cleared.common, None);
    assert_eq!(cleared.overrides, saved.overrides);
    assert_eq!(cleared.effective.high.status, "unrated");
    assert_eq!(cleared.effective.high.source, "unrated");
    assert_eq!(cleared.effective.high.score_updated_at, None);
    assert!(admin.list_provider_model_ratings(None).await?.is_empty());
    assert_eq!(
        admin.list_provider_model_rating_profiles(None).await?.len(),
        1
    );
    assert!(matches!(
        admin.get_provider_model_rating(&p.id, " model/X ").await?,
        ProviderModelRatingState::Unrated { .. }
    ));
    admin
        .set_provider_model_rating(&p.id, " model/X ", SetProviderModelRating { score: 75 })
        .await?;
    let legacy_set = admin
        .get_provider_model_rating_profile(&p.id, " model/X ")
        .await?;
    assert_eq!(legacy_set.overrides, saved.overrides);
    assert_eq!(legacy_set.effective.max.score, Some(75));
    let clear_common = admin
        .set_provider_model_rating_profile(
            &p.id,
            " model/X ",
            input(None, [Some(0), Some(50), None, None, None]),
        )
        .await?;
    assert_eq!(clear_common.common, None);
    assert_eq!(clear_common.overrides, saved.overrides);
    let last_override = admin
        .set_provider_model_rating_profile(
            &p.id,
            " model/X ",
            input(Some(75), [Some(0), None, None, None, None]),
        )
        .await?;
    assert_eq!(last_override.display_mode, "per_effort");
    let common_only = admin
        .set_provider_model_rating_profile(&p.id, " model/X ", input(Some(75), [None; 5]))
        .await?;
    assert_eq!(common_only.display_mode, "common");
    let empty = admin
        .set_provider_model_rating_profile(&p.id, " model/X ", input(None, [None; 5]))
        .await?;
    assert_eq!(empty, initial);
    assert!(
        admin
            .list_provider_model_rating_profiles(None)
            .await?
            .is_empty()
    );
    assert_eq!(admin.get_setting("config_epoch").await?, epoch);
    Ok(())
}

#[tokio::test]
async fn profile_save_preserves_unchanged_times_and_rolls_back_entire_failure() -> anyhow::Result<()>
{
    let (_dir, gw, pool) = app().await?;
    let admin = gw.admin();
    let p = admin.create_provider(provider("atomic")).await?;
    let store = gw.storage.provider_model_ratings().unwrap();
    for (effort, score) in [("common", 50), ("low", 0), ("high", 80)] {
        store
            .upsert(ProviderModelRating {
                provider_id: p.id.clone(),
                upstream_model: "x".into(),
                effort: effort.into(),
                score,
                updated_at: "2020-01-01T00:00:00.000Z".into(),
            })
            .await?;
    }
    let before = admin.get_provider_model_rating_profile(&p.id, "x").await?;
    let same = admin
        .set_provider_model_rating_profile(
            &p.id,
            "x",
            input(Some(50), [Some(0), None, Some(80), None, None]),
        )
        .await?;
    assert_eq!(before, same);
    let changed = admin
        .set_provider_model_rating_profile(
            &p.id,
            "x",
            input(Some(50), [Some(1), Some(40), Some(80), None, None]),
        )
        .await?;
    assert_eq!(changed.common, before.common);
    assert_eq!(changed.overrides.high, before.overrides.high);
    assert_ne!(changed.overrides.low, before.overrides.low);
    assert_eq!(
        changed.overrides.low.as_ref().unwrap().updated_at,
        changed.overrides.medium.as_ref().unwrap().updated_at
    );
    sqlx::query("CREATE TRIGGER fail_profile BEFORE INSERT ON provider_model_ratings WHEN NEW.effort = 'max' BEGIN SELECT RAISE(ABORT, 'test profile failure'); END").execute(&pool).await?;
    assert!(
        admin
            .set_provider_model_rating_profile(
                &p.id,
                "x",
                input(Some(99), [None, None, None, None, Some(100)])
            )
            .await
            .is_err()
    );
    assert_eq!(
        admin.get_provider_model_rating_profile(&p.id, "x").await?,
        changed
    );
    let rows = store.list(Some(&p.id)).await?;
    let invalid = ProviderModelRating {
        score: 101,
        ..rows[0].clone()
    };
    assert!(store.replace_profile(&p.id, "x", &[invalid]).await.is_err());
    assert_eq!(store.list(Some(&p.id)).await?, rows);
    Ok(())
}

#[tokio::test]
async fn profile_validates_scores_models_and_exact_provider_without_writes() -> anyhow::Result<()> {
    let (_dir, gw, _) = app().await?;
    let admin = gw.admin();
    let p = admin.create_provider(provider("validation")).await?;
    for score in [-1, 101] {
        for scope in 0..6 {
            let mut scores = [None; 5];
            let common = if scope == 0 {
                Some(score)
            } else {
                scores[scope - 1] = Some(score);
                None
            };
            assert!(
                admin
                    .set_provider_model_rating_profile(&p.id, "x", input(common, scores))
                    .await
                    .is_err()
            );
        }
    }
    for model in ["", " \t ", "a\0b", &"é".repeat(513)] {
        assert!(
            admin
                .set_provider_model_rating_profile(&p.id, model, input(None, [None; 5]))
                .await
                .is_err()
        );
    }
    for id in ["absent".to_string(), p.id.to_uppercase()] {
        assert!(matches!(
            admin
                .get_provider_model_rating_profile(&id, "x")
                .await
                .unwrap_err()
                .downcast_ref(),
            Some(ProviderModelRatingError::ProviderNotFound)
        ));
        assert!(
            admin
                .list_provider_model_rating_profiles(Some(&id))
                .await
                .is_err()
        );
    }
    for model in ["x", "X", "x ", " x", "模型/é"] {
        admin
            .set_provider_model_rating_profile(
                &p.id,
                model,
                input(None, [None, None, None, None, Some(100)]),
            )
            .await?;
    }
    assert_eq!(
        admin
            .list_provider_model_rating_profiles(Some(&p.id))
            .await?
            .len(),
        5
    );
    assert!(admin.list_provider_model_ratings(None).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn profile_copy_import_all_scopes_preserve_times_and_legacy_defaults() -> anyhow::Result<()> {
    let (_dir, gw, _) = app().await?;
    let admin = gw.admin();
    let p = admin.create_provider(provider("portable")).await?;
    admin
        .set_provider_model_rating_profile(
            &p.id,
            "x ",
            input(Some(0), [Some(0), Some(20), Some(50), Some(80), Some(100)]),
        )
        .await?;
    admin
        .set_provider_model_rating_profile(
            &p.id,
            "override-only",
            input(None, [Some(1), None, None, None, None]),
        )
        .await?;
    let original = admin.get_provider_model_rating_profile(&p.id, "x ").await?;
    let copied = admin.copy_provider(&p.id).await?;
    let copy = admin
        .get_provider_model_rating_profile(&copied.id, "x ")
        .await?;
    assert_eq!(original.common, copy.common);
    assert_eq!(original.overrides, copy.overrides);
    assert_eq!(
        gw.storage
            .provider_model_ratings()
            .unwrap()
            .list(Some(&copied.id))
            .await?
            .len(),
        7
    );
    admin.delete_provider(&copied.id).await?;
    let mut export = admin.export_config().await?;
    assert_eq!(export.version, 2);
    assert_eq!(export.providers[0].model_ratings.len(), 7);
    export.providers[0].model_ratings[0].updated_at = "2024-01-01T01:00:00+01:00".into();
    let (_dest_dir, dest, _) = app().await?;
    let result = dest.admin().import_config(export.clone()).await?;
    assert_eq!(result.ratings_imported, 7);
    let restored = dest.admin().export_config().await?;
    assert_eq!(
        restored.providers[0].model_ratings[0].updated_at,
        "2024-01-01T00:00:00.000Z"
    );
    assert_eq!(
        restored.providers[0].model_ratings[1..],
        export.providers[0].model_ratings[1..]
    );
    assert_eq!(
        dest.admin()
            .import_config(export.clone())
            .await?
            .ratings_imported,
        0
    );
    for effort in ["minimal", "COMMON", "invalid"] {
        let mut invalid = export.clone();
        invalid.providers[0].model_ratings[0].effort = effort.into();
        assert!(dest.admin().import_config(invalid).await.is_err());
    }
    let mut invalid = export.clone();
    let duplicate = invalid.providers[0].model_ratings[0].clone();
    invalid.providers[0].model_ratings.push(duplicate);
    assert!(dest.admin().import_config(invalid).await.is_err());
    let mut legacy = serde_json::to_value(export)?;
    legacy["providers"][0]["model_ratings"] =
        json!([{"upstream_model":"legacy","score":0,"updated_at":"2020-01-01T00:00:00Z"}]);
    let legacy: ExportData = serde_json::from_value(legacy)?;
    assert_eq!(legacy.providers[0].model_ratings[0].effort, "common");
    let (_legacy_dir, legacy_gw, _) = app().await?;
    assert_eq!(
        legacy_gw
            .admin()
            .import_config(legacy)
            .await?
            .ratings_imported,
        1
    );
    let legacy_profile = legacy_gw
        .admin()
        .list_provider_model_rating_profiles(None)
        .await?;
    assert_eq!(legacy_profile[0].display_mode, "common");
    assert_eq!(legacy_profile[0].effective.low.score, Some(0));
    Ok(())
}
