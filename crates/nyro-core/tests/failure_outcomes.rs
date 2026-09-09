//! Authoritative diagnostics exercised through real upstream sockets and dispatcher paths.
use axum::{
    Json, Router,
    body::Body,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateModelBackend, CreateProvider},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

async fn upstream(Json(body): Json<Value>) -> Response {
    let model = body["model"].as_str().unwrap_or_default();
    if model == "http502" {
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"error":{"message":"retry me"}})),
        )
            .into_response();
    }
    let wire = match model {
        "raw-error" => "data: {\"error\":{\"message\":\"provider failed\"}}\n\n".to_owned(),
        "unknown" => "data: {\"vendor_event\":\"opaque\"}\n\n".to_owned(),
        "overflow" => format!("data: {{\"vendor_event\":\"{}\"}}\n\n", "x".repeat(1024 * 1024 + 32)),
        "limited" => "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"length\"}]}\n\ndata: [DONE]\n\n".to_owned(),
        _ => "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n".to_owned(),
    };
    if model == "drop" {
        let stream = futures::stream::once(async {
            Ok::<_, std::io::Error>(bytes::Bytes::from_static(
                b"data: {\"choices\":[{\"delta\":{\"content\":\"hello\"}}]}\n\n",
            ))
        })
        .chain(futures::stream::pending());
        return Response::builder()
            .header("content-type", "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap();
    }
    if body["stream"] == true {
        return ([("content-type", "text/event-stream")], wire).into_response();
    }
    Json(json!({"id":"r","model":"ok","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":"stop"}]})).into_response()
}
use futures::StreamExt;

#[tokio::test]
async fn http200_partial_read_is_failed_with_safe_ingress_error() -> anyhow::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_dir, gw, mut logs, _, job) = setup().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let source = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut input = vec![0u8; 8192];
        let _ = socket.read(&mut input).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: 9999\r\n\r\ndata: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"partial\"}}]}\n\n").await.unwrap();
        socket.shutdown().await.unwrap();
    });
    let p = provider(&gw, &url).await?;
    route(&gw, "read-failure", p, "ok", vec![]).await?;
    let (id, response) = dispatch(
        &gw,
        "read-failure",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let wire = String::from_utf8_lossy(&bytes);
    assert!(wire.contains("upstream_read_error"), "{wire}");
    assert!(wire.contains(&id));
    assert!(
        !wire.contains("event: error"),
        "chat protocol must not emit Anthropic envelope"
    );
    assert!(!wire.contains(&url));
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await?
        .unwrap();
    assert_eq!(row.client_status_code, 200);
    assert_eq!(row.upstream_status_code, Some(200));
    assert_eq!(row.diagnostic.attempt_outcome, "failed");
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("upstream_read_error")
    );
    assert_eq!(
        row.diagnostic.failure_stage.as_deref(),
        Some("upstream_read")
    );
    assert!(row.diagnostic.error_message.is_some());
    assert_eq!(
        row.diagnostic.payload_metadata["upstream_response_body"]["complete"],
        false
    );
    assert_eq!(row.diagnostic.final_result.unwrap().final_outcome, "failed");
    source.await?;
    job.abort();
    Ok(())
}

#[tokio::test]
async fn compat_stream_limit_remains_output_limited() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, job) = setup().await?;
    let p = provider(&gw, &url).await?;
    route(&gw, "compat-limit", p, "limited", vec![]).await?;
    let (id, response) = dispatch(&gw, "compat-limit", ANTHROPIC_MESSAGES_2023_06_01, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await?
        .unwrap();
    assert_eq!(row.diagnostic.attempt_outcome, "output_limited");
    assert_eq!(
        row.diagnostic.client_request_id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(
        row.diagnostic.final_result.unwrap().final_outcome,
        "output_limited"
    );
    job.abort();
    Ok(())
}

#[tokio::test]
async fn codex_style_close_after_terminal_is_completed_not_cancelled() -> anyhow::Result<()> {
    // codex CLI reads the SSE stream until the protocol terminal event and
    // immediately closes the connection; the gateway then loses two harmless
    // races (response-body EOS poll and upstream EOF poll). The attempt must
    // still be classified completed — a drop without EOS after every produced
    // frame was consumed is full delivery, not a cancellation.
    let (_dir, gw, mut logs, url, job) = setup().await?;
    let p = provider(&gw, &url).await?;
    route(&gw, "codex-close", p, "ok", vec![]).await?;
    let (id, response) = dispatch(
        &gw,
        "codex-close",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        true,
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let mut saw_terminal = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let text = String::from_utf8_lossy(&chunk);
        if text.contains("[DONE]") {
            saw_terminal = true;
            break;
        }
    }
    assert!(saw_terminal, "client must observe the protocol terminal");
    drop(stream); // codex closes here: no terminal EOS poll happens
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await?
        .unwrap();
    assert_eq!(row.client_status_code, 200);
    assert_eq!(row.upstream_status_code, Some(200));
    assert_eq!(row.diagnostic.outcome_version, 1);
    assert_eq!(row.diagnostic.attempt_outcome, "completed");
    assert!(row.diagnostic.failure_kind.is_none());
    assert!(row.diagnostic.error_causes.is_empty());
    assert_eq!(
        row.diagnostic.client_request_id.as_deref(),
        Some(id.as_str())
    );
    let final_result = row.diagnostic.final_result.unwrap();
    assert_eq!(final_result.final_outcome, "completed");
    assert_eq!(final_result.attempt_count, 1);
    assert_eq!(row.performance.completion, "completed");
    assert!(row.performance.completed_at.is_some());
    assert!(logs.try_recv().is_err());
    job.abort();
    Ok(())
}

#[tokio::test]
async fn mid_stream_disconnect_without_terminal_stays_cancelled() -> anyhow::Result<()> {
    // A client that vanishes before the upstream produced any terminal must
    // not be promoted to completed by the drain reconciliation.
    let (_dir, gw, mut logs, _, job) = setup().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let source = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/v1/chat/completions",
                post(|| async {
                    let stream = futures::stream::once(async {
                        Ok::<_, std::convert::Infallible>(bytes::Bytes::from_static(
                    b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"par\"}}]}\n\n",
                ))
                    })
                    .chain(futures::stream::pending());
                    (
                        [("content-type", "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }),
            ),
        )
        .await
        .unwrap();
    });
    let p = provider(&gw, &url).await?;
    route(&gw, "mid-drop", p, "ok", vec![]).await?;
    let (_id, response) =
        dispatch(&gw, "mid-drop", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, true).await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut stream = response.into_body().into_data_stream();
    let first = stream.next().await.transpose()?;
    assert!(first.is_some(), "at least one frame must arrive");
    drop(stream); // client disconnects mid-stream, no terminal was ever sent
    let row = tokio::time::timeout(Duration::from_secs(5), logs.recv())
        .await?
        .unwrap();
    assert_eq!(row.diagnostic.attempt_outcome, "cancelled");
    assert_eq!(
        row.diagnostic.failure_kind.as_deref(),
        Some("client_cancelled")
    );
    assert!(row.performance.completed_at.is_none());
    source.abort();
    job.abort();
    Ok(())
}

#[tokio::test]
async fn forced_stream_incomplete_reason_is_not_automatically_token_limited() -> anyhow::Result<()>
{
    let (_dir, gw, mut logs, _, job) = setup().await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1", listener.local_addr()?);
    let source = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/v1/responses", post(|| async {
            ([("content-type", "text/event-stream")], "event: response.incomplete\ndata: {\"type\":\"response.incomplete\",\"response\":{\"id\":\"r\",\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"content_filter\"},\"output\":[]}}\n\n")
        }))).await.unwrap();
    });
    let p = gw
        .admin()
        .create_provider(CreateProvider {
            name: "forced".into(),
            vendor: None,
            protocol: "openai-responses".into(),
            base_url: url,
            protocol_mode: "fixed".into(),
            protocol_endpoints: vec![],
            preset_key: None,
            channel: None,
            models_source: None,
            static_models: None,
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?;
    route(&gw, "forced", p.id, "ok", vec![]).await?;
    let body = json!({"model":"forced","input":"hi","stream":false});
    let request = OPENAI_RESPONSES_V1
        .handler()
        .make_request_decoder()
        .decode_request(body.clone())?;
    let response = dispatch_pipeline(
        gw.clone(),
        HeaderMap::new(),
        RawEnvelope::new(Some(body), HashMap::new(), "POST", "/v1/responses"),
        request,
        OPENAI_RESPONSES_V1,
        RequestContext::new(OPENAI_RESPONSES_V1, Duration::from_secs(5)),
    )
    .await;
    axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await?
        .unwrap();
    assert_ne!(row.diagnostic.attempt_outcome, "completed");
    assert_ne!(row.diagnostic.attempt_outcome, "output_limited");
    assert_eq!(row.performance.response_mode, "stream");
    source.abort();
    job.abort();
    Ok(())
}

#[tokio::test]
async fn preflight_failure_has_zero_attempt_index_and_final_body_result() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, _, job) = setup().await?;
    let (id, response) = dispatch(
        &gw,
        "missing-model",
        OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1,
        false,
    )
    .await;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        logs.try_recv().is_err(),
        "final result waits for outer body EOS"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let wire: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(wire["error"]["request_id"], id);
    let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
        .await?
        .unwrap();
    assert_eq!(row.diagnostic.attempt_index, Some(0));
    assert_eq!(
        row.diagnostic.client_request_id.as_deref(),
        Some(id.as_str())
    );
    assert_eq!(row.diagnostic.attempt_outcome, "failed");
    assert_eq!(row.diagnostic.final_result.unwrap().attempt_count, 0);
    assert!(logs.try_recv().is_err());
    job.abort();
    Ok(())
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
        axum::serve(
            listener,
            Router::new().route("/v1/chat/completions", post(upstream)),
        )
        .await
        .unwrap();
    });
    Ok((dir, gw, logs, url, job))
}
async fn provider(gw: &Gateway, url: &str) -> anyhow::Result<String> {
    Ok(gw
        .admin()
        .create_provider(CreateProvider {
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
            api_key: "secret".into(),
            auth_mode: "apikey".into(),
            use_proxy: false,
            fast_mode: false,
        })
        .await?
        .id)
}
async fn route(
    gw: &Gateway,
    name: &str,
    provider: String,
    target: &str,
    targets: Vec<CreateModelBackend>,
) -> anyhow::Result<()> {
    gw.admin()
        .create_model(CreateModel {
            name: name.into(),
            balance: Some("priority".into()),
            target_provider: provider,
            target_model: target.into(),
            targets,
            enable_auth: Some(false),
            enable_payload: Some(false),
            force_max_reasoning: None,
            vision_shim: None,
        })
        .await?;
    Ok(())
}
async fn dispatch(
    gw: &Gateway,
    name: &str,
    ingress: ProtocolId,
    stream: bool,
) -> (String, Response) {
    let body = if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
        json!({"model":name,"messages":[{"role":"user","content":"hi"}],"max_tokens":20,"stream":stream})
    } else {
        json!({"model":name,"messages":[{"role":"user","content":"hi"}],"stream":stream})
    };
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
async fn native_errors_limits_observer_unknown_and_cancel_are_distinct() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, job) = setup().await?;
    let p = provider(&gw, &url).await?;
    for (model, expected) in [
        ("raw-error", "failed"),
        ("limited", "output_limited"),
        ("unknown", "unknown"),
        ("overflow", "unknown"),
        ("drop", "cancelled"),
    ] {
        route(&gw, model, p.clone(), model, vec![]).await?;
        let (id, response) =
            dispatch(&gw, model, OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["x-nyro-request-id"], id);
        if model == "drop" {
            drop(response);
        } else {
            axum::body::to_bytes(response.into_body(), 3 * 1024 * 1024).await?;
        }
        let row = tokio::time::timeout(Duration::from_secs(3), logs.recv())
            .await
            .unwrap_or_else(|_| panic!("missing log: {model}"))
            .unwrap();
        assert_eq!(
            row.diagnostic.attempt_outcome, expected,
            "{model}: {:?}",
            row.diagnostic
        );
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(row.diagnostic.attempt_index, Some(1));
        let final_result = row.diagnostic.final_result.as_ref().unwrap();
        assert_eq!(final_result.final_outcome, expected);
        assert_eq!(
            final_result.final_attempt_id.as_deref(),
            Some(row.diagnostic.log_id.as_str())
        );
        assert_eq!(final_result.attempt_count, 1);
        assert!(logs.try_recv().is_err());
    }
    job.abort();
    Ok(())
}
#[tokio::test]
async fn retry_keeps_two_attempts_and_only_final_result() -> anyhow::Result<()> {
    let (_dir, gw, mut logs, url, job) = setup().await?;
    let a = provider(&gw, &url).await?;
    let b = provider(&gw, &url).await?;
    let targets = vec![
        CreateModelBackend {
            provider_id: a.clone(),
            model: "http502".into(),
            weight: Some(100),
            priority: Some(1),
            is_fallback: Some(false),
        },
        CreateModelBackend {
            provider_id: b,
            model: "ok".into(),
            weight: Some(100),
            priority: Some(2),
            is_fallback: Some(true),
        },
    ];
    route(&gw, "retry", a, "http502", targets).await?;
    let (id, response) = dispatch(&gw, "retry", OPENAI_COMPATIBLE_CHAT_COMPLETIONS_V1, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    let mut rows = Vec::new();
    for _ in 0..2 {
        rows.push(
            tokio::time::timeout(Duration::from_secs(3), logs.recv())
                .await?
                .unwrap(),
        );
    }
    rows.sort_by_key(|row| row.diagnostic.attempt_index);
    assert_eq!(rows[0].diagnostic.attempt_outcome, "failed");
    assert_eq!(rows[0].upstream_status_code, Some(502));
    assert!(rows[0].diagnostic.final_result.is_none());
    assert_eq!(rows[1].diagnostic.attempt_outcome, "completed");
    assert_eq!(
        rows[1]
            .diagnostic
            .final_result
            .as_ref()
            .unwrap()
            .attempt_count,
        2
    );
    assert_ne!(rows[0].diagnostic.log_id, rows[1].diagnostic.log_id);
    for (n, row) in rows.iter().enumerate() {
        assert_eq!(
            row.diagnostic.client_request_id.as_deref(),
            Some(id.as_str())
        );
        assert_eq!(row.diagnostic.attempt_index, Some(n as i32 + 1));
    }
    assert!(logs.try_recv().is_err());
    job.abort();
    Ok(())
}
