//! End-to-end coverage for the provider key pool (multi-key relay vendors):
//! per-key credential selection, failover on key-level errors, log
//! attribution, and snapshot persistence across provider updates.
use axum::{
    Json, Router,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{
        CreateModel, CreateModelBackend, CreateProvider, CreateProviderProtocolEndpoint,
        ProviderKeyProbeResult, UpsertProviderKey,
    },
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::json;
use std::{collections::HashMap, sync::Arc, time::Duration};

/// Upstream that answers 401 for `Bearer key-a` and a normal completion for
/// anything else. Records which credentials were seen.
async fn auth_aware_upstream(
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let credential = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .or_else(|| {
            headers
                .get("x-api-key")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
        .unwrap_or_default();
    let _ = body;
    if credential.contains("key-a") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":{"message":"invalid api key"}})),
        )
            .into_response();
    }
    let is_anthropic = uri.path().ends_with("/messages");
    if is_anthropic {
        return (
            StatusCode::OK,
            Json(json!({"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})),
        )
            .into_response();
    }
    (
        StatusCode::OK,
        Json(json!({"id":"1","choices":[{"index":0,"message":{"role":"assistant","content":"hi"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})),
    )
        .into_response()
}

async fn setup() -> anyhow::Result<(
    tempfile::TempDir,
    Gateway,
    tokio::sync::mpsc::Receiver<nyro_core::logging::LogEntry>,
    String,
    tokio::task::JoinHandle<()>,
)> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, logs) = Gateway::from_storage(config, storage).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let job = tokio::spawn(async move {
        let app = Router::new()
            .route("/v1/chat/completions", post(auth_aware_upstream))
            .route("/v1/messages", post(auth_aware_upstream));
        let _ = axum::serve(listener, app).await;
    });
    Ok((dir, gw, logs, url, job))
}

async fn pool_provider(
    gw: &Gateway,
    url: &str,
    keys: Vec<UpsertProviderKey>,
) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
            keys,
            name: uuid::Uuid::new_v4().to_string(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: url.into(),
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "ignored-provider-level-key".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}

fn key(name: &str, secret: &str, priority: i32, models: &[&str]) -> UpsertProviderKey {
    UpsertProviderKey {
        id: None,
        name: name.into(),
        api_key: secret.into(),
        protocol: None,
        base_url: None,
        is_enabled: true,
        priority,
        manual_models: Some(serde_json::to_string(&models).unwrap()),
    }
}

fn key_pinned(
    name: &str,
    secret: &str,
    priority: i32,
    models: &[&str],
    protocol: Option<&str>,
    base_url: Option<&str>,
) -> UpsertProviderKey {
    UpsertProviderKey {
        id: None,
        name: name.into(),
        api_key: secret.into(),
        protocol: protocol.map(str::to_string),
        base_url: base_url.map(str::to_string),
        is_enabled: true,
        priority,
        manual_models: Some(serde_json::to_string(&models).unwrap()),
    }
}

async fn route(gw: &Gateway, name: &str, provider: &str, target: &str) -> anyhow::Result<()> {
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: provider.to_string(),
            target_model: target.into(),
            targets: vec![CreateModelBackend {
                provider_id: provider.to_string(),
                model: target.into(),
                weight: Some(100),
                priority: Some(1),
                is_fallback: None,
            }],
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}

async fn dispatch(gw: &Gateway, name: &str, ingress: ProtocolId) -> (String, Response) {
    let body = json!({"model":name,"messages":[{"role":"user","content":"hi"}],"max_tokens":20});
    let request = ingress
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())
        .unwrap();
    let ctx = RequestContext::new(ingress, Duration::from_secs(5));
    let id = ctx.request_id.clone();
    let response = dispatch_pipeline(
        gw.clone(),
        HeaderMap::new(),
        RawEnvelope::new(
            Some(body),
            HashMap::new(),
            "POST",
            if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
                "/v1/messages"
            } else {
                "/v1/chat/completions"
            },
        ),
        request,
        ingress,
        ctx,
    )
    .await;
    (id, response)
}

#[tokio::test]
async fn key_pool_fails_over_on_401_and_attributes_log() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, _job) = setup().await?;
    let provider = pool_provider(
        &gw,
        &url,
        vec![
            key("key-a", "key-a", 0, &["pool-model"]),
            key("key-b", "key-b", 1, &["pool-model"]),
        ],
    )
    .await?;
    route(&gw, "km", &provider, "pool-model").await?;

    let (_id, response) = dispatch(&gw, "km", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    assert!(String::from_utf8_lossy(&body).contains("hi"));

    // Both attempts logged; the successful one must carry the fallback key.
    let mut saw_key_a = false;
    let mut saw_key_b = false;
    for _ in 0..2 {
        let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
            .await
            .expect("log entry")
            .unwrap();
        match row.provider_key_name.as_deref() {
            Some("key-a") => saw_key_a = true,
            Some("key-b") => saw_key_b = true,
            other => panic!("unexpected key attribution: {other:?}"),
        }
    }
    assert!(saw_key_a && saw_key_b, "expected both keys in logs");
    Ok(())
}

#[tokio::test]
async fn key_pool_without_eligible_key_fails_with_clear_error() -> anyhow::Result<()> {
    let (_dir, gw, _logs, url, _job) = setup().await?;
    let provider =
        pool_provider(&gw, &url, vec![key("only", "key-a", 0, &["other-model"])]).await?;
    route(&gw, "km2", &provider, "pool-model").await?;

    let (_id, response) = dispatch(&gw, "km2", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).await;
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    assert!(
        String::from_utf8_lossy(&body).contains("no provider key in pool serves model"),
        "body: {body:?}"
    );
    Ok(())
}

#[tokio::test]
async fn unprobed_key_is_eligible_and_pool_overrides_provider_key() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, _job) = setup().await?;
    // No manual models and no snapshot: the key must still serve traffic.
    let provider = pool_provider(
        &gw,
        &url,
        vec![UpsertProviderKey {
            id: None,
            name: "unprobed".into(),
            api_key: "key-b".into(),
            protocol: None,
            base_url: None,
            is_enabled: true,
            priority: 0,
            manual_models: None,
        }],
    )
    .await?;
    route(&gw, "km3", &provider, "anything").await?;

    let (_id, response) = dispatch(&gw, "km3", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await
        .expect("log entry")
        .unwrap();
    assert_eq!(row.provider_key_name.as_deref(), Some("unprobed"));
    Ok(())
}

#[tokio::test]
async fn adaptive_pool_key_covers_anthropic_egress() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, _job) = setup().await?;
    let base = url.trim_end_matches("/v1").to_string();
    let provider = gw
        .admin()
        .create_provider(CreateProvider {
            keys: vec![key("shared", "key-b", 0, &["pool-model"])],
            name: uuid::Uuid::new_v4().to_string(),
            vendor: None,
            protocol: "openai-compatible".into(),
            base_url: base.clone(),
            protocol_mode: "adaptive".into(),
            protocol_endpoints: vec![
                CreateProviderProtocolEndpoint {
                    protocol: "openai-compatible/chat-completions/v1".into(),
                    base_url: format!("{base}/v1"),
                    api_key: "ignored".into(),
                    auth_scheme: "auto".into(),
                    is_enabled: true,
                    priority: 0,
                },
                CreateProviderProtocolEndpoint {
                    protocol: "anthropic/messages/2023-06-01".into(),
                    base_url: base.clone(),
                    api_key: "ignored".into(),
                    auth_scheme: "auto".into(),
                    is_enabled: true,
                    priority: 1,
                },
            ],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "ignored".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id;
    route(&gw, "km4", &provider, "pool-model").await?;

    // Anthropic ingress: egress stays Anthropic (native) and must carry the
    // pool key via x-api-key.
    let (_id, response) = dispatch(&gw, "km4", ANTHROPIC_MESSAGES_2023_06_01).await;
    assert_eq!(response.status(), StatusCode::OK);
    let _ = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await
        .expect("log entry")
        .unwrap();
    assert_eq!(row.provider_key_name.as_deref(), Some("shared"));
    Ok(())
}

#[tokio::test]
async fn probe_snapshots_survive_provider_update() -> anyhow::Result<()> {
    let (_dir, gw, _logs, url, _job) = setup().await?;
    let provider_id = pool_provider(&gw, &url, vec![key("key-a", "key-a", 0, &["m1"])]).await?;

    // Record a probe result (auto snapshot path).
    let provider = gw.admin().get_provider(&provider_id).await?;
    let key_id = provider.keys[0].id.clone();
    gw.storage
        .providers()
        .record_key_probe_result(
            &key_id,
            ProviderKeyProbeResult {
                success: true,
                error: None,
                tested_at: "2026-01-01 00:00:00".into(),
                models: Some(vec!["m1".into(), "m2".into()]),
            },
        )
        .await?;

    // Update the provider: same key id (renamed) + one brand-new key.
    let stored = gw.admin().get_provider(&provider_id).await?;
    let stored_key = &stored.keys[0];
    assert_eq!(
        stored_key.models_snapshot.as_deref(),
        Some(r#"["m1","m2"]"#)
    );
    gw.admin()
        .update_provider(
            &provider_id,
            nyro_core::db::models::UpdateProvider {
                keys: Some(vec![
                    UpsertProviderKey {
                        id: Some(stored_key.id.clone()),
                        name: "renamed".into(),
                        api_key: "key-a".into(),
                        protocol: None,
                        base_url: None,
                        is_enabled: true,
                        priority: 5,
                        manual_models: None,
                    },
                    UpsertProviderKey {
                        id: None,
                        name: "fresh".into(),
                        api_key: "key-c".into(),
                        protocol: None,
                        base_url: None,
                        is_enabled: true,
                        priority: 6,
                        manual_models: None,
                    },
                ]),
                ..Default::default()
            },
        )
        .await?;

    let updated = gw.admin().get_provider(&provider_id).await?;
    assert_eq!(updated.keys.len(), 2);
    let renamed = updated
        .keys
        .iter()
        .find(|k| k.name == "renamed")
        .expect("renamed key survives");
    assert_eq!(
        renamed.models_snapshot.as_deref(),
        Some(r#"["m1","m2"]"#),
        "snapshot must survive a same-id update"
    );
    assert_eq!(renamed.priority, 5);
    let fresh = updated
        .keys
        .iter()
        .find(|k| k.name == "fresh")
        .expect("new key added");
    assert!(fresh.models_snapshot.is_none());
    Ok(())
}

/// A candidate pinned to Anthropic Messages on an OpenAI-compatible provider
/// must egress via /v1/messages with the x-api-key scheme and its own
/// credential, converting the OpenAI-ingress request and response.
#[tokio::test]
async fn pinned_candidate_routes_anthropic_egress_with_own_credential() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, mut logs) = Gateway::from_storage(config, storage).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base = format!("http://{}", listener.local_addr()?);
    let seen_paths = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let seen_creds = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    {
        let seen_paths = seen_paths.clone();
        let seen_creds = seen_creds.clone();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/messages",
                post(move |uri: axum::http::Uri, headers: HeaderMap| {
                    let seen_paths = seen_paths.clone();
                    let seen_creds = seen_creds.clone();
                    async move {
                        let credential = headers
                            .get("x-api-key")
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string)
                            .unwrap_or_default();
                        seen_paths.lock().unwrap().push(uri.path().to_string());
                        seen_creds.lock().unwrap().push(credential);
                        (
                            StatusCode::OK,
                            Json(json!({"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}],"stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}})),
                        )
                    }
                }),
            );
            let _ = axum::serve(listener, app).await;
        });
    }

    let provider = pool_provider(
        &gw,
        &base,
        vec![key_pinned(
            "pinned",
            "anthro-key",
            0,
            &["pool-model"],
            Some("anthropic-messages"),
            None,
        )],
    )
    .await?;
    route(&gw, "km5", &provider, "pool-model").await?;

    let (_id, response) = dispatch(&gw, "km5", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    assert!(
        String::from_utf8_lossy(&body).contains("hi"),
        "body: {body:?}"
    );
    assert_eq!(
        seen_paths.lock().unwrap().as_slice(),
        ["/v1/messages"],
        "pinned candidate must egress through the Anthropic endpoint"
    );
    assert_eq!(
        seen_creds.lock().unwrap().as_slice(),
        ["anthro-key"],
        "pinned candidate must authenticate with its own secret via x-api-key"
    );
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await
        .expect("log entry")
        .unwrap();
    assert_eq!(row.provider_key_name.as_deref(), Some("pinned"));
    Ok(())
}

/// A candidate carrying its own API address retargets the upstream call,
/// even while inheriting the provider's protocol.
#[tokio::test]
async fn candidate_base_url_override_retargets_upstream() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, _logs) = Gateway::from_storage(config, storage).await?;

    async fn chat_from(marker: &'static str) -> Response {
        (
            StatusCode::OK,
            Json(json!({"id":"1","choices":[{"index":0,"message":{"role":"assistant","content":marker},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})),
        )
            .into_response()
    }
    let listener_a = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_a = format!("http://{}/v1", listener_a.local_addr()?);
    let listener_b = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let base_b = format!("http://{}/v1", listener_b.local_addr()?);
    let app_a = Router::new().route("/v1/chat/completions", post(|| chat_from("from-a")));
    let app_b = Router::new().route("/v1/chat/completions", post(|| chat_from("from-b")));
    tokio::spawn(async move {
        let _ = axum::serve(listener_a, app_a).await;
    });
    tokio::spawn(async move {
        let _ = axum::serve(listener_b, app_b).await;
    });

    let provider = pool_provider(
        &gw,
        &base_a,
        vec![key_pinned(
            "relocated",
            "key-b",
            0,
            &["pool-model"],
            None,
            Some(&base_b),
        )],
    )
    .await?;
    route(&gw, "km6", &provider, "pool-model").await?;

    let (_id, response) = dispatch(&gw, "km6", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    assert!(
        String::from_utf8_lossy(&body).contains("from-b"),
        "candidate base URL must win over the provider base: {body:?}"
    );
    Ok(())
}

/// Changing a candidate's protocol or API address invalidates its probe
/// snapshot; a rename-only update keeps it.
#[tokio::test]
async fn snapshot_resets_when_candidate_endpoint_changes() -> anyhow::Result<()> {
    let (_dir, gw, _logs, url, _job) = setup().await?;
    let provider_id = pool_provider(&gw, &url, vec![key("key-a", "key-a", 0, &["m1"])]).await?;

    let record_probe = |gw: &Gateway, key_id: String| {
        let gw = gw.clone();
        async move {
            gw.storage
                .providers()
                .record_key_probe_result(
                    &key_id,
                    ProviderKeyProbeResult {
                        success: true,
                        error: None,
                        tested_at: "2026-01-01 00:00:00".into(),
                        models: Some(vec!["m1".into(), "m2".into()]),
                    },
                )
                .await
        }
    };

    let stored = gw.admin().get_provider(&provider_id).await?;
    let key_id = stored.keys[0].id.clone();
    record_probe(&gw, key_id.clone()).await?;

    // Protocol change on the same row id → probe fields reset.
    gw.admin()
        .update_provider(
            &provider_id,
            nyro_core::db::models::UpdateProvider {
                keys: Some(vec![key_pinned(
                    "key-a",
                    "key-a",
                    0,
                    &["m1"],
                    Some("anthropic-messages"),
                    None,
                )]),
                ..Default::default()
            },
        )
        .await?;
    let updated = gw.admin().get_provider(&provider_id).await?;
    assert!(updated.keys[0].models_snapshot.is_none());
    assert!(updated.keys[0].last_probe_at.is_none());
    assert!(updated.keys[0].probe_error.is_none());
    assert_eq!(
        updated.keys[0].protocol.as_deref(),
        Some("anthropic-messages/messages/2023-06-01"),
        "protocol must be stored in canonical endpoint form"
    );

    // Re-probe, then change only the base URL → reset again.
    record_probe(&gw, updated.keys[0].id.clone()).await?;
    gw.admin()
        .update_provider(
            &provider_id,
            nyro_core::db::models::UpdateProvider {
                keys: Some(vec![key_pinned(
                    "key-a",
                    "key-a",
                    0,
                    &["m1"],
                    Some("anthropic-messages"),
                    Some("https://other-relay.test"),
                )]),
                ..Default::default()
            },
        )
        .await?;
    let updated = gw.admin().get_provider(&provider_id).await?;
    assert!(updated.keys[0].models_snapshot.is_none());
    assert_eq!(
        updated.keys[0].base_url.as_deref(),
        Some("https://other-relay.test")
    );
    Ok(())
}
