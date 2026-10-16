use super::{Storage, traits::ProviderStore};
use crate::db::models::{Provider, UpdateProvider};

#[tokio::test]
async fn memory_provider_store_is_read_only() -> anyhow::Result<()> {
    let provider: Provider = serde_json::from_value(serde_json::json!({
        "id": "yaml-provider", "name": "YAML", "vendor": "custom",
        "protocol": "openai", "base_url": "https://example.invalid",
        "api_key": "", "auth_mode": "apikey", "is_enabled": true,
        "created_at": "2000-01-01 00:00:00", "updated_at": "2000-01-01 00:00:00"
    }))?;
    let storage = super::MemoryStorage::new(vec![provider], vec![], vec![]);
    assert!(
        ProviderStore::get(&storage, "yaml-provider")
            .await?
            .unwrap()
            .protocol_endpoints
            .is_empty()
    );
    let input = serde_json::from_value(serde_json::json!({
        "name": "rejected",
        "vendor": "custom",
        "protocol": "openai",
        "base_url": "https://example.invalid",
        "api_key": ""
    }))?;
    assert!(storage.providers().create(input).await.is_err());
    assert!(
        storage
            .providers()
            .update("yaml-provider", UpdateProvider::default())
            .await
            .is_err()
    );
    Ok(())
}
