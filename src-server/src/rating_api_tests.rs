use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn fixture() -> anyhow::Result<(tempfile::TempDir, Router)> {
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::new(nyro_core::config::GatewayConfig {
        data_dir: dir.path().to_path_buf(),
        ..Default::default()
    })
    .await?;
    let _ = gw
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
    Ok((dir, create_router(gw, Some("test-admin".to_string()))))
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
async fn rating_http_auth_encoded_identity_and_zero_contract() -> anyhow::Result<()> {
    let (_dir, router) = fixture().await?;
    let url = "/api/v1/model-ratings?prefix=vendor%2FModel%2Bx%23%20";
    assert_eq!(
        call(&router, "GET", "/api/v1/model-ratings", None, false)
            .await?
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, saved) = call(&router, "PUT", &url, Some(json!({"score":0})), true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["data"]["score"], 0);
    // Prefixes are stored lowercase; matching and identity ignore case.
    assert_eq!(saved["data"]["model_prefix"], "vendor/model+x# ");
    let (_, list) = call(&router, "GET", "/api/v1/model-ratings", None, true).await?;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    // Identity is exact: the trimmed prefix is a different entry.
    let trimmed = "/api/v1/model-ratings?prefix=vendor%2FModel%2Bx%23";
    call(&router, "PUT", trimmed, Some(json!({"score":100})), true).await?;
    let (_, list) = call(&router, "GET", "/api/v1/model-ratings", None, true).await?;
    assert_eq!(list["data"].as_array().unwrap().len(), 2);
    for _ in 0..2 {
        let (status, result) = call(&router, "DELETE", &url, None, true).await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(result, json!({"ok":true}));
    }
    let (_, list) = call(&router, "GET", "/api/v1/model-ratings", None, true).await?;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);
    Ok(())
}

#[tokio::test]
async fn rating_http_rejects_bad_input_without_coercion() -> anyhow::Result<()> {
    let (_dir, router) = fixture().await?;
    let url = "/api/v1/model-ratings?prefix=x";
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
    for suffix in ["", "?prefix=", "?prefix=%00"] {
        let bad_url = format!("/api/v1/model-ratings{suffix}");
        assert_eq!(
            call(&router, "PUT", &bad_url, Some(json!({"score":80})), true)
                .await?
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let (_, list) = call(&router, "GET", "/api/v1/model-ratings", None, true).await?;
    assert!(list["data"].as_array().unwrap().is_empty());
    Ok(())
}

#[tokio::test]
async fn rating_performance_http_empty_without_logged_matches() -> anyhow::Result<()> {
    let (_dir, router) = fixture().await?;
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
    // A rating alone is not enough: groups need logged pairs that match it.
    let rating_url = "/api/v1/model-ratings?prefix=x";
    call(&router, "PUT", &rating_url, Some(json!({"score":90})), true).await?;
    let (status, response) = call(&router, "GET", "/api/v1/model-performance", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    let data = &response["data"];
    assert!(data["as_of"].as_i64().unwrap() > 0);
    assert!(
        data["window_start"].is_null(),
        "latest retained calls have no extra time-window filter"
    );
    assert_eq!(data["models"].as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn rating_http_errors_are_typed_not_misreported() {
    assert_eq!(
        rating_error(ModelRatingError::UnsupportedStorage.into()).status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        rating_error(ModelRatingError::InvalidInput("Invalid score".into()).into()).status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        rating_error(anyhow::anyhow!("database unavailable")).status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}
