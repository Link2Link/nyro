use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use tower::ServiceExt;

#[tokio::test]
async fn token_usage_http_contract_auth_and_year_validation() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let (gw, _) = Gateway::new(nyro_core::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .await?;
    let router = create_router(gw, Some("test-admin".into()));
    for (query, authenticated, expected) in [
        ("?year=2024", false, StatusCode::UNAUTHORIZED),
        ("?year=1969", true, StatusCode::BAD_REQUEST),
        ("?year=9999", true, StatusCode::BAD_REQUEST),
        ("?year=2147483648", true, StatusCode::BAD_REQUEST),
        ("?year=no", true, StatusCode::BAD_REQUEST),
        ("", true, StatusCode::BAD_REQUEST),
        ("?year=2024", true, StatusCode::OK),
    ] {
        let mut request = Request::builder().uri(format!("/api/v1/stats/token-usage{query}"));
        if authenticated {
            request = request.header("authorization", "Bearer test-admin");
        }
        let response = router.clone().oneshot(request.body(Body::empty())?).await?;
        assert_eq!(response.status(), expected, "query {query}");
        if expected == StatusCode::OK {
            let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
            let value: serde_json::Value = serde_json::from_slice(&bytes)?;
            assert_eq!(value["data"]["year"], 2024);
            assert_eq!(value["data"]["time_zone"], "UTC");
            assert!(value["data"]["generated_at"].as_i64().unwrap() > 0);
            assert_eq!(value["data"]["rows"], serde_json::json!([]));
        }
    }
    Ok(())
}
