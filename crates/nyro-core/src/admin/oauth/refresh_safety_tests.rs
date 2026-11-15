//! Offline orchestration regressions: synthetic grants, no background networking.
use super::*;
use crate::auth::{refresh::RefreshFailure, refresh_state};
use std::{
    future::Future,
    sync::atomic::AtomicUsize,
    task::{Context, Poll},
};
use tokio::sync::Notify;

struct FixtureDriver {
    driver: &'static str,
    failure: Option<RefreshFailure>,
    started: Arc<Notify>,
    release: Option<Arc<Notify>>,
    calls: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl auth::AuthDriver for FixtureDriver {
    fn metadata(&self) -> auth::AuthDriverMetadata {
        auth::AuthDriverMetadata {
            key: self.driver,
            label: "Refresh fixture",
            scheme: AuthScheme::OAuthAuthCodePkce,
            supports_new_provider: true,
            supports_existing_provider: true,
        }
    }
    async fn refresh(
        &self,
        _: &StoredCredential,
        _: RefreshAuthContext,
    ) -> anyhow::Result<CredentialBundle> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if let Some(release) = &self.release {
            release.notified().await;
        }
        if let Some(failure) = &self.failure {
            return Err(failure.clone().into());
        }
        Ok(CredentialBundle {
            access_token: Some("new-access".into()),
            refresh_token: Some("new-refresh".into()),
            expires_at: Some("2099-01-01T00:00:00Z".into()),
            raw: serde_json::json!({"account_id":"fixture"}),
            ..Default::default()
        })
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn provider(driver: &str) -> Provider {
    let (vendor, channel) = if driver == "claude-code" {
        ("anthropic", "claude-code")
    } else {
        ("openai", "codex")
    };
    serde_json::from_value(serde_json::json!({
        "id":"refresh-fixture", "name":"Refresh fixture", "vendor":vendor, "preset_key":vendor,
        "channel":channel, "protocol":"openai-responses", "protocol_mode":"fixed",
        "base_url":"http://127.0.0.1:9", "api_key":"", "auth_mode":"oauth",
        "is_enabled":false, "created_at":"", "updated_at":""
    }))
    .unwrap()
}

fn gateway(provider: &Provider) -> Gateway {
    let storage = Arc::new(crate::storage::MemoryStorage::new(
        vec![provider.clone()],
        vec![],
        vec![],
    ));
    let bootstrap = runtime();
    let result = {
        let _entered = bootstrap.enter();
        let mut init = Box::pin(Gateway::from_storage(
            crate::config::GatewayConfig {
                config_poll_interval: Duration::ZERO,
                ..Default::default()
            },
            storage,
        ));
        match init
            .as_mut()
            .poll(&mut Context::from_waker(futures::task::noop_waker_ref()))
        {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => panic!("offline construction must not drive background tasks"),
        }
    };
    drop(bootstrap);
    result.0
}

fn input(driver: &str) -> UpsertOAuthCredential {
    UpsertOAuthCredential {
        driver_key: driver.into(),
        scheme: "oauth_auth_code_pkce".into(),
        access_token: "old-access".into(),
        refresh_token: Some("old-refresh".into()),
        expires_at: Some("2000-01-01T00:00:00Z".into()),
        meta: Some(serde_json::json!({"account_id":"fixture", "keep":"metadata"}).to_string()),
        ..Default::default()
    }
}

fn fixture(
    driver: &'static str,
    failure: Option<RefreshFailure>,
    pause: bool,
) -> Arc<FixtureDriver> {
    Arc::new(FixtureDriver {
        driver,
        failure,
        started: Arc::new(Notify::new()),
        release: pause.then(|| Arc::new(Notify::new())),
        calls: Arc::new(AtomicUsize::new(0)),
    })
}

#[test]
fn refresh_unknown_is_persisted_and_cannot_be_retried_until_reauthorization() {
    let p = provider("codex");
    let gw = gateway(&p);
    runtime().block_on(async {
        let admin = gw.admin();
        let store = gw.storage.oauth_credentials();
        for failure in [
            RefreshFailure::OutcomeUnknown { stage: "read" },
            RefreshFailure::Rejected { status: 400 },
        ] {
            let row = store.upsert(&p.id, input("codex")).await.unwrap();
            let lease = store
                .try_begin_refresh(&p.id, row.status_version)
                .await
                .unwrap()
                .unwrap();
            let driver = fixture("codex", Some(failure), false);
            assert!(
                admin
                    .refresh_locked_credential(
                        &p,
                        &stored_credential_from_oauth(&row, "codex"),
                        &lease,
                        driver.clone()
                    )
                    .await
                    .is_err()
            );
            let current = store.get(&p.id).await.unwrap().unwrap();
            assert_eq!(current.status, "error");
            assert!(
                !current
                    .last_error
                    .as_deref()
                    .unwrap()
                    .contains("old-refresh")
            );
            assert!(
                store
                    .try_begin_refresh(&p.id, current.status_version)
                    .await
                    .unwrap()
                    .is_none()
            );
            assert!(admin.reconnect_provider_oauth(&p.id).await.is_err());
            assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        }
        let rebound = store.upsert(&p.id, input("codex")).await.unwrap();
        assert_eq!(rebound.status, "connected");
        assert!(
            store
                .try_begin_refresh(&p.id, rebound.status_version)
                .await
                .unwrap()
                .is_some()
        );
    });
}

#[test]
fn refresh_presend_failure_can_retry_but_untyped_codex_error_is_quarantined() {
    let p = provider("codex");
    let gw = gateway(&p);
    runtime().block_on(async {
        let store = gw.storage.oauth_credentials();
        let row = store.upsert(&p.id, input("codex")).await.unwrap();
        let lease = store
            .try_begin_refresh(&p.id, row.status_version)
            .await
            .unwrap()
            .unwrap();
        let driver = fixture("codex", Some(RefreshFailure::PreSend), false);
        assert!(
            gw.admin()
                .refresh_locked_credential(
                    &p,
                    &stored_credential_from_oauth(&row, "codex"),
                    &lease,
                    driver
                )
                .await
                .is_err()
        );
        let current = store.get(&p.id).await.unwrap().unwrap();
        assert_eq!(current.status, "connected");
        assert!(
            store
                .try_begin_refresh(&p.id, current.status_version)
                .await
                .unwrap()
                .is_some()
        );
        let (terminal, _, message) = refresh_failure_policy(
            &stored_credential_from_oauth(&row, "codex"),
            &anyhow::anyhow!("transport SECRET"),
        );
        assert!(terminal);
        assert_eq!(message, refresh_state::OUTCOME_UNKNOWN_MESSAGE);
    });
}

#[test]
fn claude_retry_after_blocks_admin_and_account_refresh_without_touching_siblings() {
    let p = provider("claude-code");
    let gw = gateway(&p);
    runtime().block_on(async {
        let admin = gw.admin();
        let store = gw.storage.oauth_credentials();
        let row = store.upsert(&p.id, input("claude-code")).await.unwrap();
        let lease = store
            .try_begin_account_refresh(&p.id, &row.account_id, row.status_version)
            .await
            .unwrap()
            .unwrap();
        let deadline = Utc::now() + chrono::Duration::minutes(10);
        let driver = fixture(
            "claude-code",
            Some(RefreshFailure::RateLimited { retry_at: deadline }),
            false,
        );
        assert!(
            admin
                .refresh_locked_credential(
                    &p,
                    &stored_credential_from_oauth(&row, "claude-code"),
                    &lease,
                    driver.clone()
                )
                .await
                .is_err()
        );
        let current = store.get(&p.id).await.unwrap().unwrap();
        assert_eq!(current.status, "connected");
        let meta: Value = serde_json::from_str(&current.meta).unwrap();
        assert_eq!(refresh_state::retry_at(&meta), Some(deadline));
        assert_eq!(meta["keep"], "metadata");
        assert!(admin.reconnect_provider_oauth(&p.id).await.is_err());
        assert!(
            admin
                .refresh_claude_pool_account(&p, &current)
                .await
                .is_err()
        );
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        let sibling = store
            .create_account(
                &p.id,
                "sibling",
                UpsertOAuthCredential {
                    access_token: "sibling-token".into(),
                    ..input("claude-code")
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .try_begin_account_refresh(&p.id, &sibling.account_id, sibling.status_version)
                .await
                .unwrap()
                .is_some()
        );
    });
}

#[test]
fn caller_cancellation_does_not_discard_a_rotated_codex_credential() {
    let p = provider("codex");
    let gw = gateway(&p);
    runtime().block_on(async {
        let store = gw.storage.oauth_credentials();
        let row = store.upsert(&p.id, input("codex")).await.unwrap();
        let lease = store
            .try_begin_refresh(&p.id, row.status_version)
            .await
            .unwrap()
            .unwrap();
        let driver = fixture("codex", None, true);
        let admin = gw.admin();
        let (p2, driver2) = (p.clone(), driver.clone());
        let task = tokio::spawn(async move {
            admin
                .refresh_locked_credential(
                    &p2,
                    &stored_credential_from_oauth(&row, "codex"),
                    &lease,
                    driver2,
                )
                .await
        });
        driver.started.notified().await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        driver.release.as_ref().unwrap().notify_one();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let row = store.get(&p.id).await.unwrap().unwrap();
                if row.status == "connected" && row.access_token == "new-access" {
                    assert_eq!(row.refresh_token.as_deref(), Some("new-refresh"));
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn reauthorization_fences_a_late_detached_refresh_result() {
    let p = provider("codex");
    let gw = gateway(&p);
    for same_tokens in [false, true] {
        runtime().block_on(async {
            let store = gw.storage.oauth_credentials();
            let row = store.upsert(&p.id, input("codex")).await.unwrap();
            let lease = store
                .try_begin_refresh(&p.id, row.status_version)
                .await
                .unwrap()
                .unwrap();
            let driver = fixture("codex", None, true);
            let admin = gw.admin();
            let (p2, driver2) = (p.clone(), driver.clone());
            let task = tokio::spawn(async move {
                admin
                    .refresh_locked_credential(
                        &p2,
                        &stored_credential_from_oauth(&row, "codex"),
                        &lease,
                        driver2,
                    )
                    .await
            });
            driver.started.notified().await;
            store
                .upsert(
                    &p.id,
                    UpsertOAuthCredential {
                        access_token: if same_tokens {
                            "new-access"
                        } else {
                            "reauthorized"
                        }
                        .into(),
                        refresh_token: Some(
                            if same_tokens {
                                "new-refresh"
                            } else {
                                "new-login-grant"
                            }
                            .into(),
                        ),
                        subject_id: Some("different-account".into()),
                        ..input("codex")
                    },
                )
                .await
                .unwrap();
            driver.release.as_ref().unwrap().notify_one();
            assert!(task.await.unwrap().is_err());
            assert_eq!(
                store.get(&p.id).await.unwrap().unwrap().access_token,
                if same_tokens {
                    "new-access"
                } else {
                    "reauthorized"
                }
            );
            assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        });
    }
}

#[test]
fn refresh_http_client_never_follows_redirects() {
    let p = provider("codex");
    let gw = gateway(&p);
    runtime().block_on(async {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let followed = Arc::new(AtomicUsize::new(0));
        let count = followed.clone();
        let app = axum::Router::new()
            .route(
                "/token",
                axum::routing::post(|| async {
                    (
                        axum::http::StatusCode::TEMPORARY_REDIRECT,
                        [("location", "/should-not-see-secret")],
                    )
                }),
            )
            .route(
                "/should-not-see-secret",
                axum::routing::post(move || {
                    let count = count.clone();
                    async move {
                        count.fetch_add(1, Ordering::SeqCst);
                        "unexpected"
                    }
                }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = gw.oauth_refresh_http_client(false).await.unwrap();
        let response = client
            .post(format!("http://{address}/token"))
            .body("synthetic-grant")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::TEMPORARY_REDIRECT);
        assert_eq!(followed.load(Ordering::SeqCst), 0);
        server.abort();
    });
}

#[test]
fn publication_failure_never_replays_the_codex_token_request() {
    use crate::storage::{SqliteStorage, traits::Storage};
    let mut gw = gateway(&provider("codex"));
    runtime().block_on(async {
        let directory = tempfile::tempdir().unwrap();
        let pool = crate::db::init_pool(directory.path()).await.unwrap();
        crate::db::migrate(&pool).await.unwrap();
        let storage = Arc::new(SqliteStorage::from_pool(pool.clone()));
        let p = storage.providers().create(serde_json::from_value(serde_json::json!({
            "name":"Publication failure", "vendor":"openai", "channel":"codex",
            "protocol":"openai-responses", "base_url":"http://127.0.0.1:9", "api_key":"",
            "auth_mode":"oauth", "is_enabled":false
        })).unwrap()).await.unwrap();
        gw.storage = storage.clone();
        let store = storage.oauth_credentials();
        let row = store.upsert(&p.id, input("codex")).await.unwrap();
        let lease = store.try_begin_refresh(&p.id, row.status_version).await.unwrap().unwrap();
        // Only the disposable fixture database is affected. Simulate a successful
        // rotation whose new token cannot be published; quarantine must still work.
        sqlx::query("CREATE TRIGGER reject_rotated_token BEFORE UPDATE OF access_token ON provider_oauth_credentials WHEN NEW.access_token = 'new-access' BEGIN SELECT RAISE(FAIL, 'fixture publication denied'); END")
            .execute(&pool).await.unwrap();
        let driver = fixture("codex", None, false);
        let error = gw.admin().refresh_locked_credential(
            &p, &stored_credential_from_oauth(&row, "codex"), &lease, driver.clone(),
        ).await.unwrap_err();
        assert!(matches!(error.downcast_ref::<RefreshFailure>(), Some(RefreshFailure::OutcomeUnknown { stage: "credential publication" })));
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        let failed = store.get(&p.id).await.unwrap().unwrap();
        assert_eq!(failed.status, "error");
        assert_eq!(failed.access_token, "old-access");
        assert_eq!(failed.last_error.as_deref(), Some(refresh_state::OUTCOME_UNKNOWN_MESSAGE));
        sqlx::query("DROP TRIGGER reject_rotated_token").execute(&pool).await.unwrap();
        assert!(store.try_begin_refresh(&p.id, failed.status_version).await.unwrap().is_none());
        pool.close().await;
        let reopened_pool = crate::db::init_pool(directory.path()).await.unwrap();
        let reopened = SqliteStorage::from_pool(reopened_pool.clone());
        let persisted = reopened.oauth_credentials().get(&p.id).await.unwrap().unwrap();
        assert_eq!(persisted.status, "error", "a restart cannot authorize replay");
        assert!(reopened.oauth_credentials().try_begin_refresh(&p.id, persisted.status_version).await.unwrap().is_none());
        reopened_pool.close().await;
    });
}

#[test]
fn codex_refresh_deadline_is_unknown_not_retryable() {
    let p = provider("codex");
    let gw = gateway(&p);
    runtime().block_on(async {
        tokio::time::pause();
        let store = gw.storage.oauth_credentials();
        let row = store.upsert(&p.id, input("codex")).await.unwrap();
        let lease = store
            .try_begin_refresh(&p.id, row.status_version)
            .await
            .unwrap()
            .unwrap();
        let driver = fixture("codex", None, true);
        let error = gw
            .admin()
            .refresh_locked_credential(
                &p,
                &stored_credential_from_oauth(&row, "codex"),
                &lease,
                driver.clone(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<RefreshFailure>(),
            Some(RefreshFailure::OutcomeUnknown { .. })
        ));
        assert_eq!(driver.calls.load(Ordering::SeqCst), 1);
        assert_eq!(store.get(&p.id).await.unwrap().unwrap().status, "error");
    });
}
