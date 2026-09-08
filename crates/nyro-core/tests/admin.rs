use nyro_core::Gateway;
use nyro_core::admin::CopyProviderOptions;
use nyro_core::config::GatewayConfig;
use nyro_core::db::models::*;
use nyro_core::storage::Storage as _;
use std::path::PathBuf;

use uuid::Uuid;

const FAR_FUTURE_RFC3339: &str = "2099-01-01T00:00:00Z";
const CODEX_RUNTIME_URL: &str = "https://chatgpt.com/backend-api/codex";

#[tokio::test]
async fn copy_provider_creates_disabled_provider_with_copy_suffix() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("source-provider"))
        .await?;

    let copied = gw.admin().copy_provider(&original.id).await?;

    assert_ne!(copied.id, original.id);
    assert_eq!(copied.name, "source-provider_Copy");
    assert_eq!(copied.vendor, original.vendor);
    assert_eq!(copied.protocol, original.protocol);
    assert_eq!(copied.base_url, original.base_url);
    assert_eq!(copied.preset_key, original.preset_key);
    assert_eq!(copied.channel, original.channel);
    assert_eq!(copied.models_source, original.models_source);
    assert_eq!(copied.static_models, original.static_models);
    assert_eq!(copied.api_key, original.api_key);
    assert_eq!(copied.auth_mode, original.auth_mode);
    assert_eq!(copied.use_proxy, original.use_proxy);
    assert!(original.is_enabled);
    assert!(!copied.is_enabled);

    Ok(())
}

#[tokio::test]
async fn copy_provider_uses_numbered_suffix_when_copy_name_exists() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("source-provider"))
        .await?;
    gw.admin().copy_provider(&original.id).await?;

    let second_copy = gw.admin().copy_provider(&original.id).await?;

    assert_eq!(second_copy.name, "source-provider_Copy2");

    Ok(())
}

#[tokio::test]
async fn copy_provider_can_copy_matching_route_targets_to_copied_provider() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("route-source-provider"))
        .await?;
    let fallback = gw
        .admin()
        .create_provider(api_key_provider_input("route-fallback-provider"))
        .await?;

    let source_route = gw
        .admin()
        .create_model(CreateModel {
            name: "source-model".to_string(),
            balance: Some("priority".to_string()),
            target_provider: String::new(),
            target_model: String::new(),
            targets: vec![
                CreateModelBackend {
                    provider_id: original.id.clone(),
                    model: "source-upstream-model".to_string(),
                    weight: Some(80),
                    priority: Some(1),
                    is_fallback: None,
                },
                CreateModelBackend {
                    provider_id: fallback.id.clone(),
                    model: "fallback-upstream-model".to_string(),
                    weight: Some(20),
                    priority: Some(2),
                    is_fallback: None,
                },
            ],
            enable_auth: Some(true),
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    let copied = gw
        .admin()
        .copy_provider_with_options(
            &original.id,
            CopyProviderOptions {
                append_targets: true,
            },
        )
        .await?;

    assert!(!copied.is_enabled);
    let models = gw.admin().list_models().await?;
    assert_eq!(
        models.len(),
        1,
        "copying route targets must not create new routes"
    );
    assert!(models.iter().all(|model| model.name != "source-model_Copy"));

    let updated_model = models
        .iter()
        .find(|model| model.id == source_route.id)
        .expect("source route should remain");
    assert_eq!(updated_model.name, "source-model");
    assert_eq!(updated_model.balance, "priority");
    assert!(updated_model.enable_auth);
    assert_eq!(updated_model.target_provider, original.id);
    assert_eq!(updated_model.target_model, "source-upstream-model");
    assert_eq!(updated_model.targets.len(), 3);
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == original.id
            && target.model == "source-upstream-model"
            && target.weight == 80
            && target.priority == 1
    }));
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == copied.id
            && target.model == "source-upstream-model"
            && target.weight == 80
            && target.priority == 1
    }));
    assert!(updated_model.targets.iter().any(|target| {
        target.provider_id == fallback.id
            && target.model == "fallback-upstream-model"
            && target.weight == 20
            && target.priority == 2
    }));

    Ok(())
}

#[tokio::test]
async fn copy_provider_does_not_append_targets_by_default() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw
        .admin()
        .create_provider(api_key_provider_input("no-route-copy-provider"))
        .await?;

    gw.admin()
        .create_model(CreateModel {
            name: "no-route-copy-model".to_string(),
            balance: None,
            target_provider: original.id.clone(),
            target_model: "source-upstream-model".to_string(),
            targets: vec![],
            enable_auth: None,
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    gw.admin().copy_provider(&original.id).await?;

    let models = gw.admin().list_models().await?;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].targets.len(), 1);
    assert_eq!(models[0].targets[0].provider_id, original.id);

    Ok(())
}

#[tokio::test]
async fn api_key_privileged_flag_roundtrips_and_keeps_bindings() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("privileged-key-provider"))
        .await?;
    let model = gw
        .admin()
        .create_model(CreateModel {
            name: "privileged-gate-model".to_string(),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "gpt-test".to_string(),
            targets: vec![CreateModelBackend {
                provider_id: provider.id.clone(),
                model: "gpt-test".to_string(),
                weight: None,
                priority: None,
                is_fallback: None,
            }],
            enable_auth: Some(true),
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    // Create a privileged key bound to nothing; it must still pass the
    // access check for the auth-enabled model (binding bypass).
    let key = gw
        .admin()
        .create_api_key(CreateApiKey {
            name: "priv-key".to_string(),
            rpm: None,
            rpd: None,
            tpm: None,
            tpd: None,
            expires_at: None,
            is_privileged: true,
            model_ids: Vec::new(),
        })
        .await?;
    assert!(key.is_privileged, "created key must echo is_privileged");

    // Storage-level: the auth access record carries the flag, and the model
    // binding check is bypassed for this key.
    {
        let auth = gw
            .storage
            .auth()
            .expect("sqlite storage exposes auth store");
        let record = auth
            .find_api_key(&key.token)
            .await?
            .expect("key row must exist");
        assert!(record.is_privileged);
        assert!(
            !auth.model_binding_exists(&record.id, &model.id).await?,
            "privileged key is created without bindings by definition"
        );
    }

    // Toggle privilege off via update; the flag flips and stays persisted.
    let updated = gw
        .admin()
        .update_api_key(
            &key.id,
            UpdateApiKey {
                name: None,
                rpm: None,
                rpd: None,
                tpm: None,
                tpd: None,
                is_enabled: None,
                is_privileged: Some(false),
                expires_at: None,
                model_ids: None,
            },
        )
        .await?;
    assert!(!updated.is_privileged);

    // Bindings survive a privileged round-trip untouched: bind a model while
    // privileged, toggle privilege off, and the binding is still there.
    gw.admin()
        .update_api_key(
            &key.id,
            UpdateApiKey {
                name: None,
                rpm: None,
                rpd: None,
                tpm: None,
                tpd: None,
                is_enabled: None,
                is_privileged: Some(true),
                expires_at: None,
                model_ids: Some(vec![model.id.clone()]),
            },
        )
        .await?;
    let demoted = gw
        .admin()
        .update_api_key(
            &key.id,
            UpdateApiKey {
                name: None,
                rpm: None,
                rpd: None,
                tpm: None,
                tpd: None,
                is_enabled: None,
                is_privileged: Some(false),
                expires_at: None,
                model_ids: None,
            },
        )
        .await?;
    assert!(!demoted.is_privileged);
    assert_eq!(
        demoted.model_ids,
        vec![model.id.clone()],
        "bindings must be preserved across privileged toggles"
    );

    Ok(())
}

#[tokio::test]
async fn create_model_persists_force_max_reasoning_and_vision_shim() -> anyhow::Result<()> {
    // Regression: the SQLite/Postgres/MySQL INSERT bound force_max_reasoning and
    // vision_shim in swapped order, which failed with
    // "NOT NULL constraint failed: models.force_max_reasoning" on databases whose
    // force_max_reasoning column is NOT NULL (added by migration).
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("model-create-roundtrip-provider"))
        .await?;

    let model = gw
        .admin()
        .create_model(CreateModel {
            name: "model-create-roundtrip".to_string(),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "gpt-test".to_string(),
            targets: vec![CreateModelBackend {
                provider_id: provider.id.clone(),
                model: "gpt-test".to_string(),
                weight: None,
                priority: None,
                is_fallback: None,
            }],
            enable_auth: None,
            force_max_reasoning: Some(true),
            enable_payload: None,
            vision_shim: Some(serde_json::json!({ "helper_model": "text-test" })),
        })
        .await?;

    assert!(
        model.force_max_reasoning,
        "force_max_reasoning must round-trip through INSERT"
    );
    assert!(
        model
            .vision_shim
            .as_deref()
            .map_or(false, |raw| raw.contains("helper_model")),
        "vision_shim config must round-trip through INSERT, got: {:?}",
        model.vision_shim
    );

    // Re-read from storage so the assertions cover the persisted row, not an echo.
    let reloaded = gw
        .storage
        .models()
        .get(&model.id)
        .await?
        .expect("model row must exist after create");
    assert!(reloaded.force_max_reasoning);
    assert!(
        reloaded
            .vision_shim
            .as_deref()
            .map_or(false, |raw| raw.contains("helper_model"))
    );

    // Defaults path (all flags None) must insert cleanly too.
    let plain = gw
        .admin()
        .create_model(CreateModel {
            name: "model-create-defaults".to_string(),
            balance: None,
            target_provider: provider.id.clone(),
            target_model: "gpt-test".to_string(),
            targets: vec![CreateModelBackend {
                provider_id: provider.id.clone(),
                model: "gpt-test".to_string(),
                weight: None,
                priority: None,
                is_fallback: None,
            }],
            enable_auth: None,
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;
    assert!(!plain.force_max_reasoning);
    assert_eq!(plain.vision_shim, None);

    Ok(())
}

#[tokio::test]
async fn model_backend_fallback_flag_roundtrips_through_storage() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let primary = gw
        .admin()
        .create_provider(api_key_provider_input("fallback-primary-provider"))
        .await?;
    let rescue = gw
        .admin()
        .create_provider(api_key_provider_input("fallback-rescue-provider"))
        .await?;

    let model = gw
        .admin()
        .create_model(CreateModel {
            name: "fallback-roundtrip".to_string(),
            balance: None,
            target_provider: primary.id.clone(),
            target_model: "gpt-test".to_string(),
            targets: vec![
                CreateModelBackend {
                    provider_id: primary.id.clone(),
                    model: "gpt-test".to_string(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: None,
                },
                CreateModelBackend {
                    provider_id: rescue.id.clone(),
                    model: "gpt-mini".to_string(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: Some(true),
                },
            ],
            enable_auth: None,
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    let rescue_row = model
        .targets
        .iter()
        .find(|target| target.provider_id == rescue.id)
        .expect("rescue backend row must exist");
    assert!(
        rescue_row.is_fallback,
        "fallback flag must round-trip through INSERT"
    );
    assert!(
        model
            .targets
            .iter()
            .any(|target| target.provider_id == primary.id && !target.is_fallback),
        "regular rows stay non-fallback"
    );

    // Persisted rows carry the flag too, not just the echoed create response.
    let store = gw
        .storage
        .model_backends()
        .expect("sqlite storage exposes backend store");
    let reloaded = store.list_backends_by_model(&model.id).await?;
    assert_eq!(reloaded.len(), 2);
    assert_eq!(
        reloaded.iter().filter(|target| target.is_fallback).count(),
        1,
        "exactly one persisted fallback row"
    );

    // Validation: two fallback rows in one update must be rejected.
    let err = gw
        .admin()
        .update_model(
            &model.id,
            UpdateModel {
                name: None,
                balance: None,
                target_provider: None,
                target_model: None,
                targets: Some(vec![
                    UpsertModelBackend {
                        id: None,
                        provider_id: primary.id.clone(),
                        model: "gpt-test".to_string(),
                        weight: Some(100),
                        priority: Some(1),
                        is_fallback: Some(true),
                    },
                    UpsertModelBackend {
                        id: None,
                        provider_id: rescue.id.clone(),
                        model: "gpt-mini".to_string(),
                        weight: Some(100),
                        priority: Some(1),
                        is_fallback: Some(true),
                    },
                ]),
                enable_auth: None,
                enable_payload: None,
                force_max_reasoning: None,
                vision_shim: None,
                is_enabled: None,
            },
        )
        .await
        .expect_err("duplicate fallback rows must be rejected");
    assert!(
        err.to_string().contains("only one fallback backend"),
        "unexpected error: {err}"
    );

    Ok(())
}

#[tokio::test]
async fn delete_provider_removes_route_associations_before_provider() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let removed_provider = gw
        .admin()
        .create_provider(api_key_provider_input("route-delete-provider"))
        .await?;
    let kept_provider = gw
        .admin()
        .create_provider(api_key_provider_input("route-keep-provider"))
        .await?;

    let removed_route = gw
        .admin()
        .create_model(CreateModel {
            name: "route-owned-model".to_string(),
            balance: None,
            target_provider: removed_provider.id.clone(),
            target_model: "gpt-delete".to_string(),
            targets: vec![],
            enable_auth: None,
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;
    let kept_route = gw
        .admin()
        .create_model(CreateModel {
            name: "route-kept-model".to_string(),
            balance: None,
            target_provider: kept_provider.id.clone(),
            target_model: "gpt-keep".to_string(),
            targets: vec![
                CreateModelBackend {
                    provider_id: kept_provider.id.clone(),
                    model: "gpt-keep".to_string(),
                    weight: Some(100),
                    priority: Some(1),
                    is_fallback: None,
                },
                CreateModelBackend {
                    provider_id: removed_provider.id.clone(),
                    model: "gpt-delete-secondary".to_string(),
                    weight: Some(50),
                    priority: Some(2),
                    is_fallback: None,
                },
            ],
            enable_auth: None,
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    gw.admin().delete_provider(&removed_provider.id).await?;

    assert!(
        gw.admin().get_provider(&removed_provider.id).await.is_err(),
        "provider should be deleted after dependent route rows are removed"
    );

    let models = gw.admin().list_models().await?;
    assert!(
        !models.iter().any(|model| model.id == removed_route.id),
        "routes whose primary provider was deleted should be removed"
    );
    let kept_route = models
        .iter()
        .find(|model| model.id == kept_route.id)
        .expect("route with a different primary provider should remain");
    assert_eq!(kept_route.target_provider, kept_provider.id);
    assert!(
        kept_route
            .targets
            .iter()
            .all(|target| target.provider_id != removed_provider.id),
        "secondary route target associations for the deleted provider should be removed"
    );

    let model_cache = gw.model_cache.read().await;
    assert!(model_cache.match_model("route-owned-model").is_none());
    assert!(model_cache.match_model("route-kept-model").is_some());

    Ok(())
}

#[tokio::test]
async fn copy_oauth_provider_copies_credential_binding() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let original = gw.admin().create_provider(oauth_provider_input()).await?;
    seed_oauth_credential(&gw, &original.id, "copy-access-token", "copy-refresh-token").await?;

    let copied = gw.admin().copy_provider(&original.id).await?;
    let copied_credential = gw.storage.oauth_credentials().get(&copied.id).await?;

    assert_eq!(copied.name, format!("{}_Copy", original.name));
    assert_eq!(copied.auth_mode, "oauth");
    assert!(copied.api_key.is_empty());
    assert_eq!(
        copied_credential
            .as_ref()
            .map(|cred| cred.access_token.as_str()),
        Some("copy-access-token"),
    );

    Ok(())
}

#[tokio::test]
async fn logout_provider_oauth_preserves_oauth_mode_and_disconnects_binding() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;
    let provider = gw.admin().create_provider(oauth_provider_input()).await?;
    seed_oauth_credential(&gw, &provider.id, "test-access-token", "test-refresh-token").await?;

    let status = gw.admin().logout_provider_oauth(&provider.id).await?;
    assert_eq!(status.status, "disconnected");

    let updated = gw.admin().get_provider(&provider.id).await?;
    assert_eq!(updated.effective_auth_mode(), "oauth");
    assert!(updated.api_key.is_empty());
    let oauth_cred = gw.storage.oauth_credentials().get(&provider.id).await?;
    assert!(
        oauth_cred.is_none(),
        "oauth credential should be deleted after logout"
    );

    Ok(())
}

async fn build_gateway() -> anyhow::Result<Gateway> {
    let config = GatewayConfig {
        data_dir: test_data_dir(),
        ..Default::default()
    };
    let (gw, _log_rx) = Gateway::new(config).await?;
    Ok(gw)
}

// ── config epoch tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn config_epoch_starts_at_zero_and_increments_on_model_create() -> anyhow::Result<()> {
    let gw = build_gateway().await?;

    let epoch_before: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("epoch-test-provider"))
        .await?;
    gw.admin()
        .create_model(CreateModel {
            name: "epoch-test-model".to_string(),
            balance: Some("weighted".to_string()),
            target_provider: provider.id.clone(),
            target_model: "gpt-4".to_string(),
            targets: vec![],
            enable_auth: Some(false),
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    let epoch_after: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    assert!(
        epoch_after > epoch_before,
        "config_epoch should increment after create_model: before={epoch_before} after={epoch_after}"
    );
    Ok(())
}

#[tokio::test]
async fn config_epoch_increments_on_model_update_and_delete() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let provider = gw
        .admin()
        .create_provider(api_key_provider_input("epoch-update-provider"))
        .await?;
    let model = gw
        .admin()
        .create_model(CreateModel {
            name: "epoch-update-model".to_string(),
            balance: Some("weighted".to_string()),
            target_provider: provider.id.clone(),
            target_model: "gpt-4".to_string(),
            targets: vec![],
            enable_auth: Some(false),
            force_max_reasoning: None,
            enable_payload: None,
            vision_shim: None,
        })
        .await?;

    let epoch_before_update: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);

    gw.admin()
        .update_model(
            &model.id,
            UpdateModel {
                is_enabled: Some(false),
                ..Default::default()
            },
        )
        .await?;

    let epoch_after_update: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        epoch_after_update > epoch_before_update,
        "epoch should increment on update"
    );

    gw.admin().delete_model(&model.id).await?;

    let epoch_after_delete: i64 = gw
        .storage
        .settings()
        .get("config_epoch")
        .await?
        .as_deref()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    assert!(
        epoch_after_delete > epoch_after_update,
        "epoch should increment on delete"
    );

    Ok(())
}

// ── readyz (StorageBootstrap::health) ─────────────────────────────────────

#[tokio::test]
async fn storage_health_is_reachable_for_sqlite_gateway() -> anyhow::Result<()> {
    let gw = build_gateway().await?;
    let health = gw.storage.bootstrap().health().await?;
    assert!(
        health.can_connect,
        "SQLite health check should report can_connect"
    );
    assert!(
        health.schema_compatible,
        "SQLite health check should report schema_compatible after migration"
    );
    Ok(())
}

#[tokio::test]
async fn sqlite_provider_endpoints_are_transactional_and_persist_test_status() -> anyhow::Result<()>
{
    let gw = build_gateway().await?;
    let input = CreateProvider {
        name: "adaptive-storage-provider".to_string(),
        vendor: None,
        protocol: "openai-compatible/chat-completions/v1".to_string(),
        base_url: "https://chat.example/v1".to_string(),
        protocol_mode: "adaptive".to_string(),
        protocol_endpoints: vec![
            CreateProviderProtocolEndpoint {
                protocol: "openai-compatible/chat-completions/v1".to_string(),
                base_url: "https://chat.example/v1".to_string(),
                api_key: "sk-chat".to_string(),
                auth_scheme: "bearer".to_string(),
                is_enabled: true,
                priority: 0,
            },
            CreateProviderProtocolEndpoint {
                protocol: "anthropic-messages/messages/2023-06-01".to_string(),
                base_url: "https://messages.example".to_string(),
                api_key: "sk-anthropic".to_string(),
                auth_scheme: "x-api-key".to_string(),
                is_enabled: true,
                priority: 1,
            },
        ],
        preset_key: None,
        channel: None,
        models_source: None,
        static_models: None,
        api_key: "sk-chat".to_string(),
        auth_mode: "apikey".to_string(),
        use_proxy: false,
        fast_mode: false,
    };

    let provider = gw.storage.providers().create(input.clone()).await?;
    assert_eq!(provider.protocol_endpoints.len(), 2);
    let endpoint_id = provider.protocol_endpoints[1].id.clone();
    gw.storage
        .providers()
        .record_endpoint_test_result(
            &endpoint_id,
            nyro_core::storage::traits::ProviderEndpointTestResult {
                success: false,
                error: Some("HTTP 401 Unauthorized".to_string()),
                tested_at: "2026-08-07T00:00:00Z".to_string(),
            },
        )
        .await?;
    let provider = gw.storage.providers().get(&provider.id).await?.unwrap();
    let tested = provider
        .protocol_endpoints
        .iter()
        .find(|endpoint| endpoint.id == endpoint_id)
        .unwrap();
    assert_eq!(tested.test_status, "failed");
    assert_eq!(tested.test_error.as_deref(), Some("HTTP 401 Unauthorized"));

    let mut duplicate = input;
    duplicate.name = "duplicate-endpoint-provider".to_string();
    duplicate.protocol_endpoints[1].protocol = "openai-compatible/chat-completions/v1".to_string();
    assert!(gw.storage.providers().create(duplicate).await.is_err());
    assert!(
        !gw.storage
            .providers()
            .exists_by_name("duplicate-endpoint-provider", None)
            .await?,
        "provider insert must roll back when an endpoint insert fails"
    );

    Ok(())
}

#[tokio::test]
async fn schema_compatible_is_false_when_migrations_skipped() -> anyhow::Result<()> {
    // Create a SQLite pool on a fresh directory without running any migrations.
    // Gateway::new() would fail at ModelCache load, so test directly at storage level.
    let dir = tempfile::tempdir()?;
    let pool = nyro_core::db::init_pool(dir.path()).await?;
    let storage = nyro_core::storage::SqliteStorage::from_pool(pool);

    let health = storage.bootstrap().health().await?;

    assert!(
        health.can_connect,
        "should still connect to SQLite even without schema"
    );
    assert!(
        !health.schema_compatible,
        "schema_compatible must be false when models table has not been created"
    );
    Ok(())
}

fn test_data_dir() -> PathBuf {
    std::env::temp_dir().join(format!("nyro-admin-integration-tests-{}", Uuid::new_v4()))
}

fn oauth_provider_input() -> CreateProvider {
    CreateProvider {
        name: format!("oauth-provider-{}", Uuid::new_v4()),
        vendor: Some("openai".to_string()),
        protocol: "openai".to_string(),
        base_url: CODEX_RUNTIME_URL.to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: Vec::new(),
        preset_key: Some("openai".to_string()),
        channel: Some("codex".to_string()),
        models_source: None,
        static_models: None,
        api_key: String::new(),
        auth_mode: "oauth".to_string(),
        use_proxy: false,
        fast_mode: false,
    }
}

fn api_key_provider_input(name: &str) -> CreateProvider {
    CreateProvider {
        name: name.to_string(),
        vendor: Some("openai".to_string()),
        protocol: "openai-compatible".to_string(),
        base_url: "https://api.openai.com/v1".to_string(),
        protocol_mode: "fixed".to_string(),
        protocol_endpoints: Vec::new(),
        preset_key: Some("openai".to_string()),
        channel: Some("default".to_string()),
        models_source: Some("https://api.openai.com/v1/models".to_string()),
        static_models: Some("gpt-test\ntext-test".to_string()),
        api_key: "sk-test".to_string(),
        auth_mode: "apikey".to_string(),
        use_proxy: true,
        fast_mode: false,
    }
}

async fn seed_oauth_credential(
    gw: &Gateway,
    provider_id: &str,
    access_token: &str,
    refresh_token: &str,
) -> anyhow::Result<()> {
    gw.storage
        .oauth_credentials()
        .upsert(
            provider_id,
            UpsertOAuthCredential {
                driver_key: "codex".to_string(),
                scheme: "oauth_auth_code_pkce".to_string(),
                access_token: access_token.to_string(),
                refresh_token: Some(refresh_token.to_string()),
                expires_at: Some(FAR_FUTURE_RFC3339.to_string()),
                resource_url: Some(CODEX_RUNTIME_URL.to_string()),
                subject_id: Some("acct_test".to_string()),
                scopes: Some("[\"openid\",\"offline_access\"]".to_string()),
                meta: Some(format!(r#"{{"access_token":"{access_token}"}}"#)),
            },
        )
        .await?;
    Ok(())
}

// ── log deletion tests ──────────────────────────────────────────────────────

#[tokio::test]
async fn admin_clears_payloads_deletes_single_log_and_clears_error_logs() -> anyhow::Result<()> {
    use nyro_core::logging::LogEntry;
    use nyro_core::protocol::ir::Usage;

    let gw = build_gateway().await?;
    let entry = |client_status: i32, upstream_status: Option<i32>| LogEntry {
        performance: Default::default(),
        api_key_id: None,
        api_key_name: None,
        created_at: 1,
        client_protocol: "openai/chat/v1".into(),
        upstream_protocol: "openai/chat/v1".into(),
        provider_id: "provider-1".into(),
        provider_name: "Provider".into(),
        model_id: Some("model-1".into()),
        model_name: Some("Model".into()),
        upstream_url: None,
        client_model: "gpt-test".into(),
        upstream_model: "gpt-test".into(),
        reasoning_effort: None,
        route_decision: None,
        method: Some("POST".into()),
        path: Some("/v1/chat/completions".into()),
        client_request_headers: Some(r#"{"client-request":true}"#.into()),
        client_request_body: Some(r#"{"client-request":true}"#.into()),
        client_response_headers: Some(r#"{"client-response":true}"#.into()),
        client_response_body: Some(r#"{"client-response":true}"#.into()),
        upstream_request_headers: Some(r#"{"upstream-request":true}"#.into()),
        upstream_request_body: Some(r#"{"upstream-request":true}"#.into()),
        upstream_response_headers: Some(r#"{"upstream-response":true}"#.into()),
        upstream_response_body: Some(r#"{"upstream-response":true}"#.into()),
        upstream_status_code: upstream_status,
        client_status_code: client_status,
        latency_total_ms: 1,
        latency_upstream_ms: Some(1),
        usage: Usage::default(),
        is_stream: false,
        stream_chunks_count: 0,
        stream_first_chunk_ms: None,
        enable_payload: None,
    };

    gw.storage
        .logs()
        .append_batch(vec![
            entry(200, Some(200)),
            entry(429, Some(429)),
            entry(200, Some(503)),
            entry(200, Some(200)),
        ])
        .await?;

    assert_eq!(gw.admin().clear_log_payloads().await?, 2);
    assert_eq!(gw.admin().clear_log_payloads().await?, 0);

    let rows = gw.admin().query_logs(LogQuery::default()).await?;
    assert_eq!(rows.total, 4, "payload clearing preserves log rows");
    let first_ok = rows
        .items
        .iter()
        .find(|i| i.client_status_code == Some(200) && i.upstream_status_code == Some(200))
        .expect("successful row exists")
        .id
        .clone();
    let detail = gw
        .admin()
        .get_log(&first_ok)
        .await?
        .expect("log remains after payload clearing");
    assert!(detail.client_request_headers.is_none());
    assert!(detail.client_request_body.is_none());
    assert!(detail.client_response_headers.is_none());
    assert!(detail.client_response_body.is_none());
    assert!(detail.upstream_request_headers.is_none());
    assert!(detail.upstream_request_body.is_none());
    assert!(detail.upstream_response_headers.is_none());
    assert!(detail.upstream_response_body.is_none());

    let client_error_id = rows
        .items
        .iter()
        .find(|i| i.client_status_code == Some(429))
        .expect("client-error row exists")
        .id
        .clone();
    let client_error = gw
        .admin()
        .get_log(&client_error_id)
        .await?
        .expect("client-error log remains");
    assert!(client_error.client_request_body.is_some());
    assert!(client_error.upstream_response_body.is_some());

    let upstream_error_id = rows
        .items
        .iter()
        .find(|i| i.upstream_status_code == Some(503))
        .expect("upstream-error row exists")
        .id
        .clone();
    let upstream_error = gw
        .admin()
        .get_log(&upstream_error_id)
        .await?
        .expect("upstream-error log remains");
    assert!(upstream_error.client_request_body.is_some());
    assert!(upstream_error.upstream_response_body.is_some());

    // Single-row delete; a missing id reports 0 instead of an error.
    assert_eq!(gw.admin().delete_log(&first_ok).await?, 1);
    assert_eq!(gw.admin().delete_log("missing-id").await?, 0);
    let rows = gw.admin().query_logs(LogQuery::default()).await?;
    assert_eq!(rows.total, 3);

    // Error wipe uses client status, so it removes only the client-error row.
    assert_eq!(gw.admin().clear_error_logs().await?, 1);
    let rows = gw.admin().query_logs(LogQuery::default()).await?;
    assert_eq!(rows.total, 2);
    assert_eq!(rows.items[0].client_status_code, Some(200));

    // Full clear still works and removes the remainder.
    assert_eq!(gw.admin().clear_logs().await?, 2);
    let rows = gw.admin().query_logs(LogQuery::default()).await?;
    assert_eq!(rows.total, 0);

    Ok(())
}
