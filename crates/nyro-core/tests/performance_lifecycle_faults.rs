//! Real dispatcher paths: original wire evidence survives conversion and forced streams.
use axum::{Json, Router, http::HeaderMap, response::IntoResponse, routing::post};
use nyro_core::{
    Gateway,
    config::GatewayConfig,
    db::models::{CreateModel, CreateProvider},
    protocol::{ids::*, ir::RawEnvelope},
    proxy::{context::RequestContext, dispatcher::dispatch_pipeline},
    storage::SqliteStorage,
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc, time::Duration};

async fn chat(Json(body): Json<Value>) -> axum::response::Response {
    let reason = if body["model"] == "limited" {
        "length"
    } else {
        "stop"
    };
    if body["stream"] == true {
        let sse = format!(
            "data: {{\"id\":\"r\",\"model\":\"test\",\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"hello\"}},\"finish_reason\":null}}]}}\n\ndata: {{\"id\":\"r\",\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{reason}\"}}],\"usage\":{{\"prompt_tokens\":5,\"completion_tokens\":10}}}}\n\ndata: [DONE]\n\n"
        );
        ([("content-type", "text/event-stream")], sse).into_response()
    } else {
        Json(json!({"id":"r","model":"test","choices":[{"index":0,"message":{"role":"assistant","content":"hello"},"finish_reason":reason}],"usage":{"prompt_tokens":5,"completion_tokens":10}})).into_response()
    }
}
async fn responses(Json(body): Json<Value>) -> axum::response::Response {
    assert_eq!(
        body["stream"], true,
        "forced streaming captured after last rewrite"
    );
    let sse = concat!(
        "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"test\",\"status\":\"in_progress\",\"output\":[]}}\n\n",
        "event: response.output_text.delta\ndata: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
        "event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"test\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"role\":\"assistant\",\"content\":[{\"type\":\"output_text\",\"text\":\"hello\"}]}],\"usage\":{\"input_tokens\":5,\"output_tokens\":10}}}\n\n"
    );
    ([("content-type", "text/event-stream")], sse).into_response()
}

#[tokio::test]
async fn converted_and_forced_stream_attempts_wait_for_body_eos() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let config = GatewayConfig {
        data_dir: dir.path().into(),
        config_poll_interval: Duration::ZERO,
        ..Default::default()
    };
    let storage = Arc::new(SqliteStorage::from_config(&config).await?);
    let (gw, mut logs) = Gateway::from_storage(config, storage).await?;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new()
                .route("/v1/chat/completions", post(chat))
                .route("/v1/responses", post(responses)),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
    });
    for (name, protocol, ingress, stream) in [
        (
            "compat-buffered",
            "openai-compatible",
            ANTHROPIC_MESSAGES_2023_06_01,
            false,
        ),
        (
            "compat-streamed",
            "openai-compatible",
            ANTHROPIC_MESSAGES_2023_06_01,
            true,
        ),
        (
            "forced-stream",
            "openai-responses",
            OPENAI_RESPONSES_V1,
            false,
        ),
        (
            "native-ir",
            "openai-compatible",
            GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA,
            true,
        ),
    ] {
        let provider = gw
            .admin()
            .create_provider(CreateProvider {
                name: name.into(),
                vendor: None,
                protocol: protocol.into(),
                base_url: format!("http://{address}/v1"),
                protocol_mode: "fixed".into(),
                protocol_endpoints: vec![],
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
        gw.admin()
            .create_model(CreateModel {
                name: name.into(),
                balance: None,
                target_provider: provider.id,
                target_model: "test".into(),
                targets: vec![],
                enable_auth: Some(false),
                enable_payload: None,
                force_max_reasoning: Some(true),
                vision_shim: None,
            })
            .await?;
        let body = if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
            json!({"model":name,"max_tokens":100,"messages":[{"role":"user","content":"hi"}],"stream":stream,"output_config":{"effort":"low"}})
        } else if ingress == GOOGLE_GEMINI_GENERATE_CONTENT_V1BETA {
            json!({"model":name,"contents":[{"role":"user","parts":[{"text":"hi"}]}],"generationConfig":{"thinkingConfig":{"thinkingLevel":"low"}}})
        } else {
            json!({"model":name,"input":"hi","stream":stream,"reasoning":{"effort":"low"}})
        };
        let mut request = ingress
            .handler()
            .make_request_decoder()
            .decode_request(body.clone())?;
        request.stream.enabled = stream;
        request.model = name.into();
        let envelope = RawEnvelope::new(
            Some(body),
            HashMap::new(),
            "POST",
            if ingress == ANTHROPIC_MESSAGES_2023_06_01 {
                "/v1/messages"
            } else {
                "/v1/responses"
            },
        );
        let response = dispatch_pipeline(
            gw.clone(),
            HeaderMap::new(),
            envelope,
            request,
            ingress,
            RequestContext::new(ingress, Duration::from_secs(5)),
        )
        .await;
        assert!(
            response.status().is_success(),
            "{name}: {}",
            response.status()
        );
        assert!(
            logs.try_recv().is_err(),
            "{name}: enqueue is not final delivery"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        assert!(!bytes.is_empty());
        let entry = tokio::time::timeout(Duration::from_secs(3), logs.recv())
            .await?
            .unwrap();
        assert_eq!(
            entry.performance.completion, "completed",
            "{name}: {:?}",
            entry.performance
        );
        assert_eq!(
            entry.performance.response_mode,
            if stream || name == "forced-stream" {
                "stream"
            } else {
                "buffered"
            }
        );
        assert!(
            entry.performance.effort_tier.is_some(),
            "{name}: {:?}",
            entry.performance
        );
        assert_ne!(
            entry.performance.effort_tier.as_deref(),
            Some("low"),
            "final forced max rewrite must win"
        );
        assert!(entry.performance.upstream_duration_ms.is_some());
        assert!(entry.performance.completed_at.is_some());
        assert!(logs.try_recv().is_err());
    }
    let _ = stop_tx.send(());
    server.await??;
    Ok(())
}
