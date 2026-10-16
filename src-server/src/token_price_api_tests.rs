use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use nyro_core::config::GatewayConfig;
use nyro_core::storage::{SqliteStorage, Storage};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn fixture() -> anyhow::Result<(tempfile::TempDir, Gateway, SqliteStorage, Router)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    };
    let storage = SqliteStorage::from_config(&config).await?;
    let (gw, _) = Gateway::from_storage(config, Arc::new(storage.clone())).await?;
    let router = create_router(gw.clone(), Some("test-admin".into()));
    Ok((dir, gw, storage, router))
}

async fn call(
    router: &Router,
    method: &str,
    url: &str,
    body: Option<Value>,
    auth: bool,
) -> anyhow::Result<(StatusCode, Value)> {
    let mut request = Request::builder().method(method).uri(url);
    if auth {
        request = request.header("authorization", "Bearer test-admin");
    }
    let body = match body {
        Some(body) => {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        }
        None => Body::empty(),
    };
    let response = router.clone().oneshot(request.body(body)?).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    Ok((status, serde_json::from_slice(&bytes)?))
}

fn price() -> Value {
    json!({"currency":"USD","input_rate":2,"output_rate":8,"cache_read_rate":null})
}

#[tokio::test]
async fn token_price_http_auth_exact_identity_save_and_reset() -> anyhow::Result<()> {
    let (_dir, _gw, _storage, router) = fixture().await?;
    let base = "/api/v1/stats/token-price";
    let url = "/api/v1/stats/token-price?provider_id=P%2F%2B%23%20&model=Model%2F%2B%23%20";
    let input = json!({"provider_id":"P/+# ","model":"Model/+# ","price":price()});
    for (method, path, body) in [("GET", url, None), ("PUT", base, Some(input.clone()))] {
        assert_eq!(
            call(&router, method, path, body, false).await?.0,
            StatusCode::UNAUTHORIZED
        );
    }
    let (status, automatic) = call(&router, "GET", url, None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(automatic["data"]["default_price"].is_null());
    assert!(automatic["data"]["override_price"].is_null());
    let (status, saved) = call(&router, "PUT", base, Some(input), true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(saved["data"]["provider_id"], "P/+# ");
    assert_eq!(saved["data"]["model"], "Model/+# ");
    assert_eq!(saved["data"]["override_price"]["input_rate"], 2.0);
    assert!(saved["data"]["override_price"]["cache_read_rate"].is_null());
    assert_eq!(call(&router, "GET", url, None, true).await?.1, saved);
    for different in [
        "/api/v1/stats/token-price?provider_id=p%2F%2B%23%20&model=Model%2F%2B%23%20",
        "/api/v1/stats/token-price?provider_id=P%2F%2B%23%20&model=model%2F%2B%23%20",
        "/api/v1/stats/token-price?provider_id=P%2F%2B%23%20&model=Model%2F%2B%23",
    ] {
        assert!(
            call(&router, "GET", different, None, true).await?.1["data"]["override_price"]
                .is_null()
        );
    }
    let zero = json!({"provider_id":"P/+# ","model":"Model/+# ","price":{
        "currency":"CNY","input_rate":0,"output_rate":0,"cache_read_rate":0}});
    assert_eq!(
        call(&router, "PUT", base, Some(zero), true).await?.1["data"]["override_price"]["cache_read_rate"],
        0.0
    );
    for _ in 0..2 {
        let (status, reset) = call(
            &router,
            "PUT",
            base,
            Some(json!({
            "provider_id":"P/+# ","model":"Model/+# ","price":null})),
            true,
        )
        .await?;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(reset, automatic);
    }
    Ok(())
}

#[tokio::test]
async fn token_price_http_default_reference_remains_separate_from_override() -> anyhow::Result<()> {
    let (_dir, _gw, storage, router) = fixture().await?;
    let provider = storage
        .providers()
        .create(CreateProvider {
            name: "BigModel".into(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            keys: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "test".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    let url = format!(
        "/api/v1/stats/token-price?provider_id={}&model=glm-5.3",
        provider.id
    );
    let automatic = call(&router, "GET", &url, None, true).await?.1;
    assert_eq!(automatic["data"]["default_price"]["currency"], "CNY");
    assert_eq!(
        automatic["data"]["default_price"]["pricing_provider"],
        "zhipuai"
    );
    assert!(
        automatic["data"]["default_price"]["input_rate"]
            .as_f64()
            .unwrap()
            > 0.0
    );
    let saved = call(
        &router,
        "PUT",
        "/api/v1/stats/token-price",
        Some(json!({
        "provider_id":provider.id, "model":"glm-5.3", "price":price()})),
        true,
    )
    .await?
    .1;
    assert_eq!(
        saved["data"]["default_price"],
        automatic["data"]["default_price"]
    );
    assert_eq!(saved["data"]["override_price"]["currency"], "USD");
    assert_eq!(call(&router, "GET", &url, None, true).await?.1, saved);
    Ok(())
}

#[tokio::test]
async fn token_price_http_inherited_reference_save_reset_and_provider_isolation()
-> anyhow::Result<()> {
    let (_dir, _gw, _storage, router) = fixture().await?;
    let base = "/api/v1/stats/token-price";
    // Retained/deleted provider IDs use exactly the same inheritance contract.
    let url = format!("{base}?provider_id=deleted&model=claude-haiku-5-5-medium");
    let (status, empty) = call(&router, "GET", &url, None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert!(empty["data"].get("inherited_price").unwrap().is_null());
    for model in ["claude-haiku", "claude-haiku-5-5"] {
        assert_eq!(
            call(
                &router,
                "PUT",
                base,
                Some(json!({
                    "provider_id":"deleted", "model":model, "price":price()
                })),
                true
            )
            .await?
            .0,
            StatusCode::OK
        );
    }
    let inherited = call(&router, "GET", &url, None, true).await?.1;
    assert!(inherited["data"]["default_price"].is_null());
    assert!(inherited["data"]["override_price"].is_null());
    assert_eq!(
        inherited["data"]["inherited_price"],
        json!({
            "currency":"USD", "input_rate":2.0, "output_rate":8.0, "cache_read_rate":null,
            "pricing_provider":"manual", "pricing_model":"claude-haiku-5-5"
        })
    );
    let saved = call(
        &router,
        "PUT",
        base,
        Some(json!({
            "provider_id":"deleted", "model":"claude-haiku-5-5-medium",
            "price":{"currency":"CNY","input_rate":0,"output_rate":0,"cache_read_rate":0}
        })),
        true,
    )
    .await?
    .1;
    assert_eq!(saved["data"]["override_price"]["currency"], "CNY");
    assert_eq!(
        saved["data"]["inherited_price"],
        inherited["data"]["inherited_price"]
    );
    assert_eq!(call(&router, "GET", &url, None, true).await?.1, saved);
    let reset = call(
        &router,
        "PUT",
        base,
        Some(json!({
            "provider_id":"deleted", "model":"claude-haiku-5-5-medium", "price":null
        })),
        true,
    )
    .await?
    .1;
    assert_eq!(reset, inherited);
    assert_eq!(call(&router, "GET", &url, None, true).await?.1, inherited);
    for query in [
        "provider_id=Deleted&model=claude-haiku-5-5-medium",
        "provider_id=deleted&model=CLAUDE-HAIKU-5-5-medium",
        "provider_id=other&model=claude-haiku-5-5-medium",
        "provider_id=deleted&model=claude-haikux",
    ] {
        let data = call(&router, "GET", &format!("{base}?{query}"), None, true)
            .await?
            .1;
        assert!(data["data"]["inherited_price"].is_null(), "{query}");
        assert!(data["data"]["override_price"].is_null());
    }
    Ok(())
}

#[tokio::test]
async fn token_price_http_validation_is_400_without_coercion() -> anyhow::Result<()> {
    let (_dir, _gw, _storage, router) = fixture().await?;
    let base = "/api/v1/stats/token-price";
    for body in [
        json!({}),
        json!({"provider_id":"p","model":"m"}),
        json!({"provider_id":" ","model":"m","price":null}),
        json!({"provider_id":"p","model":"\t","price":null}),
        json!({"provider_id":42,"model":"m","price":null}),
        json!({"provider_id":"p","model":"m","price":0}),
        json!({"provider_id":"p","model":"m","price":price(),"extra":true}),
    ] {
        assert_eq!(
            call(&router, "PUT", base, Some(body), true).await?.0,
            StatusCode::BAD_REQUEST
        );
    }
    for (field, value) in [
        ("currency", json!("usd")),
        ("currency", json!("EUR")),
        ("currency", json!(null)),
        ("input_rate", json!(-1)),
        ("output_rate", json!(-1)),
        ("cache_read_rate", json!(-1)),
        ("input_rate", json!(1_000_000_001_f64)),
        ("output_rate", json!(1_000_000_001_f64)),
        ("cache_read_rate", json!(1_000_000_001_f64)),
        ("input_rate", json!("2")),
        ("output_rate", json!(null)),
        ("cache_read_rate", json!(true)),
    ] {
        let mut rates = price();
        rates[field] = value;
        let (status, error) = call(
            &router,
            "PUT",
            base,
            Some(json!({"provider_id":"p","model":"m","price":rates})),
            true,
        )
        .await?;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{field}: {error}");
    }
    for suffix in [
        "",
        "?provider_id=p",
        "?model=m",
        "?provider_id=&model=m",
        "?provider_id=p&model=%20",
    ] {
        assert_eq!(
            call(&router, "GET", &format!("{base}{suffix}"), None, true)
                .await?
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    for raw in [
        "{",
        r#"{"provider_id":"p","model":"m","price":{"currency":"USD","input_rate":1e999,"output_rate":0}}"#,
    ] {
        let request = Request::builder()
            .method("PUT")
            .uri(base)
            .header("authorization", "Bearer test-admin")
            .header("content-type", "application/json")
            .body(Body::from(raw))?;
        assert_eq!(
            router.clone().oneshot(request).await?.status(),
            StatusCode::BAD_REQUEST
        );
    }
    Ok(())
}

#[tokio::test]
async fn token_price_http_corruption_generic_bypass_and_storage_errors() -> anyhow::Result<()> {
    let (_dir, gw, storage, router) = fixture().await?;
    let base = "/api/v1/stats/token-price";
    call(
        &router,
        "PUT",
        base,
        Some(json!({"provider_id":"p","model":"m","price":price()})),
        true,
    )
    .await?;
    let (key, _) = storage
        .settings()
        .list_all()
        .await?
        .into_iter()
        .find(|(key, _)| key.starts_with("token_price.v1."))
        .unwrap();
    assert_eq!(
        call(
            &router,
            "PUT",
            &format!("/api/v1/settings/{key}"),
            Some(json!({"value":"bad json"})),
            true
        )
        .await?
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut export = gw.admin().export_config().await?;
    export.settings = vec![(key.clone(), "bad json".into())];
    assert_eq!(
        call(
            &router,
            "POST",
            "/api/v1/config/import",
            Some(serde_json::to_value(export)?),
            true
        )
        .await?
        .0,
        StatusCode::BAD_REQUEST
    );
    storage.settings().set(&key, "bad json").await?;
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{base}?provider_id=p&model=m"),
            None,
            true
        )
        .await?
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        call(
            &router,
            "GET",
            "/api/v1/stats/token-usage?year=2024",
            None,
            true
        )
        .await?
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        call(
            &router,
            "PUT",
            base,
            Some(json!({"provider_id":"p","model":"m","price":null})),
            true
        )
        .await?
        .0,
        StatusCode::OK
    );
    storage.pool().close().await;
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("{base}?provider_id=p&model=m"),
            None,
            true
        )
        .await?
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(
        call(
            &router,
            "PUT",
            base,
            Some(json!({"provider_id":"p","model":"m","price":null})),
            true
        )
        .await?
        .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    Ok(())
}
