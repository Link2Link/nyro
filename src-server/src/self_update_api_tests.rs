use super::*;
use axum::body::{Body, to_bytes};
use axum::http::Request;
use nyro_core::update::UpdateOptions;
use tower::ServiceExt;

async fn gateway(dir: &tempfile::TempDir) -> anyhow::Result<Gateway> {
    let (gw, _) = Gateway::new(nyro_core::config::GatewayConfig {
        data_dir: dir.path().into(),
        ..Default::default()
    })
    .await?;
    Ok(gw)
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
    authenticated: bool,
) -> anyhow::Result<(StatusCode, serde_json::Value)> {
    let mut request = Request::builder().method(method).uri(uri);
    if authenticated {
        request = request.header("authorization", "Bearer test-admin");
    }
    let request = match body {
        Some(body) => request
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))?,
        None => request.body(Body::empty())?,
    };
    let response = router.clone().oneshot(request).await?;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await?;
    let value = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes)?
    };
    Ok((status, value))
}

/// An updater whose release host refuses connections.
fn offline_updater(gw: &Gateway, self_update: bool) -> anyhow::Result<UpdateService> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let mut options = UpdateOptions::new(gw.config.data_dir.clone());
    options.manifest_url = format!("http://{addr}/nyro-server-update.json");
    options.self_update_enabled = self_update;
    UpdateService::new(gw.storage.clone(), options)
}

#[tokio::test]
async fn update_routes_require_admin_auth() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gw = gateway(&dir).await?;
    let updater = offline_updater(&gw, true)?;
    let router = create_router_with_updater(gw, Some("test-admin".into()), Some(updater));
    for (method, uri) in [
        ("GET", "/api/v1/system/version"),
        ("GET", "/api/v1/system/update/check"),
        ("POST", "/api/v1/system/update/apply"),
        ("GET", "/api/v1/system/update/status"),
    ] {
        let body = (method == "POST").then(|| serde_json::json!({ "version": "9.9.9" }));
        let (status, _) = call(&router, method, uri, body, false).await?;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
    }
    Ok(())
}

#[tokio::test]
async fn version_and_status_report_updater_state() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gw = gateway(&dir).await?;
    let updater = offline_updater(&gw, true)?;
    let router = create_router_with_updater(gw, Some("test-admin".into()), Some(updater));

    let (status, body) = call(&router, "GET", "/api/v1/system/version", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(body["data"]["update_check"], true);
    assert_eq!(body["data"]["self_update"]["supported"], cfg!(unix));

    let (status, body) = call(&router, "GET", "/api/v1/system/update/status", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["phase"], "idle");
    assert!(body["data"].get("error").is_none());

    let (status, body) = call(
        &router,
        "GET",
        "/api/v1/system/update/check?force=true",
        None,
        true,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(body["code"], "UPDATE_FETCH_FAILED");
    Ok(())
}

#[tokio::test]
async fn release_without_manifest_is_not_published_not_a_network_error() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gw = gateway(&dir).await?;
    // A release host that answers 404 for the manifest (release predates it).
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, Router::new()).await;
    });
    let mut options = UpdateOptions::new(gw.config.data_dir.clone());
    options.manifest_url = format!("http://{addr}/nyro-server-update.json");
    let updater = UpdateService::new(gw.storage.clone(), options)?;
    let router = create_router_with_updater(gw, Some("test-admin".into()), Some(updater));

    let (status, body) = call(
        &router,
        "GET",
        "/api/v1/system/update/check?force=true",
        None,
        true,
    )
    .await?;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "UPDATE_NOT_PUBLISHED");
    Ok(())
}

#[tokio::test]
async fn disabled_or_missing_updater_rejects_apply() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let gw = gateway(&dir).await?;
    let updater = offline_updater(&gw, false)?;
    let router = create_router_with_updater(gw.clone(), Some("test-admin".into()), Some(updater));
    let apply = serde_json::json!({ "version": "9.9.9" });

    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/system/update/apply",
        Some(apply.clone()),
        true,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UPDATE_UNSUPPORTED");
    let (_, body) = call(&router, "GET", "/api/v1/system/version", None, true).await?;
    assert_eq!(body["data"]["self_update"]["reason"], "disabled");

    // Hosts without an updater still answer version queries.
    let router = create_router(gw, Some("test-admin".into()));
    let (status, body) = call(&router, "GET", "/api/v1/system/version", None, true).await?;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"]["update_check"], false);
    assert_eq!(body["data"]["self_update"]["reason"], "unavailable");
    let (status, body) = call(
        &router,
        "POST",
        "/api/v1/system/update/apply",
        Some(apply),
        true,
    )
    .await?;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["code"], "UPDATE_UNSUPPORTED");
    Ok(())
}
