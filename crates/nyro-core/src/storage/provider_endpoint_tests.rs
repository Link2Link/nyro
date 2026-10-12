use super::{Storage, traits::ProviderStore};
use crate::db::models::{CreateProvider, Provider, UpdateProvider};

fn create_input(vendor: &str, protocol: &str) -> anyhow::Result<CreateProvider> {
    Ok(serde_json::from_value(serde_json::json!({
        "name": format!("endpoint-storage-test-{}", uuid::Uuid::new_v4()),
        "vendor": vendor,
        "protocol": protocol,
        "base_url": "https://api2.cursor.sh",
        "api_key": "",
        "auth_mode": "oauth",
        "protocol_endpoints": []
    }))?)
}

pub(super) async fn exercise_native_cursor_endpoints(storage: &dyn Storage) -> anyhow::Result<()> {
    for (vendor, protocol, expected_count) in [
        ("cursor", "cursor-agent/run/v1", 0),
        (" CuRsOr ", "cursor-agent/run/v1", 0),
        ("cursor", "openai", 1),
        ("custom", "cursor-agent/run/v1", 1),
    ] {
        let provider = storage
            .providers()
            .create(create_input(vendor, protocol)?)
            .await?;
        assert_eq!(provider.protocol_endpoints.len(), expected_count);
        let updated = storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    name: Some(format!("{}-updated", provider.name)),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(
            updated.protocol_endpoints.len(),
            expected_count,
            "omitting endpoints must preserve the native/legacy distinction"
        );
        let updated = storage
            .providers()
            .update(
                &provider.id,
                UpdateProvider {
                    protocol_endpoints: Some(Vec::new()),
                    ..Default::default()
                },
            )
            .await?;
        assert!(
            updated.protocol_endpoints.is_empty(),
            "explicit empty endpoints must stay empty"
        );
        assert!(
            storage
                .providers()
                .get(&provider.id)
                .await?
                .unwrap()
                .protocol_endpoints
                .is_empty()
        );
        if expected_count == 0 {
            // Startup reruns migrations; an endpoint backfill must not turn a
            // native provider into a compatibility provider on restart.
            for _ in 0..2 {
                storage.bootstrap().migrate().await?;
                let migrated = storage.providers().get(&provider.id).await?.unwrap();
                assert_eq!(
                    migrated.protocol, "cursor-agent/run/v1",
                    "native execution endpoint must not collapse to a fixed-mode suite"
                );
                assert!(
                    migrated.protocol_endpoints.is_empty(),
                    "reentrant migration must preserve empty Cursor native endpoints"
                );
            }
        }
        storage.providers().delete(&provider.id).await?;
    }
    Ok(())
}

#[tokio::test]
async fn sqlite_native_cursor_has_no_legacy_endpoint() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let pool = crate::db::init_pool(directory.path()).await?;
    crate::db::migrate(&pool).await?;
    let storage = super::SqliteStorage::from_pool(pool.clone());
    exercise_native_cursor_endpoints(&storage).await?;
    pool.close().await;
    Ok(())
}

#[tokio::test]
async fn memory_native_cursor_keeps_yaml_endpoints_empty_and_read_only() -> anyhow::Result<()> {
    let provider: Provider = serde_json::from_value(serde_json::json!({
        "id": "cursor-yaml", "name": "Cursor", "vendor": "cursor",
        "protocol": "cursor-agent/run/v1", "base_url": "https://api2.cursor.sh",
        "api_key": "", "auth_mode": "oauth", "is_enabled": true,
        "created_at": "2000-01-01 00:00:00", "updated_at": "2000-01-01 00:00:00"
    }))?;
    let storage = super::MemoryStorage::new(vec![provider], vec![], vec![]);
    assert!(
        ProviderStore::get(&storage, "cursor-yaml")
            .await?
            .unwrap()
            .protocol_endpoints
            .is_empty()
    );
    assert!(
        ProviderStore::list(&storage).await?[0]
            .protocol_endpoints
            .is_empty()
    );
    assert!(
        storage
            .providers()
            .create(create_input("cursor", "cursor-agent/run/v1")?)
            .await
            .is_err()
    );
    assert!(
        storage
            .providers()
            .update("cursor-yaml", UpdateProvider::default())
            .await
            .is_err()
    );
    Ok(())
}
