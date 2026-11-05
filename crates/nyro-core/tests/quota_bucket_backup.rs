//! Quota identity is local runtime state, not portable backup configuration.
//!
//! Exercise public AdminService export/import and setting writes with isolated
//! in-memory SQLite. Gateway startup is single-polled on an undriven runtime so
//! models.dev, usage and OAuth jobs are cancelled before any network-capable
//! task is polled. No upstream requests or real credentials are needed.

use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider, ExportData},
    quota_buckets::{ManualQuotaBucketBinding, QuotaBucketConfig},
    storage::{MemoryStorage, SqliteStorage, Storage},
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

const ORDINARY_KEY: &str = "quota_backup_test_note";
const ORDINARY_VALUE: &str = "Portable ordinary setting";
const PRIVATE_PREFIXES: [&str; 3] = [
    "quota_bucket_owner.",
    "quota_bucket_fence.",
    "quota_buckets.",
];

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

fn offline_gateway() -> anyhow::Result<Gateway> {
    let bootstrap = runtime()?;
    let result = {
        let _entered = bootstrap.enter();
        let storage = Arc::new(MemoryStorage::new(vec![], vec![], vec![]));
        let mut constructor = Box::pin(Gateway::from_storage(
            GatewayConfig {
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            storage,
        ));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        match constructor.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => anyhow::bail!(
                "offline Gateway construction yielded; refusing to run background network tasks"
            ),
        }
    };
    drop(bootstrap);
    let (gateway, _logs) = result?;
    Ok(gateway)
}

/// Attach a real SQL storage only after bootstrap's network-capable tasks have
/// been cancelled. SQL workers and migration touch this disposable DB alone.
async fn sqlite_gateway(mut gateway: Gateway) -> anyhow::Result<Gateway> {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            SqliteConnectOptions::new()
                .in_memory(true)
                .foreign_keys(true),
        )
        .await?;
    let storage = Arc::new(SqliteStorage::from_pool(pool));
    storage.bootstrap().migrate().await?;
    gateway.storage = storage;
    Ok(gateway)
}

async fn seed_configuration(gateway: &Gateway) -> anyhow::Result<String> {
    let admin = gateway.admin();
    let provider = admin
        .create_provider(CreateProvider {
            name: "Offline backup provider".into(),
            vendor: Some("custom".into()),
            protocol: "openai-compatible".into(),
            base_url: "http://127.0.0.1:1/v1".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: Some("default".into()),
            models_source: None,
            static_models: None,
            api_key: "inert-offline-backup-key".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    admin
        .create_model(CreateModel {
            name: "offline-backup-route".into(),
            balance: Some("priority".into()),
            target_provider: provider.id.clone(),
            target_model: "gemini-3.8-flash".into(),
            targets: vec![],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: Some(false),
            vision_shim: None,
        })
        .await?;
    admin.set_setting(ORDINARY_KEY, ORDINARY_VALUE).await?;
    Ok(provider.id)
}

fn is_private_setting(key: &str) -> bool {
    PRIVATE_PREFIXES
        .iter()
        .any(|prefix| key.starts_with(prefix))
}

#[test]
fn export_excludes_quota_owner_fences_and_live_resource_bindings() -> anyhow::Result<()> {
    let gateway = offline_gateway()?;
    runtime()?.block_on(async {
        let gateway = sqlite_gateway(gateway).await?;
        let provider_id = seed_configuration(&gateway).await?;
        let mut local_settings = Vec::new();
        for id in [provider_id.as_str(), "retired-provider"] {
            for prefix in PRIVATE_PREFIXES {
                let key = format!("{prefix}{id}");
                let value = format!("local-nonportable-evidence:{prefix}{id}");
                // Direct fixture seeding is intentional: public setting APIs
                // must not be used to bypass scoped owner/binding validation.
                gateway.storage.settings().set(&key, &value).await?;
                local_settings.push((key, value));
            }
        }
        let backup = gateway.admin().export_config().await?;
        let json = serde_json::to_string(&backup)?;
        let backup: ExportData = serde_json::from_str(&json)?;
        assert_eq!(backup.providers.len(), 1);
        assert_eq!(backup.models.len(), 1);
        assert!(
            backup
                .settings
                .iter()
                .any(|(key, value)| key == ORDINARY_KEY && value == ORDINARY_VALUE)
        );
        assert!(
            backup
                .settings
                .iter()
                .all(|(key, _)| !is_private_setting(key)),
            "backup must not copy owner identities, restart fences or discovered bindings"
        );
        assert!(!json.contains("local-nonportable-evidence"));
        for (key, value) in local_settings {
            assert_eq!(
                gateway.storage.settings().get(&key).await?,
                Some(value),
                "export filtering must not delete live local evidence"
            );
        }
        Ok(())
    })
}

#[test]
fn importing_old_or_hostile_quota_settings_skips_them_without_partial_failure() -> anyhow::Result<()>
{
    let source = offline_gateway()?;
    let destination_v1 = offline_gateway()?;
    let destination_v2 = offline_gateway()?;
    runtime()?.block_on(async {
        let source = sqlite_gateway(source).await?;
        let source_id = seed_configuration(&source).await?;
        let mut backup = source.admin().export_config().await?;
        // Keep the exact setting count deterministic, independent of internal
        // config_epoch updates caused by normal provider/model creation.
        backup.settings.retain(|(key, _)| key == ORDINARY_KEY);
        for id in [source_id.as_str(), "nonexistent-source-provider"] {
            for prefix in PRIVATE_PREFIXES {
                // Even malformed legacy/attacker values must be skipped, not
                // parsed or sent through public internal-setting rejection
                // after providers have already been created.
                backup.settings.push((
                    format!("{prefix}{id}"),
                    "not valid JSON; do not restore".into(),
                ));
            }
        }
        backup.settings.push((
            "quota_backup_after_private_rows".into(),
            "Imported completely".into(),
        ));
        for (version, destination) in [(1, destination_v1), (2, destination_v2)] {
            let destination = sqlite_gateway(destination).await?;
            let mut incoming = backup.clone();
            incoming.version = version;
            let incoming: ExportData = serde_json::from_str(&serde_json::to_string(&incoming)?)?;
            let result = destination.admin().import_config(incoming).await?;
            assert_eq!(result.providers_imported, 1, "backup version {version}");
            assert_eq!(result.models_imported, 1, "backup version {version}");
            assert_eq!(
                result.settings_imported, 2,
                "only portable settings count as imported"
            );
            let providers = destination.admin().list_providers().await?;
            assert_eq!(providers.len(), 1);
            assert_eq!(providers[0].name, "Offline backup provider");
            assert_ne!(
                providers[0].id, source_id,
                "import creates a new real provider identity"
            );
            let models = destination.admin().list_models().await?;
            assert_eq!(models.len(), 1);
            assert_eq!(models[0].name, "offline-backup-route");
            assert_eq!(models[0].target_provider, providers[0].id);
            assert_eq!(
                destination
                    .admin()
                    .get_setting(ORDINARY_KEY)
                    .await?
                    .as_deref(),
                Some(ORDINARY_VALUE)
            );
            assert_eq!(
                destination
                    .admin()
                    .get_setting("quota_backup_after_private_rows")
                    .await?
                    .as_deref(),
                Some("Imported completely")
            );
            let settings = destination.storage.settings().list_all().await?;
            assert!(
                settings.iter().all(|(key, _)| !is_private_setting(key)),
                "old or new provider IDs must not inherit copied quota identities: {settings:?}"
            );
            assert!(
                destination
                    .quota_registry
                    .bucket_snapshot(&providers[0].id)
                    .is_empty()
            );
            assert!(
                destination
                    .quota_registry
                    .bucket_snapshot(&source_id)
                    .is_empty()
            );
            assert!(
                destination
                    .admin()
                    .get_provider_quota_buckets_config(&providers[0].id)
                    .await?
                    .bindings
                    .is_empty()
            );
            let reexported = destination.admin().export_config().await?;
            assert_eq!(reexported.providers.len(), 1);
            assert_eq!(reexported.models.len(), 1);
            assert!(
                reexported
                    .settings
                    .iter()
                    .all(|(key, _)| !is_private_setting(key))
            );
        }
        Ok(())
    })
}

#[test]
fn generic_setting_writes_cannot_bypass_quota_identity_or_binding_validation() -> anyhow::Result<()>
{
    let gateway = offline_gateway()?;
    runtime()?.block_on(async {
        let gateway = sqlite_gateway(gateway).await?;
        let provider_id = seed_configuration(&gateway).await?;
        let admin = gateway.admin();
        for prefix in ["quota_bucket_owner.", "quota_bucket_fence."] {
            let key = format!("{prefix}{provider_id}");
            gateway
                .storage
                .settings()
                .set(&key, "destination-owned-evidence")
                .await?;
            assert!(
                admin
                    .set_setting(&key, "attacker-replacement")
                    .await
                    .is_err()
            );
            assert_eq!(
                gateway.storage.settings().get(&key).await?.as_deref(),
                Some("destination-owned-evidence")
            );
        }
        let key = format!("quota_buckets.{provider_id}");
        let config = QuotaBucketConfig {
            revision: 0,
            bindings: vec![ManualQuotaBucketBinding {
                id: "injected-aggregate".into(),
                scope_id: "foreign-owner".into(),
                label: "Foreign Flash".into(),
                models: vec!["gemini-3.8-flash".into()],
                constraint_ids: vec!["undiscovered-foreign-resource".into()],
            }],
        };
        assert!(
            admin
                .set_setting(&key, &serde_json::to_string(&config)?)
                .await
                .is_err(),
            "generic setting writes must validate discovered owner-scoped resources"
        );
        assert!(gateway.storage.settings().get(&key).await?.is_none());
        assert!(
            admin
                .get_provider_quota_buckets_config(&provider_id)
                .await?
                .bindings
                .is_empty()
        );
        assert!(
            gateway
                .quota_registry
                .bucket_snapshot(&provider_id)
                .is_empty()
        );
        assert_eq!(
            admin.get_setting(ORDINARY_KEY).await?.as_deref(),
            Some(ORDINARY_VALUE)
        );
        Ok(())
    })
}
