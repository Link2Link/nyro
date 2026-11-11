//! Public connectivity checks must not persist an inconclusive Claude cooldown as failure.
//! Bootstrap background jobs are cancelled before being polled; all HTTP is loopback.

use std::{
    future::Future,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use chrono::Utc;
use nyro_core::{
    Gateway,
    admin::DevinPoolConfig,
    config::GatewayConfig,
    db::models::{CreateProvider, OAuthCredential, UpsertOAuthCredential},
    storage::{MemoryStorage, SqliteStorage, Storage},
};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

const PREVIOUS_TEST_AT: &str = "2001-02-03 04:05:06";
const INTERVAL_MS: i64 = 10 * 60 * 1000;

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

// Match the offline bootstrap used by quota_bucket_backup: never drive the
// startup runtime, then attach SQL storage only after its tasks are cancelled.
fn offline_gateway() -> anyhow::Result<Gateway> {
    let bootstrap = runtime()?;
    let result = {
        let _entered = bootstrap.enter();
        let mut constructor = Box::pin(Gateway::from_storage(
            GatewayConfig {
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            Arc::new(MemoryStorage::new(vec![], vec![], vec![])),
        ));
        let mut context = Context::from_waker(futures::task::noop_waker_ref());
        match constructor.as_mut().poll(&mut context) {
            Poll::Ready(result) => result,
            Poll::Pending => anyhow::bail!("offline Gateway construction yielded"),
        }
    };
    drop(bootstrap);
    let (mut gateway, _logs) = result?;
    gateway.http_client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    Ok(gateway)
}

struct Fixture {
    gateway: Gateway,
    storage: Arc<SqliteStorage>,
    provider_id: String,
    accounts: Vec<OAuthCredential>,
    keys: Vec<String>,
    retry_at: i64,
}

impl Fixture {
    async fn new(mut gateway: Gateway, base: &str, expired_first: bool) -> anyhow::Result<Self> {
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
        gateway.storage = storage.clone();
        let provider = gateway
            .admin()
            .create_provider(CreateProvider {
                name: "Claude connectivity cooldown fixture".into(),
                vendor: Some("anthropic".into()),
                protocol: "anthropic-messages".into(),
                // The account resource_url, not this provider default, owns admission.
                base_url: "http://127.0.0.1:1".into(),
                protocol_mode: "fixed".into(),
                protocol_endpoints: vec![],
                keys: vec![],
                preset_key: Some("anthropic".into()),
                channel: Some("claude-code".into()),
                models_source: None,
                static_models: None,
                api_key: String::new(),
                auth_mode: "oauth".into(),
                use_proxy: false,
                fast_mode: false,
            })
            .await?;
        let observed_at = Utc::now().timestamp_millis();
        let retry_at = observed_at + INTERVAL_MS;
        let mut accounts = Vec::new();
        let mut keys = Vec::new();
        for index in 0..3 {
            let uuid = format!("00000000-0000-4000-8000-{index:012}");
            let mut account = storage
                .oauth_credentials()
                .create_account(
                    &provider.id,
                    &format!("Account {index}"),
                    UpsertOAuthCredential {
                        driver_key: "claude-code".into(),
                        scheme: "oauth_auth_code_pkce".into(),
                        access_token: format!("SECRET-connectivity-{index}"),
                        // No refresh token: even a regressed refresh path fails
                        // locally rather than contacting the real OAuth endpoint.
                        refresh_token: None,
                        expires_at: Some(if expired_first && index == 0 {
                            "2000-01-01T00:00:00Z".into()
                        } else {
                            "2099-01-01T00:00:00Z".into()
                        }),
                        resource_url: Some(base.into()),
                        subject_id: Some(format!("different-subject-{index}")),
                        meta: Some(json!({"account": {"uuid": uuid}}).to_string()),
                        ..Default::default()
                    },
                )
                .await?;
            let state = json!({
                "usage": {"windows": [{"name": "five_hour", "used_percent": 75.0, "resets_at": null}], "plan": "pro"},
                "observed_at": observed_at, "usage_stale": true,
                "auth_invalid": true, "hard_quota": true,
                "hard_quota_reason": "reauthorization_verification", "rejection_revision": 7
            })
            .to_string();
            assert!(
                storage
                    .oauth_credentials()
                    .compare_account_state(
                        &provider.id,
                        &account.account_id,
                        account.status_version,
                        &account.account_state,
                        &state,
                    )
                    .await?
            );
            account = storage
                .oauth_credentials()
                .get_account(&provider.id, &account.account_id)
                .await?
                .expect("seeded account exists");
            // Seed the private durable ledger directly, using its documented
            // origin + stable upstream UUID identity (never provider ID/token).
            let origin = reqwest::Url::parse(base)?.origin().ascii_serialization();
            let scope = serde_json::to_vec(&(origin, uuid))?;
            let key = format!(
                "claude_usage_cooldown.{}",
                hex::encode(Sha256::digest(scope))
            );
            // Third account exercises observed_at fallback with no ledger row.
            if index < 2 {
                storage.settings().set(&key, &retry_at.to_string()).await?;
            }
            keys.push(key);
            accounts.push(account);
        }
        gateway
            .admin()
            .configure_provider_account_pool(&provider.id, DevinPoolConfig::default())
            .await?;
        Ok(Self {
            gateway,
            storage,
            provider_id: provider.id,
            accounts,
            keys,
            retry_at,
        })
    }

    async fn seed_health(&self, success: bool) -> anyhow::Result<()> {
        sqlx::query("UPDATE providers SET last_test_success = ?, last_test_at = ? WHERE id = ?")
            .bind(success)
            .bind(PREVIOUS_TEST_AT)
            .bind(&self.provider_id)
            .execute(self.storage.pool())
            .await?;
        Ok(())
    }

    async fn assert_health_preserved(&self, success: bool) -> anyhow::Result<()> {
        let provider = self.gateway.admin().get_provider(&self.provider_id).await?;
        assert_eq!(provider.last_test_success, Some(success));
        assert_eq!(provider.last_test_at.as_deref(), Some(PREVIOUS_TEST_AT));
        Ok(())
    }
}

#[test]
fn all_cooldown_preserves_sqlite_health_credentials_and_observations() -> anyhow::Result<()> {
    let gateway = offline_gateway()?;
    runtime()?.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let fixture = Fixture::new(gateway, &base, true).await?;
        for previous in [true, false] {
            fixture.seed_health(previous).await?;
            let result = fixture
                .gateway
                .admin()
                .test_provider(&fixture.provider_id)
                .await?;
            assert!(!result.success);
            assert_eq!(result.skipped_reason.as_deref(), Some("usage_cooldown"));
            assert!(result.error.is_none());
            assert_eq!(result.accounts.len(), 3);
            for original in &fixture.accounts {
                let row = result
                    .accounts
                    .iter()
                    .find(|row| row.account_id == original.account_id)
                    .unwrap();
                assert!(!row.success);
                assert_eq!(row.skipped_reason.as_deref(), Some("usage_cooldown"));
                assert_eq!(row.retry_at, Some(fixture.retry_at));
                assert!(row.error.is_none());
                let current = fixture
                    .storage
                    .oauth_credentials()
                    .get_account(&fixture.provider_id, &original.account_id)
                    .await?
                    .unwrap();
                // Includes generation, refresh lease/status/error/timestamp, token,
                // observed_at and all refusal/quota evidence, not merely usage.
                assert_eq!(
                    serde_json::to_value(current)?,
                    serde_json::to_value(original)?
                );
            }
            fixture.assert_health_preserved(previous).await?;
            for key in &fixture.keys[..2] {
                assert_eq!(
                    fixture.storage.settings().get(key).await?,
                    Some(fixture.retry_at.to_string())
                );
            }
            assert!(
                fixture
                    .storage
                    .settings()
                    .get(&fixture.keys[2])
                    .await?
                    .is_none()
            );
            let wire = serde_json::to_string(&result)?;
            assert!(!wire.contains("SECRET-connectivity") && !wire.contains("access_token"));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        Ok(())
    })
}

async fn respond_once(listener: &TcpListener, status: u16) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        let (mut socket, _) = listener.accept().await?;
        let mut request = Vec::new();
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let mut buffer = [0; 1024];
            let read = socket.read(&mut buffer).await?;
            anyhow::ensure!(read > 0 && request.len() < 16_384, "incomplete fixture request");
            request.extend_from_slice(&buffer[..read]);
        }
        let request = String::from_utf8(request)?;
        assert!(request.starts_with("GET /api/oauth/usage "));
        assert!(request.to_ascii_lowercase().contains("authorization: bearer secret-connectivity-0"));
        let body = if status == 200 {
            r#"{"five_hour":{"utilization":21}}"#
        } else {
            "PRIVATE-UPSTREAM-DIAGNOSTIC"
        };
        socket.write_all(format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len(),
        ).as_bytes()).await?;
        Ok(())
    }).await?
}

#[test]
fn mixed_failure_preserves_health_but_expired_cooldown_success_is_persisted() -> anyhow::Result<()>
{
    let gateway = offline_gateway()?;
    runtime()?.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
        let fixture = Fixture::new(gateway, &base, false).await?;
        for (status, previous) in [(401, true), (401, false), (200, false)] {
            fixture.seed_health(previous).await?;
            // Expire only account 0. An explicit old ledger takes precedence
            // over its recent observed_at; siblings remain cooling down.
            fixture
                .storage
                .settings()
                .set(&fixture.keys[0], "0")
                .await?;
            let admin = fixture.gateway.admin();
            let (result, served) = tokio::join!(
                admin.test_provider(&fixture.provider_id),
                respond_once(&listener, status),
            );
            served?;
            let result = result?;
            assert_eq!(result.success, status == 200);
            assert!(result.skipped_reason.is_none());
            assert_eq!(result.error.is_none(), status == 200);
            let tested = result
                .accounts
                .iter()
                .find(|row| row.account_id == fixture.accounts[0].account_id)
                .unwrap();
            assert_eq!(tested.success, status == 200);
            assert!(tested.skipped_reason.is_none() && tested.retry_at.is_none());
            assert_eq!(tested.error.is_none(), status == 200);
            assert_eq!(
                result
                    .accounts
                    .iter()
                    .filter(|row| row.skipped_reason.as_deref() == Some("usage_cooldown"))
                    .count(),
                2
            );
            if status == 200 {
                let provider = admin.get_provider(&fixture.provider_id).await?;
                assert_eq!(provider.last_test_success, Some(true));
                assert!(provider.last_test_at.is_some());
                assert_ne!(provider.last_test_at.as_deref(), Some(PREVIOUS_TEST_AT));
            } else {
                fixture.assert_health_preserved(previous).await?;
            }
            let wire = serde_json::to_string(&result)?;
            assert!(!wire.contains("PRIVATE-UPSTREAM") && !wire.contains("SECRET-connectivity"));
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(25), listener.accept())
                .await
                .is_err()
        );
        Ok(())
    })
}
