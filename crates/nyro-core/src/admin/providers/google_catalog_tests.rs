use super::*;
use crate::auth::{AuthDriver, drivers::GoogleSubscriptionDriver};
use crate::provider::google::antigravity::{
    catalog_source_for_runtime, credential_catalog_snapshot,
};

async fn fixture() -> (tempfile::TempDir, Gateway, Provider, OAuthCredential) {
    use std::{
        future::Future,
        sync::Arc,
        task::{Context, Poll},
    };
    let directory = tempfile::tempdir().unwrap();
    let provider: Provider = serde_json::from_value(serde_json::json!({
        "id":"google-catalog-fixture", "name":"Google catalog fixture",
        "vendor":"google", "channel":"gemini-cli", "preset_key":"google",
        "protocol":"google-gemini", "protocol_mode":"fixed", "protocol_endpoints":[],
        "base_url":"https://cloudcode-pa.googleapis.com", "api_key":"", "auth_mode":"oauth",
        "is_enabled":false, "created_at":"", "updated_at":""
    }))
    .unwrap();
    let storage = Arc::new(crate::storage::MemoryStorage::new(
        vec![provider.clone()],
        vec![],
        vec![],
    ));
    // Construct without ever polling startup network jobs. A separate runtime
    // is dropped before this test drives catalog requests on loopback.
    let gw = std::thread::spawn(move || {
        let bootstrap = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
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
                Poll::Ready(result) => result.unwrap().0,
                Poll::Pending => {
                    panic!("offline construction must not poll background network tasks")
                }
            }
        };
        drop(bootstrap);
        result
    })
    .join()
    .unwrap();
    let row = gw
        .storage
        .oauth_credentials()
        .upsert(
            &provider.id,
            UpsertOAuthCredential {
                driver_key: "google-gemini-cli".into(),
                scheme: "oauth_auth_code_pkce".into(),
                access_token: "fixture-token".into(),
                refresh_token: Some("fixture-refresh".into()),
                meta: Some(
                    serde_json::json!({"project_id":"fixture-project", "unrelated":"preserve"})
                        .to_string(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    (directory, gw, provider, row)
}

fn runtime(provider: &Provider, row: &OAuthCredential, endpoint: &str) -> ResolvedProviderRuntime {
    let credential = super::super::auth_data::stored_credential_from_oauth(row, &row.driver_key);
    let mut binding = GoogleSubscriptionDriver::GEMINI_CLI
        .bind_runtime(provider, &credential)
        .unwrap();
    binding.base_url_override = Some(endpoint.into());
    binding
        .extra_headers
        .insert("user-agent".into(), "bound-catalog-test/9".into());
    ResolvedProviderRuntime {
        access_token: row.access_token.clone(),
        binding,
        credential: Some(credential),
        claude_observation_account: None,
    }
}

#[tokio::test]
async fn google_catalog_admin_uses_runtime_binding_and_persists_empty_success() {
    let (_directory, gw, provider, row) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(
            |headers: HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                assert_eq!(headers["authorization"], "Bearer fixture-token");
                assert_eq!(headers["user-agent"], "bound-catalog-test/9");
                assert_eq!(body["project"], "fixture-project");
                axum::Json(serde_json::json!({"models":{}}))
            },
        ),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let runtime = runtime(&provider, &row, &endpoint);
    let models = gw
        .admin()
        .google_subscription_catalog(&provider, Some(&runtime), false)
        .await
        .unwrap();
    server.abort();
    assert_eq!(
        models,
        Some(Vec::new()),
        "successful empty is not a failed fetch"
    );
    let stored = gw
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await
        .unwrap()
        .unwrap();
    let meta: Value = serde_json::from_str(&stored.meta).unwrap();
    assert_eq!(meta["subscription_models"], serde_json::json!([]));
    assert_eq!(meta["subscription_models_source"]["endpoint"], endpoint);
    assert_eq!(
        meta["subscription_models_source"]["user_agent"],
        "bound-catalog-test/9"
    );
    assert_eq!(meta["unrelated"], "preserve");
    assert_eq!(stored.access_token, row.access_token);
    assert_eq!(stored.refresh_token, row.refresh_token);
    assert_eq!(stored.account_state, row.account_state);
    assert_eq!(stored.last_refresh_at, row.last_refresh_at);
}

#[tokio::test]
async fn google_catalog_admin_does_not_overwrite_new_credential_after_async_fetch() {
    use std::sync::Arc;
    let (_directory, gw, provider, row) = fixture().await;
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let signals = (started.clone(), release.clone());
    let app = axum::Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(move || {
            let (started, release) = signals.clone();
            async move {
                started.notify_one();
                release.notified().await;
                axum::Json(serde_json::json!({"models":{"old-model":{}}}))
            }
        }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let runtime = runtime(&provider, &row, &endpoint);
    let admin = gw.admin();
    let fetching = admin.google_subscription_catalog(&provider, Some(&runtime), true);
    let rebinding = async {
        started.notified().await;
        gw.storage
            .oauth_credentials()
            .upsert(
                &provider.id,
                crate::db::models::UpsertOAuthCredential {
                    driver_key: row.driver_key.clone(),
                    scheme: row.scheme.clone(),
                    access_token: "replacement-token".into(),
                    refresh_token: Some("replacement-refresh".into()),
                    meta: Some(
                        serde_json::json!({"project_id":"new-project", "new_meta":true})
                            .to_string(),
                    ),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        release.notify_one();
    };
    let (result, ()) = tokio::join!(fetching, rebinding);
    server.abort();
    assert!(result.is_ok());
    let stored = gw
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.access_token, "replacement-token");
    assert_eq!(stored.refresh_token.as_deref(), Some("replacement-refresh"));
    let meta: Value = serde_json::from_str(&stored.meta).unwrap();
    assert_eq!(
        meta,
        serde_json::json!({"project_id":"new-project", "new_meta":true})
    );
}

#[tokio::test]
async fn google_catalog_admin_failure_preserves_snapshot_and_strict_error() {
    let (_directory, gw, provider, row) = fixture().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let app = axum::Router::new().route(
        "/v1internal:fetchAvailableModels",
        axum::routing::post(|| async { axum::http::StatusCode::NOT_FOUND }),
    );
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let runtime = runtime(&provider, &row, &endpoint);
    let admin = gw.admin();
    assert!(
        admin
            .google_subscription_catalog(&provider, Some(&runtime), false)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        admin
            .google_subscription_catalog(&provider, Some(&runtime), true)
            .await
            .is_err()
    );
    server.abort();
    let stored = gw
        .storage
        .oauth_credentials()
        .get(&provider.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.meta, row.meta);
    assert_eq!(stored.status_version, row.status_version);
    let credential = runtime.credential.unwrap();
    assert!(credential_catalog_snapshot(&provider, Some(&credential)).is_none());
    let _source = catalog_source_for_runtime(&provider, &credential, &runtime.binding).unwrap();
}
