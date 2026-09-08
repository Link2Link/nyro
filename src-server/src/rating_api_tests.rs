use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn fixture() -> anyhow::Result<(tempfile::TempDir, Router, String)> {
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::new(nyro_core::config::GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let p = gw
        .admin()
        .create_provider(CreateProvider {
            name: "ratings-http".to_string(),
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
        })
        .await?;
    Ok((dir, create_router(gw, Some("test-admin".to_string())), p.id))
}

async fn call(
    router: &Router,
    method: &str,
    url: &str,
    body: Option<Value>,
    authenticated: bool,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder().method(method).uri(url);
    if authenticated {
        request = request.header("authorization", "Bearer test-admin");
    }
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&body)?)
        }
        None => Body::empty(),
    };
    let response = router.clone().oneshot(request.body(body)?).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

#[tokio::test]
async fn rating_http_auth_encoded_identity_and_unrated_zero_contract() -> anyhow::Result<()> {
    let (_dir, router, id) = fixture().await?;
    let url = format!("/api/v1/providers/{id}/model-rating?model=vendor%2FModel%2Bx%23%20");
    assert_eq!(
        call(&router, "GET", &url, None, false).await?.0,
        StatusCode::UNAUTHORIZED
    );
    let (status, state) = call(&router, "GET", &url, None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(state["data"]["status"], "unrated");
    assert!(state["data"]["score"].is_null());
    assert!(state["data"]["updated_at"].is_null());
    assert_eq!(state["data"]["upstream_model"], "vendor/Model+x# ");
    let (status, saved) = call(&router, "PUT", &url, Some(json!({"score":0})), true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["data"]["score"], 0);
    let (_, state) = call(&router, "GET", &url, None, true).await?;
    assert_eq!(state["data"]["status"], "rated");
    assert_eq!(state["data"]["score"], 0);
    let (_, list) = call(&router, "GET", "/api/v1/provider-model-ratings", None, true).await?;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    let trimmed = format!("/api/v1/providers/{id}/model-rating?model=vendor%2FModel%2Bx%23");
    assert_eq!(
        call(&router, "GET", &trimmed, None, true).await?.1["data"]["status"],
        "unrated"
    );
    for _ in 0..2 {
        let (status, result) = call(&router, "DELETE", &url, None, true).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result, json!({"ok":true}));
    }
    assert_eq!(
        call(&router, "GET", &url, None, true).await?.1["data"]["status"],
        "unrated"
    );
    Ok(())
}

#[tokio::test]
async fn rating_http_rejects_bad_input_without_coercion() -> anyhow::Result<()> {
    let (_dir, router, id) = fixture().await?;
    let url = format!("/api/v1/providers/{id}/model-rating?model=x");
    for body in [
        json!({}),
        json!({"score":-1}),
        json!({"score":101}),
        json!({"score":1.5}),
        json!({"score":null}),
        json!({"score":"85"}),
        json!({"score":85,"updated_at":"2020-01-01T00:00:00Z"}),
    ] {
        let (status, error) = call(&router, "PUT", &url, Some(body), true).await?;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(error["error"].is_string());
    }
    for suffix in ["", "?model=", "?model=%00"] {
        let bad_url = format!("/api/v1/providers/{id}/model-rating{suffix}");
        assert_eq!(
            call(&router, "GET", &bad_url, None, true).await?.0,
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v1/providers/unknown/model-rating?model=x",
            None,
            true
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, list) = call(&router, "GET", "/api/v1/provider-model-ratings", None, true).await?;
    assert!(list["data"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn rating_profile_http_atomic_input_fallback_and_legacy_common_only() -> anyhow::Result<()> {
    let (_dir, router, id) = fixture().await?;
    let url = format!("/api/v1/providers/{id}/model-rating-profile?model=exact%2FModel%20");
    assert_eq!(
        call(&router, "GET", &url, None, false).await?.0,
        StatusCode::UNAUTHORIZED
    );
    let body =
        json!({"common":80,"overrides":{"low":null,"medium":null,"high":90,"xhigh":null,"max":0}});
    let (status, profile) = call(&router, "PUT", &url, Some(body.clone()), true).await?;
    assert_eq!(status, StatusCode::OK);
    let profile = &profile["data"];
    assert_eq!(profile["upstream_model"], "exact/Model ");
    assert_eq!(profile["display_mode"], "per_effort");
    assert_eq!(profile["effective"]["low"]["score"], 80);
    assert_eq!(profile["effective"]["low"]["source"], "common");
    assert_eq!(profile["effective"]["high"]["score"], 90);
    assert_eq!(profile["effective"]["high"]["source"], "override");
    assert_eq!(profile["effective"]["max"]["score"], 0);
    for invalid in [
        json!({"common":80}),
        json!({"overrides":body["overrides"]}),
        json!({"common":null,"overrides":{"high":90}}),
        json!({"common":null,"overrides":{"low":null,"medium":null,"high":101,"xhigh":null,"max":null}}),
        json!({"common":null,"overrides":{"low":null,"minimal":50,"medium":null,"high":null,"xhigh":null,"max":null}}),
    ] {
        assert_eq!(
            call(&router, "PUT", &url, Some(invalid), true).await?.0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            &call(&router, "GET", &url, None, true).await?.1["data"],
            profile
        );
    }
    let legacy = format!("/api/v1/providers/{id}/model-rating?model=exact%2FModel%20");
    call(&router, "DELETE", &legacy, None, true).await?;
    let state = call(&router, "GET", &url, None, true).await?.1;
    assert!(state["data"]["common"].is_null());
    assert_eq!(state["data"]["effective"]["low"]["status"], "unrated");
    assert_eq!(state["data"]["effective"]["high"]["score"], 90);
    assert_eq!(
        call(&router, "GET", "/api/v1/provider-model-ratings", None, true)
            .await?
            .1["data"],
        json!([])
    );
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v1/provider-model-rating-profiles",
            None,
            true
        )
        .await?
        .1["data"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn rating_performance_http_has_frozen_window_and_explicit_no_samples() -> anyhow::Result<()> {
    let (_dir, router, id) = fixture().await?;
    assert_eq!(
        call(&router, "GET", "/api/v1/model-performance", None, false)
            .await?
            .0,
        StatusCode::UNAUTHORIZED
    );
    let empty = call(&router, "GET", "/api/v1/model-performance", None, true)
        .await?
        .1;
    assert_eq!(empty["data"]["models"], json!([]));
    let profile_url = format!("/api/v1/providers/{id}/model-rating-profile?model=x");
    call(&router, "PUT", &profile_url, Some(json!({"common":null,"overrides":{"low":null,"medium":null,"high":90,"xhigh":null,"max":null}})), true).await?;
    let (status, response) = call(&router, "GET", "/api/v1/model-performance", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    let data = &response["data"];
    assert_eq!(
        data["as_of"].as_i64().unwrap() - data["window_start"].as_i64().unwrap(),
        604800000
    );
    assert_eq!(data["models"].as_array().unwrap().len(), 1);
    let item = &data["models"][0];
    assert_eq!(item["status"], "ready");
    assert_eq!(item["profile"]["display_mode"], "per_effort");
    assert_eq!(item["mixed"]["valid_tps_count"], 0);
    assert!(item["tiers"]["high"]["average_tps"].is_null());
    assert_eq!(item["tiers"]["high"]["selected_request_count"], 0);
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v1/model-performance?provider_id=unknown",
            None,
            true
        )
        .await?
        .0,
        StatusCode::NOT_FOUND
    );
    Ok(())
}

#[test]
fn rating_http_errors_are_typed_not_misreported_as_unrated() {
    assert_eq!(
        rating_error(ProviderModelRatingError::UnsupportedStorage.into()).status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        rating_error(ProviderModelRatingError::ProviderNotFound.into()).status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        rating_error(ProviderModelRatingError::InvalidInput("Invalid score".into()).into())
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        rating_error(anyhow::anyhow!("database unavailable")).status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
