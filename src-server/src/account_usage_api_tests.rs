use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use nyro_core::storage::{SqliteStorage, Storage};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn account_usage_http_contract_auth_and_server_owned_cycles() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = nyro_core::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let provider = storage
        .providers()
        .create(CreateProvider {
            name: "Devin test".into(),
            vendor: Some("devin".into()),
            protocol: "openai/chat".into(),
            base_url: "https://example.invalid".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: String::new(),
            auth_mode: "oauth".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    let account = storage
        .oauth_credentials()
        .create_account(
            &provider.id,
            "Test account",
            UpsertOAuthCredential {
                driver_key: "devin".into(),
                scheme: "session_token".into(),
                access_token: "fake-test-token".into(),
                ..Default::default()
            },
        )
        .await?;
    let reset = chrono::Utc::now().timestamp() + 86_400;
    assert!(storage.oauth_credentials().compare_account_state(
        &provider.id, &account.account_id, account.status_version, &account.account_state,
        &serde_json::json!({"usage": {"weekly": {"remaining_percent": 80, "resets_at_unix": reset}}}).to_string(),
    ).await?);
    let router = create_router(gw, Some("test-admin".into()));
    for (query, authenticated, expected) in [
        ("", false, StatusCode::UNAUTHORIZED),
        ("", true, StatusCode::OK),
        // Legacy client parameters cannot override account-owned cycle dates.
        ("?period_start=0", true, StatusCode::OK),
    ] {
        let mut request = Request::builder().uri(format!(
            "/api/v1/providers/{}/accounts/usage-stats{query}",
            provider.id
        ));
        if authenticated {
            request = request.header("authorization", "Bearer test-admin");
        }
        let response = router.clone().oneshot(request.body(Body::empty())?).await?;
        assert_eq!(response.status(), expected, "query {query}");
        if expected == StatusCode::OK {
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(value["data"].as_array().unwrap().len(), 1);
            assert_eq!(value["data"][0]["account_id"], account.account_id);
            assert_eq!(
                value["data"][0]["period_start"],
                (reset - 7 * 86_400) * 1000
            );
            assert_eq!(value["data"][0]["period_end"], reset * 1000);
            for window in ["all_time", "period"] {
                assert_eq!(value["data"][0][window]["total_tokens"], 0);
                assert_eq!(value["data"][0][window]["request_count"], 0);
                assert_eq!(
                    value["data"][0][window]["equivalent_costs"],
                    serde_json::json!([])
                );
                assert_eq!(value["data"][0][window]["unpriced_request_count"], 0);
            }
            assert!(!String::from_utf8_lossy(&bytes).contains("fake-test-token"));
        }
    }
    Ok(())
}
